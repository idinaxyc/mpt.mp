#!/usr/bin/env python3
"""Локальные тесты lan-sync: два «ядра» (имитация плеера) + два процесса плагина на localhost.

    python3 tests/test_sync.py          # из папки plugins/lan-sync

Никакой сети, кроме 127.0.0.1; вместо автопоиска — адреса вручную (peers).
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.join(os.path.dirname(HERE), "main.py")


class FakeCore:
    """Минимальное ядро: библиотека, очередь, плеер, тема, плейлисты и протокол плагинов."""

    def __init__(self, name, root, port, peers, token="secret-token"):
        self.name, self.root = name, root
        self.music = os.path.join(root, "Music")
        os.makedirs(self.music, exist_ok=True)
        self.folders = [self.music]
        self.lock = threading.RLock()
        self.tracks, self.playlists = {}, {}
        self.queue, self.index = [], 0
        self.playing, self.pos, self.pos_at = False, 0, time.monotonic()
        self.volume = 0.5
        self.theme = {"name": "Everforest", "colors": {"accent": "#a7c080", "background": "#2d353b"}}
        self.requests = 0
        self.scan()
        data = os.path.join(root, "data")
        os.makedirs(data, exist_ok=True)
        self.cfg = {"token": token, "device_name": name, "port": port, "peers": peers,
                    "sync_playback": True, "sync_volume": True, "sync_theme": True, "sync_playlists": True,
                    "share_files": True, "download_missing": True, "download_dir": ""}
        env = dict(os.environ, MPT_DATA_DIR=data)
        self.proc = subprocess.Popen([sys.executable, PLUGIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True, bufsize=1, env=env)
        self.send({"type": "hello", "api": 1, "core_version": "t", "plugin_id": "lan-sync",
                   "data_dir": data, "config": self.cfg, "permissions": []})
        self.logs, self.status = [], ""
        threading.Thread(target=self.read, daemon=True).start()
        threading.Thread(target=lambda: [self.logs.append("stderr: " + l) for l in self.proc.stderr], daemon=True).start()

    # -- протокол
    def send(self, obj):
        with self.lock:
            self.proc.stdin.write(json.dumps(obj) + "\n")
            self.proc.stdin.flush()

    def event(self, name, data=None):
        self.send({"type": "event", "name": name, "data": data or {}})

    def read(self):
        for line in self.proc.stdout:
            msg = json.loads(line)
            if msg["type"] == "request":
                threading.Thread(target=self.answer, args=(msg,), daemon=True).start()
            elif msg["type"] == "status":
                self.status = msg["text"]
            elif msg["type"] == "log":
                self.logs.append(msg["msg"])

    def answer(self, msg):
        self.requests += 1
        try:
            res = self.call(msg["method"], msg.get("params", {}))
            self.send({"type": "response", "id": msg["id"], "ok": True, "result": res})
        except KeyError as e:
            self.send({"type": "response", "id": msg["id"], "ok": False, "error": {"code": "not_found", "message": str(e)}})

    # -- имитация игрока
    def cur_pos(self):
        with self.lock:
            return int(self.pos + ((time.monotonic() - self.pos_at) * 1000 if self.playing else 0))

    def set_pos(self, p, playing=None):
        with self.lock:
            self.pos, self.pos_at = int(p), time.monotonic()
            if playing is not None:
                self.playing = playing

    def state(self):
        return {"playing": self.playing, "position_ms": self.cur_pos(), "duration_ms": 600000,
                "volume": self.volume, "repeat": "off", "shuffle": False,
                "queue_index": self.index, "queue_len": len(self.queue)}

    def call(self, m, p):
        with self.lock:
            if m == "player.get_state":
                return self.state()
            if m == "player.get_queue":
                return {"track_ids": list(self.queue), "index": self.index}
            if m == "player.play_queue":
                self.queue = [i for i in p["track_ids"] if i in self.tracks]
                self.index = p.get("start", 0)
                self.set_pos(0, True)
                return {}
            if m == "player.seek":
                self.set_pos(p["position_ms"])
                return {}
            if m == "player.play":
                self.set_pos(self.cur_pos(), True)
                return {}
            if m == "player.pause":
                self.set_pos(self.cur_pos(), False)
                return {}
            if m == "player.set_volume":
                self.volume = p["volume"]
                return {}
            if m == "library.get_tracks":
                allt = list(self.tracks.values())
                return {"total": len(allt), "tracks": allt[p["offset"]:p["offset"] + p["limit"]]}
            if m == "library.get_playlists":
                return {"playlists": list(self.playlists.values())}
            if m == "library.create_playlist":
                pid = f"pl{len(self.playlists) + 1}"
                self.playlists[pid] = {"id": pid, "name": p["name"], "track_ids": [i for i in p["track_ids"] if i in self.tracks]}
                threading.Timer(0.05, self.event, ("library_changed",)).start()
                return {"id": pid}
            if m == "library.add_to_playlist":
                pl = self.playlists[p["id"]]
                pl["track_ids"] += [i for i in p["track_ids"] if i not in pl["track_ids"]]
                threading.Timer(0.05, self.event, ("library_changed",)).start()
                return {}
            if m == "library.get_folders":
                return {"folders": self.folders}
            if m == "library.add_folder":
                if p["path"] not in self.folders:
                    self.folders.append(p["path"])
                return {}
            if m == "library.rescan":
                self.scan()
                threading.Timer(0.05, self.event, ("library_changed",)).start()
                return {}
            if m == "settings.get_theme":
                return json.loads(json.dumps(self.theme))
            if m == "settings.set_theme":
                self.theme = {"name": p.get("name") or self.theme["name"], "colors": {**self.theme["colors"], **p["colors"]}}
                return {}
        raise KeyError(m)

    def scan(self):
        with self.lock:
            found = {}
            for folder in self.folders:
                for dp, _, files in os.walk(folder):
                    for f in sorted(files):
                        if f.endswith(".part"):
                            continue
                        path = os.path.join(dp, f)
                        tid = f"{self.name}-{abs(hash(path)) % 10**8}"
                        found[tid] = {"id": tid, "path": path, "title": f, "artist": "", "album": "",
                                      "duration_ms": 1000, "size": os.path.getsize(path)}
            self.tracks = found

    def add_file(self, name, content):
        with open(os.path.join(self.music, name), "wb") as f:
            f.write(content)
        self.scan()
        self.event("library_changed")

    def id_of(self, name):
        for t in self.tracks.values():
            if os.path.basename(t["path"]) == name or os.path.basename(t["path"]).lower() == name.lower():
                return t["id"]

    # -- действия «пользователя» (как в UI): меняют состояние и шлют события
    def user_play(self, names, start=0):
        self.call("player.play_queue", {"track_ids": [self.id_of(n) for n in names], "start": start})
        self.event("queue_changed")
        self.event("state")

    def user_seek(self, ms):
        self.call("player.seek", {"position_ms": ms})
        self.event("state")

    def user_pause(self):
        self.call("player.pause", {})
        self.event("state")

    def user_volume(self, v):
        self.volume = v
        self.event("state")

    def user_theme(self, accent):
        with self.lock:
            self.theme["colors"]["accent"] = accent

    def stop(self):
        try:
            self.send({"type": "shutdown"})
            self.proc.wait(5)
        except Exception:
            self.proc.kill()


def wait(cond, timeout=15, msg=""):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(0.1)
    raise AssertionError("не дождались: " + msg)


def main():
    tmp = tempfile.mkdtemp(prefix="lansync-")
    pa, pb = 47931, 47932
    A = B = None
    try:
        A = FakeCore("alpha", os.path.join(tmp, "a"), pa, f"127.0.0.1:{pb}")
        B = FakeCore("bravo", os.path.join(tmp, "b"), pb, f"127.0.0.1:{pa}")
        for name, size in (("One.mp3", 3000), ("Two.mp3", 4000), ("Three.mp3", 5000)):
            data = (name.encode() * 5000)[:size] + os.urandom(0)
            A.add_file(name, data)
            B.add_file(name, data)
        A.add_file("OnlyA.flac", os.urandom(300_000))
        time.sleep(1.5)
        for c in (A, B):
            assert c.status and "token" not in c.status, c.status

        print("1. громкость A → B")
        A.user_volume(0.8)
        wait(lambda: abs(B.volume - 0.8) < 0.01, msg="volume")
        time.sleep(1.5)
        assert abs(A.volume - 0.8) < 0.01, "эхо вернулось"

        print("2. воспроизведение A → B (очередь, трек, позиция)")
        A.user_play(["One.mp3", "Two.mp3", "Three.mp3"], start=1)
        A.user_seek(42000)
        wait(lambda: B.queue and B.tracks[B.queue[B.index]]["title"] == "Two.mp3" and B.playing, msg="queue")
        wait(lambda: abs(B.cur_pos() - A.cur_pos()) < 2500, msg="position")
        assert len(B.queue) == 3

        time.sleep(2)  # дать обмену затихнуть: пауза пользователя не должна гоняться с применением чужой перемотки
        print("3. пауза на B → A (главный сменился)")
        B.user_pause()
        wait(lambda: not A.playing, msg="pause")
        assert "bravo" in A.status or "this device" in B.status, (A.status, B.status)
        time.sleep(1.5)
        assert not B.playing and not A.playing

        print("4. перемотка на A → B")
        A.user_seek(100000)
        wait(lambda: abs(B.cur_pos() - 100000) < 2500, msg="seek")

        print("5. одновременное изменение громкости: решает метка, оба сходятся к одному значению")
        A.user_volume(0.2)
        B.user_volume(0.9)
        wait(lambda: abs(A.volume - B.volume) < 0.01, 20, "convergence")
        print("   итог:", A.volume, B.volume)

        print("6. тема A → B")
        A.user_theme("#ff0000")
        wait(lambda: B.theme["colors"]["accent"] == "#ff0000", 15, "theme")

        print("7. плейлисты: объединение")
        a1, b1 = A.id_of("One.mp3"), B.id_of("Two.mp3")
        A.call("library.create_playlist", {"name": "Mix", "track_ids": [a1]})
        A.event("library_changed")
        B.call("library.create_playlist", {"name": "mix", "track_ids": [b1]})
        B.event("library_changed")
        def merged(c):
            pls = list(c.playlists.values())
            if len(pls) != 1:
                return False
            titles = sorted(c.tracks[i]["title"] for i in pls[0]["track_ids"])
            return titles == ["One.mp3", "Two.mp3"]
        wait(lambda: merged(A) and merged(B), 30, "playlist union")

        print("8. файлы: OnlyA.flac уходит на B, потом можно играть")
        wait(lambda: B.id_of("OnlyA.flac"), 40, "download")
        src = open(A.tracks[A.id_of("OnlyA.flac")]["path"], "rb").read()
        dst = open(B.tracks[B.id_of("OnlyA.flac")]["path"], "rb").read()
        assert src == dst, "файл повреждён"
        assert os.path.join("Music", "mpt-sync") in B.tracks[B.id_of("OnlyA.flac")]["path"]
        A.user_play(["OnlyA.flac"])
        wait(lambda: B.queue and B.tracks[B.queue[B.index]]["title"] == "OnlyA.flac", 15, "play downloaded")

        print("9. чужой токен отвергается")
        C = FakeCore("charlie", os.path.join(tmp, "c"), 47933, f"127.0.0.1:{pa}", token="another-token")
        try:
            C.user_volume(0.1)
            time.sleep(4)
            assert abs(A.volume - 0.1) > 0.01 and abs(B.volume - 0.1) > 0.01, "чужой токен принят!"
            assert not any("secret-token" in l or "another-token" in l for l in A.logs + B.logs + C.logs), "токен в логах"
        finally:
            C.stop()
        print("запросов к ядру:", A.requests, B.requests)
        print("OK")
    except AssertionError:
        for c in (A, B):
            if c:
                print(c.name, "status:", c.status, "logs:", c.logs[-10:])
        raise
    finally:
        for c in (A, B):
            if c:
                c.stop()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
