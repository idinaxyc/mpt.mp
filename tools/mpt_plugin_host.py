#!/usr/bin/env python3
"""Тестовый хост протокола mpt-plugin/1 — проверяет плагин без запуска mpt.mp.

    python3 tools/mpt_plugin_host.py plugins/now-playing-file
    python3 tools/mpt_plugin_host.py <папка-плагина> --keep-data
    python3 tools/mpt_plugin_host.py plugins/sleep-timer --set minutes=0

Что делает: читает plugin.json, запускает плагин как это сделает ядро,
отправляет hello, ждёт ready, прогоняет сценарий (смена трека, пауза,
config_changed, команды из манифеста), отвечает на запросы заглушками и
в конце шлёт shutdown. Выходит с кодом 1, если плагин нарушил протокол.
Только стандартная библиотека Python 3.8+.
"""
import argparse
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

API = 1
ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{1,31}$")
SETTING_TYPES = {"string", "bool", "int", "choice", "path"}
PERMISSIONS = {"player.read", "player.control", "library.read", "library.write",
               "settings.read", "settings.write"}

METHOD_PERMISSION = {
    "player.get_state": "player.read",
    "player.get_queue": "player.read",
    "player.play": "player.control",
    "player.pause": "player.control",
    "player.toggle": "player.control",
    "player.next": "player.control",
    "player.prev": "player.control",
    "player.seek": "player.control",
    "player.set_volume": "player.control",
    "player.play_queue": "player.control",
    "library.get_tracks": "library.read",
    "library.get_track": "library.read",
    "library.get_playlists": "library.read",
    "library.create_playlist": "library.write",
    "library.add_to_playlist": "library.write",
    "library.rescan": "library.write",
    "library.get_folders": "library.read",
    "library.add_folder": "library.write",
    "settings.get_theme": "settings.read",
    "settings.set_theme": "settings.write",
}
EVENT_PERMISSION = {
    "state": "player.read",
    "track_changed": "player.read",
    "queue_changed": "player.read",
    "library_changed": "library.read",
    "config_changed": None,  # доступно всем
}

TRACKS = [
    {"id": "aaaa000000000001", "path": "/home/user/Music/one.flac", "title": "One",
     "artist": "Artist A", "album": "Album", "duration_ms": 180000, "size": 1000},
    {"id": "aaaa000000000002", "path": "/home/user/Music/two.mp3", "title": "Two",
     "artist": "", "album": "", "duration_ms": 200000, "size": 2000},
]


class Host:
    def __init__(self, plugin_dir, keep_data, overrides=None):
        self.dir = os.path.abspath(plugin_dir)
        self.overrides = overrides or {}
        self.problems = []
        self.q = queue.Queue()
        self.proc = None
        self.keep_data = keep_data
        self.next_event_id = 0

    # ---- вывод ----------------------------------------------------------
    def note(self, text):
        print(f"  {text}")

    def problem(self, text):
        self.problems.append(text)
        print(f"  ✗ {text}")

    # ---- манифест -------------------------------------------------------
    def load_manifest(self):
        path = os.path.join(self.dir, "plugin.json")
        try:
            with open(path, encoding="utf-8") as f:
                m = json.load(f)
        except Exception as e:  # noqa: BLE001
            self.problem(f"plugin.json не читается: {e}")
            return None
        for key in ("id", "name", "version", "api", "run"):
            if key not in m:
                self.problem(f"в plugin.json нет поля «{key}»")
        if not ID_RE.match(str(m.get("id", ""))):
            self.problem("id должен быть вида ^[a-z0-9][a-z0-9-]{1,31}$")
        if m.get("api") != API:
            self.problem(f"api = {m.get('api')}, хост понимает только {API}")
        cmd = (m.get("run") or {}).get("command")
        if not (isinstance(cmd, list) and cmd and all(isinstance(c, str) for c in cmd)):
            self.problem("run.command должен быть непустым списком строк")
        for perm in m.get("permissions", []):
            if perm not in PERMISSIONS:
                self.problem(f"неизвестное право «{perm}»")
        for s in m.get("settings", []):
            if s.get("type") not in SETTING_TYPES or "key" not in s:
                self.problem(f"некорректная настройка: {s}")
        return m

    # ---- обмен ----------------------------------------------------------
    def reader(self, stream):
        for raw in stream:
            self.q.put(raw)
        self.q.put(None)

    def send(self, obj):
        line = json.dumps(obj, ensure_ascii=False)
        try:
            self.proc.stdin.write(line + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError):
            self.problem("плагин закрыл stdin раньше времени")

    def handle(self, raw, perms):
        """Разбирает одну строку плагина. Возвращает сообщение или None."""
        raw = raw.rstrip("\n")
        if not raw.strip():
            return None
        try:
            msg = json.loads(raw)
            if not isinstance(msg, dict) or "type" not in msg:
                raise ValueError("нет поля type")
        except ValueError as e:
            self.problem(f"в stdout не JSON-сообщение ({e}): {raw[:80]!r}")
            return None
        kind = msg["type"]
        if kind == "request":
            self.answer(msg, perms)
        elif kind == "status":
            self.note(f"status[{msg.get('level', 'info')}]: {msg.get('text')}")
        elif kind == "log":
            self.note(f"log[{msg.get('level', 'info')}]: {msg.get('msg')}")
        elif kind == "ready":
            pass
        else:
            self.note(f"(неизвестный тип «{kind}» — ядро его проигнорирует)")
        return msg

    def answer(self, msg, perms):
        rid, method = msg.get("id"), msg.get("method")
        self.note(f"request {rid}: {method} {json.dumps(msg.get('params', {}), ensure_ascii=False)}")
        if rid is None:
            self.problem("request без id")
            return
        need = METHOD_PERMISSION.get(method)
        if need is None:
            err = {"code": "unknown_method", "message": f"нет метода {method}"}
        elif need not in perms:
            err = {"code": "permission_denied", "message": f"нужно право {need}"}
        else:
            err = None
        if err:
            self.send({"type": "response", "id": rid, "ok": False, "error": err})
            return
        result = {}
        if method == "player.get_state":
            result = {"playing": True, "position_ms": 12000, "duration_ms": 180000,
                      "volume": 0.8, "repeat": "off", "shuffle": False,
                      "queue_index": 0, "queue_len": len(TRACKS)}
        elif method == "player.get_queue":
            result = {"track_ids": [t["id"] for t in TRACKS], "index": 0}
        elif method == "library.get_tracks":
            result = {"total": len(TRACKS), "tracks": TRACKS}
        elif method == "library.get_playlists":
            result = {"playlists": []}
        elif method == "library.get_folders":
            result = {"folders": ["/home/user/Music"]}
        elif method == "settings.get_theme":
            result = {"name": "mock", "colors": {"accent": "#a7c080", "background": "#2d353b"}}
        self.send({"type": "response", "id": rid, "ok": True, "result": result})

    def pump(self, seconds, perms):
        end = time.time() + seconds
        while True:
            left = end - time.time()
            if left <= 0:
                return True
            try:
                raw = self.q.get(timeout=left)
            except queue.Empty:
                return True
            if raw is None:
                return False  # плагин закрыл stdout
            self.handle(raw, perms)

    def event(self, name, data, subscribed, perms):
        need = EVENT_PERMISSION.get(name, "?")
        if (name in subscribed or name == "config_changed") and (need is None or need in perms):
            self.send({"type": "event", "name": name, "data": data})
            self.note(f"→ event {name}")

    # ---- сценарий -------------------------------------------------------
    def run(self):
        print(f"Плагин: {self.dir}")
        manifest = self.load_manifest()
        if manifest is None or self.problems:
            return 1
        perms = set(manifest.get("permissions", []))
        data_dir = tempfile.mkdtemp(prefix="mpt-plugin-data-")
        config = {s["key"]: s.get("default") for s in manifest.get("settings", []) if "key" in s}
        # Чтобы не писать в домашнюю папку при проверке, подменим пути-настройки.
        for s in manifest.get("settings", []):
            if s.get("type") == "path" and isinstance(config.get(s["key"]), str):
                config[s["key"]] = os.path.join(data_dir, os.path.basename(config[s["key"]]) or "out")
        config.update(self.overrides)
        env = dict(os.environ, MPT_API=str(API), MPT_PLUGIN_ID=manifest["id"],
                   MPT_PLUGIN_DIR=self.dir, MPT_DATA_DIR=data_dir)
        try:
            self.proc = subprocess.Popen(
                manifest["run"]["command"], cwd=self.dir, env=env, text=True, bufsize=1,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        except OSError as e:
            self.problem(f"не удалось запустить {manifest['run']['command']}: {e}")
            return 1
        threading.Thread(target=self.reader, args=(self.proc.stdout,), daemon=True).start()
        threading.Thread(
            target=lambda: [print(f"  stderr: {l.rstrip()}") for l in self.proc.stderr],
            daemon=True).start()

        self.send({"type": "hello", "api": API, "core_version": "test-host",
                   "plugin_id": manifest["id"], "data_dir": data_dir,
                   "config": config, "permissions": sorted(perms)})
        self.note("→ hello")

        # Ждём ready не дольше 5 секунд.
        subscribed = set()
        got_ready = False
        deadline = time.time() + 5
        while time.time() < deadline and not got_ready:
            try:
                raw = self.q.get(timeout=max(0.01, deadline - time.time()))
            except queue.Empty:
                break
            if raw is None:
                self.problem("плагин завершился, не отправив ready")
                return 1
            msg = self.handle(raw, perms)
            if msg is None:
                continue
            if msg.get("type") == "ready":
                subscribed = set(msg.get("subscribe", []))
                got_ready = True
            else:
                self.problem(f"до ready плагин прислал «{msg.get('type')}»: первым должно идти ready")
        if not got_ready:
            self.problem("ready не получен за 5 секунд")
            self.proc.kill()
            return 1
        self.note(f"ready, подписки: {sorted(subscribed)}")
        for name in subscribed:
            need = EVENT_PERMISSION.get(name, "?")
            if need == "?":
                self.note(f"(неизвестное событие «{name}» — будет проигнорировано)")
            elif need is not None and need not in perms:
                self.problem(f"подписка на «{name}» без права {need}: события не придут")

        state = {"playing": True, "position_ms": 0, "duration_ms": 180000, "volume": 0.8,
                 "repeat": "off", "shuffle": False, "queue_index": 0, "queue_len": 2}
        self.event("track_changed", {"track": TRACKS[0], "index": 0}, subscribed, perms)
        self.event("state", state, subscribed, perms)
        self.pump(0.4, perms)
        self.event("state", dict(state, playing=False, position_ms=5000), subscribed, perms)
        self.pump(0.4, perms)
        self.event("track_changed", {"track": TRACKS[1], "index": 1}, subscribed, perms)
        self.event("state", dict(state, queue_index=1), subscribed, perms)
        self.pump(0.4, perms)
        self.event("config_changed", {"config": config}, subscribed, perms)
        for cmd in manifest.get("commands", []):
            self.send({"type": "command", "name": cmd["name"], "args": {}})
            self.note(f"→ command {cmd['name']}")
        self.pump(0.5, perms)
        # Неизвестное событие плагин обязан пережить.
        self.send({"type": "event", "name": "from_the_future", "data": {"x": 1}})
        self.pump(0.2, perms)
        if self.proc.poll() is not None:
            self.problem(f"плагин упал во время сценария (код {self.proc.returncode})")
            return 1

        self.send({"type": "shutdown"})
        self.note("→ shutdown")
        try:
            self.proc.wait(timeout=3)
            self.note(f"плагин завершился, код {self.proc.returncode}")
            if self.proc.returncode not in (0, None):
                self.problem(f"код выхода {self.proc.returncode} вместо 0")
        except subprocess.TimeoutExpired:
            self.problem("плагин не завершился за 3 с после shutdown")
            self.proc.kill()
        while True:  # дочитываем остаток
            try:
                raw = self.q.get(timeout=0.2)
            except queue.Empty:
                break
            if raw:
                self.handle(raw, perms)
        if self.keep_data:
            self.note(f"данные плагина: {data_dir}")
        else:
            shutil.rmtree(data_dir, ignore_errors=True)
        return 1 if self.problems else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("plugin_dir")
    ap.add_argument("--keep-data", action="store_true", help="не удалять папку с данными плагина")
    ap.add_argument("--set", action="append", default=[], metavar="КЛЮЧ=ЗНАЧЕНИЕ",
                    help="переопределить настройку плагина (значение — JSON или строка)")
    args = ap.parse_args()
    overrides = {}
    for item in args.set:
        key, _, raw = item.partition("=")
        try:
            overrides[key] = json.loads(raw)
        except ValueError:
            overrides[key] = raw
    code = Host(args.plugin_dir, args.keep_data, overrides).run()
    print("\nИТОГ:", "всё в порядке" if code == 0 else "есть нарушения протокола")
    sys.exit(code)


if __name__ == "__main__":
    main()
