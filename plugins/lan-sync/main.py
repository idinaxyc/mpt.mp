#!/usr/bin/env python3
"""mpt.mp · lan-sync — синхронизация нескольких плееров в локальной сети.

Что синхронизируется
  * воспроизведение: очередь, текущий трек, позиция, пауза/play (один «регистр» playback);
  * громкость, тема (регистры volume, theme);
  * плейлисты (объединение по имени, без удалений);
  * файлы треков, которых нет на устройстве (скачиваются с другого устройства).

Кто главный
  Главного устройства нет «навсегда»: им считается то, что сделало последнее изменение
  параметра. У каждого изменения есть метка гибридных логических часов (HLC):
  max(время, самая большая метка из виденных) + 1. Устройство, уже принявшее чужое
  изменение, не может «проиграть» его одновременному старому — его новая метка больше.
  Совсем одновременные изменения (метки равны или разошлись из-за часов) решает
  детерминированно: сначала метка, затем id устройства. Все устройства приходят
  к одному результату. Подробности — в README.md.

Только стандартная библиотека Python 3.8+. Токен нигде не печатается и не передаётся.
"""
import hashlib
import hmac
import http.client
import http.server
import itertools
import json
import os
import queue
import re
import secrets
import socket
import socketserver
import sys
import threading
import time
import urllib.parse

MAX_QUEUE = 3000  # длиннее очередь не синхронизируем
MAX_BODY = 1 << 20  # 1 МиБ на запрос
POS_TOL_MS = 2500  # отличие позиции от ожидаемой, после которого это «перемотка»
SKEW_S = 90  # допустимая разница часов для подписи запроса
BEACON_S = 5
SYNC_S = 10
TAG_LEN = 32
HEX_COLOR = re.compile(r"^#[0-9a-fA-F]{6}$")
BAD_CHARS = re.compile(r'[<>:"/\\|?*\x00-\x1f]')

# ---------------------------------------------------------------- протокол ядра

write_lock = threading.Lock()
ids = itertools.count(1)
waiters = {}
rate_lock = threading.Lock()
last_req = [0.0]
running = threading.Event()
running.set()


def send(obj):
    with write_lock:
        sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def log(msg, level="info"):
    send({"type": "log", "level": level, "msg": str(msg)})


class RpcError(Exception):
    def __init__(self, code, message):
        super().__init__(f"{code}: {message}")
        self.code = code


def rpc(method, params=None, timeout=10):
    """Запрос к ядру из любого потока (кроме читающего stdin). Не чаще ~25 в секунду."""
    with rate_lock:
        wait = last_req[0] + 0.04 - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        last_req[0] = time.monotonic()
    rid = next(ids)
    ev, box = threading.Event(), {}
    waiters[rid] = (ev, box)
    send({"type": "request", "id": rid, "method": method, "params": params or {}})
    if not ev.wait(timeout):
        waiters.pop(rid, None)
        raise RpcError("timeout", method)
    if not box.get("ok"):
        err = box.get("error") or {}
        raise RpcError(err.get("code", "internal"), err.get("message", ""))
    return box.get("result") or {}


# ---------------------------------------------------------------- вспомогательное


def safe_name(path):
    """Имя файла, безопасное на любой ОС. Идемпотентно: safe_name(safe_name(x)) == safe_name(x)."""
    name = os.path.basename(str(path).replace("\\", "/"))
    name = BAD_CHARS.sub("_", name).strip(" .")
    return name[:200] or "track"


def track_key(path, size):
    return f"{safe_name(path).lower()}|{int(size)}"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def digest_of(obj):
    return hashlib.sha1(json.dumps(obj, sort_keys=True, ensure_ascii=False).encode()).hexdigest()[:16]


def order(reg):
    return (reg["ts"], reg["dev"])


def valid_value(key, v):
    """Проверка значения регистра из сети — ему нельзя верить."""
    try:
        if key == "playback":
            keys = v["keys"]
            return (
                isinstance(keys, list)
                and 0 < len(keys) <= MAX_QUEUE
                and all(isinstance(k, str) and 0 < len(k) < 300 for k in keys)
                and isinstance(v["index"], int)
                and 0 <= v["index"] < len(keys)
                and isinstance(v["playing"], bool)
                and isinstance(v["pos"], int)
                and v["pos"] >= 0
            )
        if key == "volume":
            return isinstance(v, (int, float)) and not isinstance(v, bool) and 0 <= v <= 1
        if key == "theme":
            colors = v["colors"]
            return (
                isinstance(colors, dict)
                and 0 < len(colors) <= 32
                and all(isinstance(k, str) and isinstance(c, str) and HEX_COLOR.match(c) for k, c in colors.items())
                and isinstance(v.get("name", ""), str)
                and len(v.get("name", "")) <= 60
            )
    except (KeyError, TypeError):
        return False
    return False


# ---------------------------------------------------------------- плагин


class Plugin:
    def __init__(self, hello):
        self.cfg = dict(hello.get("config", {}))
        self.data_dir = hello.get("data_dir") or os.environ.get("MPT_DATA_DIR") or "."
        os.makedirs(self.data_dir, exist_ok=True)
        self.state_path = os.path.join(self.data_dir, "state.json")
        self.lock = threading.RLock()
        self.regs = {}
        self.hlc_max = 0
        self.dev = None
        self.load_state()
        self.name = self.cfg_str("device_name") or socket.gethostname()[:40]
        self.peers = {}  # (host, port) -> dict
        self.jobs = queue.Queue()
        self.push_q = queue.Queue()
        self.dl_q = queue.PriorityQueue()
        self.dl_seq = itertools.count()
        self.dl_active = set()
        self.dl_failed = {}
        self.upload_slots = threading.Semaphore(2)
        self.seen_sigs = {}
        self.key2id, self.id2key, self.key2path = {}, {}, {}
        self.last_reindex = 0.0
        self.local_pl = {}
        self.wanted_pl = {}  # name_lower -> {"name":..., "keys":[...]}
        self.lib_digest = self.pl_digest = ""
        self.last_obs = None
        self.last_theme = None
        self.pending_playback = False
        self.need_rescan = False
        self.last_status = None
        self.http_port = 0
        self.server = None

    # ---- настройки
    def cfg_str(self, key):
        v = self.cfg.get(key, "")
        return v.strip() if isinstance(v, str) else ""

    def flag(self, key):
        return bool(self.cfg.get(key, True))

    @property
    def token(self):
        t = self.cfg_str("token")
        return t.encode() if len(t) >= 8 else None

    # ---- сохранение состояния
    def load_state(self):
        try:
            with open(self.state_path, encoding="utf-8") as f:
                d = json.load(f)
            self.dev = d.get("dev")
            for k, r in (d.get("regs") or {}).items():
                if valid_value(k, r.get("v")) and isinstance(r.get("ts"), int):
                    r["at"] = time.monotonic()
                    self.regs[k] = r
                    self.hlc_max = max(self.hlc_max, r["ts"])
        except (OSError, ValueError, AttributeError):
            pass
        if not isinstance(self.dev, str) or not re.fullmatch(r"[0-9a-f]{12}", self.dev):
            self.dev = secrets.token_hex(6)
            self.save_state()

    def save_state(self):
        with self.lock:
            data = {
                "dev": self.dev,
                "regs": {k: {f: r[f] for f in ("v", "ts", "dev", "name")} for k, r in self.regs.items()},
            }
        tmp = self.state_path + ".tmp"
        try:
            with open(tmp, "w", encoding="utf-8") as f:
                json.dump(data, f, ensure_ascii=False)
            os.replace(tmp, self.state_path)
        except OSError as e:
            log(f"could not save state: {e}", "warn")

    # ---- часы и регистры
    def stamp(self):
        with self.lock:
            self.hlc_max = max(int(time.time() * 1000), self.hlc_max + 1)
            return self.hlc_max

    def publish(self, key, value, at=None):
        reg = {"v": value, "ts": self.stamp(), "dev": self.dev, "name": self.name, "at": at or time.monotonic()}
        with self.lock:
            self.regs[key] = reg
        self.save_state()
        self.push_q.put(key)
        self.update_status()

    def wire(self, key):
        with self.lock:
            r = self.regs.get(key)
            if not r:
                return None
            return {
                "v": r["v"],
                "ts": r["ts"],
                "dev": r["dev"],
                "name": r["name"],
                "age": int((time.monotonic() - r["at"]) * 1000),
            }

    def merge(self, key, w):
        """Принимает регистр из сети. True — он новее нашего и принят."""
        try:
            ok = (
                isinstance(w, dict)
                and isinstance(w["ts"], int)
                and isinstance(w["dev"], str)
                and re.fullmatch(r"[0-9a-f]{12}", w["dev"])
                and valid_value(key, w["v"])
            )
        except KeyError:
            ok = False
        if not ok or w["dev"] == self.dev:
            return False
        age = w.get("age", 0)
        age = age if isinstance(age, int) and 0 <= age < 3600_000 else 0
        reg = {
            "v": w["v"],
            "ts": w["ts"],
            "dev": w["dev"],
            "name": str(w.get("name", ""))[:40],
            "at": time.monotonic() - age / 1000,
        }
        with self.lock:
            self.hlc_max = max(self.hlc_max, reg["ts"])
            cur = self.regs.get(key)
            if cur and order(cur) >= order(reg):
                return False
            self.regs[key] = reg
        self.save_state()
        self.jobs.put(("apply", key))
        self.update_status()
        return True

    # ---- статус
    def live_peers(self):
        now = time.monotonic()
        with self.lock:
            return [p for p in self.peers.values() if now - p["seen"] < 60]

    def update_status(self):
        with self.lock:
            head = max(self.regs.values(), key=order, default=None)
        if self.token is None:
            text, level = "set a shared token (8+ characters) in the settings", "warn"
        else:
            n = len(self.live_peers())
            who = "—"
            if head:
                who = "this device" if head["dev"] == self.dev else (head["name"] or head["dev"][:6])
            text, level = f"devices nearby: {n} · leader: {who}", "info"
        if text != self.last_status:
            self.last_status = text
            send({"type": "status", "level": level, "text": text})

    # ---- подпись HTTP
    def sign(self, *parts):
        return hmac.new(self.token, "\n".join(str(p) for p in parts).encode(), hashlib.sha256).hexdigest()

    def check_replay(self, sig):
        now = time.time()
        with self.lock:
            for s in [s for s, t in self.seen_sigs.items() if now - t > SKEW_S * 2]:
                del self.seen_sigs[s]
            if sig in self.seen_sigs:
                return False
            self.seen_sigs[sig] = now
            return True

    # ---------------------------------------------------------------- библиотека
    def reindex(self):
        key2id, id2key, key2path, tracks = {}, {}, {}, 0
        offset = 0
        while True:
            r = rpc("library.get_tracks", {"offset": offset, "limit": 200})
            batch = r.get("tracks") or []
            for t in batch:
                try:
                    k = track_key(t["path"], t["size"])
                except (KeyError, TypeError, ValueError):
                    continue
                id2key[t["id"]] = k
                if k not in key2id:
                    key2id[k] = t["id"]
                    key2path[k] = t["path"]
            tracks += len(batch)
            offset += len(batch)
            if not batch or offset >= int(r.get("total", 0)):
                break
        pls = {}
        for p in rpc("library.get_playlists").get("playlists", []):
            ks = [id2key[i] for i in p.get("track_ids", []) if i in id2key]
            pls[p["name"].strip().lower()] = {"name": p["name"], "keys": ks[:5000], "id": p["id"]}
        with self.lock:
            self.key2id, self.id2key, self.key2path = key2id, id2key, key2path
            self.local_pl = pls
            self.lib_digest = digest_of(sorted(key2id))
            self.pl_digest = digest_of(sorted((n, sorted(set(p["keys"]))) for n, p in pls.items()))
        self.last_reindex = time.monotonic()

    def playlists_wire(self):
        with self.lock:
            return {n: {"name": p["name"], "keys": p["keys"]} for n, p in self.local_pl.items()}

    def merge_playlists(self):
        """Объединяет «желаемые» плейлисты с локальными: только добавляет, ничего не удаляет."""
        if not self.flag("sync_playlists"):
            return
        with self.lock:
            wanted = {n: dict(w) for n, w in self.wanted_pl.items()}
        changed = False
        for n, w in wanted.items():
            with self.lock:
                have = self.local_pl.get(n)
                present = [self.key2id[k] for k in w["keys"] if k in self.key2id]
            if have is None:
                if present:
                    rpc("library.create_playlist", {"name": w["name"], "track_ids": present})
                    changed = True
            else:
                known = {self.key2id.get(k) for k in have["keys"]}
                add = [i for i in present if i not in known]
                if add:
                    rpc("library.add_to_playlist", {"id": have["id"], "track_ids": add})
                    changed = True
        if changed:
            self.jobs.put(("reindex",))

    # ---------------------------------------------------------------- наблюдение за ядром
    def observe(self):
        st = rpc("player.get_state")
        q = rpc("player.get_queue")
        qids = [i for i in q.get("track_ids", [])][: MAX_QUEUE + 1]
        if any(i not in self.id2key for i in qids) and time.monotonic() - self.last_reindex > 10:
            self.reindex()
        return {
            "ids": qids,
            "keys": [self.id2key.get(i) for i in qids],
            "index": int(q.get("index", 0) or 0),
            "playing": bool(st.get("playing")),
            "pos": int(st.get("position_ms", 0) or 0),
            "dur": int(st.get("duration_ms", 0) or 0),
            "vol": float(st.get("volume", 1.0)),
            "t": time.monotonic(),
        }

    @staticmethod
    def playback_value(o):
        """Значение регистра из наблюдения; None, если синхронизировать нечего."""
        keys, idx = o["keys"], o["index"]
        if not keys or len(keys) > MAX_QUEUE or not (0 <= idx < len(keys)) or keys[idx] is None:
            return None
        before = sum(1 for k in keys[:idx] if k is not None)
        return {"keys": [k for k in keys if k is not None], "index": before, "playing": o["playing"], "pos": o["pos"]}

    def watch(self):
        obs = self.observe()
        prev, self.last_obs = self.last_obs, obs
        if prev is not None:
            if self.flag("sync_playback") and self.playback_changed(prev, obs):
                v = self.playback_value(obs)
                if v:
                    self.publish("playback", v, at=obs["t"])
            if self.flag("sync_volume") and abs(obs["vol"] - prev["vol"]) > 0.004:
                self.publish("volume", round(obs["vol"], 3))

    @staticmethod
    def playback_changed(prev, obs):
        if obs["ids"] != prev["ids"] or obs["index"] != prev["index"]:
            return True
        if obs["playing"] != prev["playing"]:
            return True
        expected = prev["pos"] + (int((obs["t"] - prev["t"]) * 1000) if prev["playing"] else 0)
        return abs(obs["pos"] - expected) > POS_TOL_MS

    def watch_theme(self):
        t = rpc("settings.get_theme")
        sig = (t.get("name", ""), tuple(sorted((t.get("colors") or {}).items())))
        prev, self.last_theme = self.last_theme, sig
        if prev is not None and prev != sig and self.flag("sync_theme"):
            self.publish("theme", {"name": t.get("name", ""), "colors": t.get("colors") or {}})

    # ---------------------------------------------------------------- применение чужих изменений
    def apply(self, key):
        with self.lock:
            reg = self.regs.get(key)
        if not reg or reg["dev"] == self.dev:
            return
        if key == "playback" and self.flag("sync_playback"):
            self.apply_playback(reg)
        elif key == "volume" and self.flag("sync_volume"):
            rpc("player.set_volume", {"volume": float(reg["v"])})
            self.last_obs = self.observe()
        elif key == "theme" and self.flag("sync_theme"):
            rpc("settings.set_theme", {"name": reg["v"].get("name", ""), "colors": reg["v"]["colors"]})
            t = rpc("settings.get_theme")
            self.last_theme = (t.get("name", ""), tuple(sorted((t.get("colors") or {}).items())))

    def apply_playback(self, reg):
        v = reg["v"]
        keys, idx = v["keys"], v["index"]
        with self.lock:
            ids_ = [self.key2id.get(k) for k in keys]
        target = idx
        if ids_[idx] is None:
            self.pending_playback = True
            self.want_download(keys[idx], urgent=True)
            nxt = [i for i in range(idx + 1, len(keys)) if ids_[i]]
            if not nxt:
                return
            target = nxt[0]
        else:
            self.pending_playback = False
        pids = [i for i in ids_ if i]
        start = sum(1 for i in ids_[:target] if i)
        cur = self.observe()
        if cur["ids"] != pids or cur["index"] != start:
            rpc("player.play_queue", {"track_ids": pids, "start": start})
            time.sleep(0.5)
            cur = self.observe()
        if target == idx:
            want = v["pos"] + (int((time.monotonic() - reg["at"]) * 1000) if v["playing"] else 0)
            if time.monotonic() - reg["at"] >= 3600:
                want = v["pos"]
        else:
            want = 0
        if want > 0 and abs(cur["pos"] - want) > 2000 and (not cur["dur"] or want < cur["dur"] - 500):
            rpc("player.seek", {"position_ms": want})
            time.sleep(0.3)
            cur = self.observe()
        # «играет» из очень старой записи не запускает музыку при подключении устройства
        playing = v["playing"] and time.monotonic() - reg["at"] < 3600
        if cur["playing"] != playing:
            rpc("player.play" if playing else "player.pause")
            time.sleep(0.3)
        self.last_obs = self.observe()  # поглощаем собственные изменения — это не «локальная правка»

    # ---------------------------------------------------------------- сеть: клиент
    def request(self, peer, method, path, body=None):
        """Подписанный запрос к другому устройству. Возвращает (status, bytes) или бросает OSError."""
        if self.token is None:
            raise OSError("no token")
        payload = json.dumps(body, ensure_ascii=False).encode() if body is not None else b""
        ts = str(int(time.time()))
        nonce = secrets.token_hex(8)
        sig = self.sign("req", ts, nonce, self.dev, method, path, sha(payload))
        conn = http.client.HTTPConnection(peer["host"], peer["port"], timeout=5)
        try:
            conn.request(
                method,
                path,
                body=payload or None,
                headers={
                    "X-Mpt-Ts": ts,
                    "X-Mpt-Nonce": nonce,
                    "X-Mpt-Dev": self.dev,
                    "X-Mpt-Sig": sig,
                    "Content-Type": "application/json",
                },
            )
            resp = conn.getresponse()
            data = resp.read(MAX_BODY + 1)
            if resp.status != 200 and not resp.getheader("X-Mpt-Rsig"):
                raise OSError(f"HTTP {resp.status} (tokens differ?)")
            if len(data) > MAX_BODY:
                raise OSError("response too large")
            want = self.sign("resp", sig, resp.status, sha(data))
            if not hmac.compare_digest(want, resp.getheader("X-Mpt-Rsig", "")):
                raise OSError("bad response signature (different tokens?)")
            return resp.status, data
        finally:
            conn.close()

    def call_json(self, peer, method, path, body=None):
        status, data = self.request(peer, method, path, body)
        if status != 200:
            raise OSError(f"HTTP {status}")
        return json.loads(data.decode())

    def sync_with(self, peer, push_keys=()):
        """Обмен: мы шлём «головы» регистров и то, что только что изменилось; в ответ — новее у них."""
        with self.lock:
            heads = {k: [r["ts"], r["dev"]] for k, r in self.regs.items()}
        regs = {k: w for k in push_keys if (w := self.wire(k))}
        body = {
            "dev": self.dev,
            "name": self.name,
            "port": self.http_port,
            "heads": heads,
            "regs": regs,
            "lib": self.lib_digest,
            "pl": self.pl_digest,
        }
        r = self.call_json(peer, "POST", "/sync", body)
        self.touch(peer, r.get("dev"), r.get("name"))
        for k, w in (r.get("regs") or {}).items():
            if k in ("playback", "volume", "theme"):
                self.merge(k, w)
        want = [k for k in (r.get("want") or []) if k in ("playback", "volume", "theme") and k not in regs]
        if want:
            more = {k: w for k in want if (w := self.wire(k))}
            self.call_json(peer, "POST", "/sync", {**body, "regs": more, "heads": heads})
        self.after_digests(peer, r.get("lib"), r.get("pl"))

    def touch(self, peer, dev=None, name=None):
        peer["seen"] = time.monotonic()
        if isinstance(dev, str) and re.fullmatch(r"[0-9a-f]{12}", dev):
            peer["dev"] = dev
        if isinstance(name, str):
            peer["name"] = name[:40]
        self.update_status()

    def after_digests(self, peer, lib, pl):
        if isinstance(lib, str) and lib != self.lib_digest and lib != peer.get("lib_seen"):
            self.jobs.put(("pull_lib", peer, lib))
        if isinstance(pl, str) and pl != self.pl_digest and pl != peer.get("pl_seen") and self.flag("sync_playlists"):
            self.jobs.put(("pull_pl", peer, pl))

    def pull_lib(self, peer, digest):
        if not self.flag("download_missing"):
            peer["lib_seen"] = digest
            return
        r = self.call_json(peer, "GET", "/library")
        for k in r.get("keys", []):
            if isinstance(k, str):
                self.want_download(k, peer=peer)
        peer["lib_seen"] = digest

    def pull_pl(self, peer, digest):
        r = self.call_json(peer, "GET", "/playlists")
        with self.lock:
            for n, p in (r.get("playlists") or {}).items():
                if not (isinstance(n, str) and isinstance(p, dict) and isinstance(p.get("keys"), list)):
                    continue
                keys = [k for k in p["keys"] if isinstance(k, str)][:5000]
                cur = self.wanted_pl.setdefault(n[:200], {"name": str(p.get("name", n))[:200], "keys": []})
                have = set(cur["keys"])
                cur["keys"].extend(k for k in keys if k not in have)
        peer["pl_seen"] = digest
        self.merge_playlists()

    # ---------------------------------------------------------------- скачивание
    def want_download(self, key, peer=None, urgent=False):
        if not self.flag("download_missing") or self.token is None:
            return
        with self.lock:
            if key in self.key2id or key in self.dl_active:
                return
            if time.monotonic() < self.dl_failed.get(key, 0):
                return
            self.dl_active.add(key)
        self.dl_q.put((0 if urgent else 1, next(self.dl_seq), key, peer))

    def download_dir(self):
        folders = rpc("library.get_folders").get("folders", [])
        d = self.cfg_str("download_dir")
        if d:
            d = os.path.expanduser(d)
        else:
            base = folders[0] if folders else os.path.join(os.path.expanduser("~"), "Music")
            d = os.path.join(base, "mpt-sync")
        os.makedirs(d, exist_ok=True)
        norm = os.path.abspath(d)
        # подпапка добавленной папки уже сканируется; иначе добавим саму папку в библиотеку
        if not any(norm == os.path.abspath(f) or norm.startswith(os.path.abspath(f) + os.sep) for f in folders):
            rpc("library.add_folder", {"path": d})
        return d

    def download_worker(self):
        done = 0
        while running.is_set():
            try:
                _, _, key, peer = self.dl_q.get(timeout=2)
            except queue.Empty:
                if self.need_rescan:
                    self.need_rescan = False
                    self.safe(lambda: rpc("library.rescan"))
                continue
            try:
                self.download(key, peer)
                done += 1
                self.need_rescan = True
                if self.dl_q.empty() or done % 20 == 0:
                    self.need_rescan = False
                    rpc("library.rescan")
            except (OSError, RpcError, ValueError) as e:
                self.dl_failed[key] = time.monotonic() + 60
                log(f"download failed for {key.split('|')[0]}: {e}", "warn")
            finally:
                with self.lock:
                    self.dl_active.discard(key)

    def download(self, key, peer):
        candidates = [peer] if peer else self.live_peers()
        name, size_s = key.rsplit("|", 1)
        size = int(size_s)
        last_err = None
        for p in candidates:
            try:
                self.fetch_file(p, key, size)
                return
            except OSError as e:
                last_err = e
        raise OSError(last_err or "no devices")

    def fetch_file(self, peer, key, size):
        d = self.download_dir()
        path_q = "/file?k=" + urllib.parse.quote(key, safe="")
        ts = str(int(time.time()))
        nonce = secrets.token_hex(8)
        sig = self.sign("req", ts, nonce, self.dev, "GET", path_q, sha(b""))
        conn = http.client.HTTPConnection(peer["host"], peer["port"], timeout=15)
        # безопасное имя: из ключа, без пути; оригинальный регистр сервер передаст в заголовке
        tmp = None
        try:
            conn.request("GET", path_q, headers={"X-Mpt-Ts": ts, "X-Mpt-Nonce": nonce, "X-Mpt-Dev": self.dev, "X-Mpt-Sig": sig})
            resp = conn.getresponse()
            if resp.status != 200:
                resp.read()
                raise OSError(f"HTTP {resp.status}")
            if not hmac.compare_digest(self.sign("file", sig, size), resp.getheader("X-Mpt-Rsig", "")):
                raise OSError("bad header signature")
            name = safe_name(urllib.parse.unquote(resp.getheader("X-Mpt-Name", "")))
            if track_key(name, size) != key:
                raise OSError("file name does not match the key")
            final = os.path.join(d, name)
            if os.path.exists(final):
                stem, ext = os.path.splitext(name)
                final = os.path.join(d, f"{stem} ({key[-6:].replace('|', '')}){ext}")
            tmp = final + ".part"
            mac = hmac.new(self.token, ("file-body\n" + sig + "\n").encode(), hashlib.sha256)
            got = 0
            with open(tmp, "wb") as f:
                while got < size:
                    chunk = resp.read(min(1 << 16, size - got))
                    if not chunk:
                        raise OSError("connection lost")
                    mac.update(chunk)
                    f.write(chunk)
                    got += len(chunk)
            tag = resp.read(TAG_LEN)
            if len(tag) != TAG_LEN or not hmac.compare_digest(mac.digest(), tag):
                raise OSError("content failed signature check")
            os.replace(tmp, final)
            tmp = None
        finally:
            conn.close()
            if tmp:
                try:
                    os.remove(tmp)
                except OSError:
                    pass

    # ---------------------------------------------------------------- сеть: сервер
    def serve(self):
        plugin = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            timeout = 15

            def log_message(self, *a):
                pass

            def reply(self, status, obj, req_sig):
                data = json.dumps(obj, ensure_ascii=False).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("X-Mpt-Rsig", plugin.sign("resp", req_sig, status, sha(data)))
                self.end_headers()
                self.wfile.write(data)

            def auth(self, body):
                """Возвращает подпись запроса или None (ответ об ошибке уже отправлен)."""
                if plugin.token is None:
                    self.fail(503)
                    return None
                ts = self.headers.get("X-Mpt-Ts", "")
                dev = self.headers.get("X-Mpt-Dev", "")
                nonce = self.headers.get("X-Mpt-Nonce", "")
                sig = self.headers.get("X-Mpt-Sig", "")
                try:
                    fresh = abs(time.time() - int(ts)) <= SKEW_S
                except ValueError:
                    fresh = False
                want = plugin.sign("req", ts, nonce, dev, self.command, self.path, sha(body))
                if not (fresh and hmac.compare_digest(want, sig)) or not plugin.check_replay(sig):
                    time.sleep(0.3)
                    self.fail(403)
                    return None
                return sig

            def fail(self, status):
                self.send_response(status)
                self.send_header("Content-Length", "0")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True

            def body(self):
                try:
                    n = int(self.headers.get("Content-Length", "0"))
                except ValueError:
                    n = -1
                if n < 0 or n > MAX_BODY:
                    self.fail(413)
                    return None
                return self.rfile.read(n) if n else b""

            def do_GET(self):
                body = self.body()
                if body is None:
                    return
                sig = self.auth(body)
                if sig is None:
                    return
                path = urllib.parse.urlsplit(self.path)
                if path.path == "/library":
                    with plugin.lock:
                        self.reply(200, {"keys": sorted(plugin.key2id)}, sig)
                elif path.path == "/playlists":
                    self.reply(200, {"playlists": plugin.playlists_wire()}, sig)
                elif path.path == "/file":
                    self.send_file(sig, urllib.parse.parse_qs(path.query).get("k", [""])[0])
                else:
                    self.reply(404, {}, sig)

            def send_file(self, sig, key):
                with plugin.lock:
                    src = plugin.key2path.get(key)
                if not plugin.flag("share_files") or not src:
                    return self.reply(404, {}, sig)
                try:
                    size = os.path.getsize(src)
                    f = open(src, "rb")
                except OSError:
                    return self.reply(404, {}, sig)
                if track_key(src, size) != key or not plugin.upload_slots.acquire(blocking=False):
                    f.close()
                    return self.reply(404 if track_key(src, size) != key else 503, {}, sig)
                try:
                    mac = hmac.new(plugin.token, ("file-body\n" + sig + "\n").encode(), hashlib.sha256)
                    self.send_response(200)
                    self.send_header("Content-Type", "application/octet-stream")
                    self.send_header("Content-Length", str(size + TAG_LEN))
                    self.send_header("X-Mpt-Rsig", plugin.sign("file", sig, size))
                    self.send_header("X-Mpt-Name", urllib.parse.quote(safe_name(src)))
                    self.end_headers()
                    left = size
                    while left > 0:
                        chunk = f.read(min(1 << 16, left))
                        if not chunk:
                            raise OSError("file changed while sending")
                        mac.update(chunk)
                        self.wfile.write(chunk)
                        left -= len(chunk)
                    self.wfile.write(mac.digest())
                except OSError:
                    self.close_connection = True
                finally:
                    f.close()
                    plugin.upload_slots.release()

            def do_POST(self):
                body = self.body()
                if body is None:
                    return
                sig = self.auth(body)
                if sig is None:
                    return
                if self.path != "/sync":
                    return self.reply(404, {}, sig)
                try:
                    req = json.loads(body.decode())
                    dev = req["dev"]
                    if not (isinstance(dev, str) and re.fullmatch(r"[0-9a-f]{12}", dev)) or dev == plugin.dev:
                        raise ValueError("dev")
                    heads = req.get("heads") or {}
                    incoming = req.get("regs") or {}
                    port = int(req["port"])
                except (ValueError, KeyError, TypeError):
                    return self.reply(400, {}, sig)
                peer = plugin.peer_at(self.client_address[0], port)
                plugin.touch(peer, dev, req.get("name"))
                for k, w in incoming.items():
                    if k in ("playback", "volume", "theme"):
                        plugin.merge(k, w)
                out, want = {}, []
                with plugin.lock:
                    mine = dict(plugin.regs)
                for k in ("playback", "volume", "theme"):
                    h = heads.get(k)
                    try:
                        theirs = (int(h[0]), str(h[1])) if h else None
                    except (TypeError, ValueError, IndexError):
                        theirs = None
                    r = mine.get(k)
                    if r and (theirs is None or order(r) > theirs):
                        out[k] = plugin.wire(k)
                    elif theirs and (not r or theirs > order(r)) and k not in incoming:
                        want.append(k)
                plugin.after_digests(peer, req.get("lib"), req.get("pl"))
                self.reply(
                    200,
                    {"dev": plugin.dev, "name": plugin.name, "regs": out, "want": want, "lib": plugin.lib_digest, "pl": plugin.pl_digest},
                    sig,
                )

        class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
            daemon_threads = True
            allow_reuse_address = True
            request_queue_size = 16

        port = int(self.cfg.get("port", 47821))
        self.server = Server(("", port), Handler)
        self.http_port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def peer_at(self, host, port):
        with self.lock:
            p = self.peers.get((host, port))
            if p is None:
                p = {"host": host, "port": port, "seen": time.monotonic(), "manual": False}
                self.peers[(host, port)] = p
            return p

    # ---------------------------------------------------------------- поиск устройств
    def beacon_msg(self):
        t = str(int(time.time()))
        return json.dumps(
            {"d": self.dev, "n": self.name, "p": self.http_port, "t": t, "s": self.sign("beacon", self.dev, self.http_port, t)}
        ).encode()

    def discovery(self):
        port = int(self.cfg.get("port", 47821))
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        try:
            sock.bind(("", port))
        except OSError as e:
            log(f"UDP port {port} is busy, device discovery will not work: {e}", "warn")
            sock = None
        if sock:
            threading.Thread(target=self.listen, args=(sock,), daemon=True).start()
        sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sender.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        while running.is_set():
            if self.token is not None:
                try:
                    sender.sendto(self.beacon_msg(), ("255.255.255.255", port))
                except OSError:
                    pass  # нет сети — не страшно, попробуем позже
            time.sleep(BEACON_S)

    def listen(self, sock):
        while running.is_set():
            try:
                data, addr = sock.recvfrom(1024)
                m = json.loads(data.decode())
                dev, p, t, s = m["d"], int(m["p"]), str(m["t"]), m["s"]
            except (OSError, ValueError, KeyError, TypeError):
                continue
            if self.token is None or dev == self.dev or not (1024 <= p <= 65535):
                continue
            try:
                fresh = abs(time.time() - int(t)) <= SKEW_S
            except ValueError:
                continue
            if fresh and hmac.compare_digest(self.sign("beacon", dev, p, t), str(s)):
                peer = self.peer_at(addr[0], p)
                new = "dev" not in peer
                self.touch(peer, dev, m.get("n"))
                if new:
                    self.push_q.put("*")  # познакомились — обменяться состоянием

    def manual_peers(self):
        for item in self.cfg_str("peers").replace(";", ",").split(","):
            item = item.strip()
            if not item:
                continue
            host, _, port = item.rpartition(":")
            if not host:
                host, port = item, str(self.cfg.get("port", 47821))
            try:
                p = self.peer_at(host, int(port))
                p["manual"] = True
            except ValueError:
                log(f"cannot parse address '{item}'", "warn")

    # ---------------------------------------------------------------- фоновые потоки
    def safe(self, fn):
        try:
            return fn()
        except (RpcError, OSError, ValueError, KeyError, TypeError) as e:
            log(f"{type(e).__name__}: {e}", "warn")

    def worker(self):
        next_watch = next_theme = 0.0
        self.safe(self.reindex)
        while running.is_set():
            playing = bool(self.last_obs and self.last_obs["playing"])
            try:
                job = self.jobs.get(timeout=1.0 if playing else 2.5)
            except queue.Empty:
                job = None
            if job:
                if job[0] == "apply":
                    self.safe(lambda: self.apply(job[1]))
                elif job[0] == "reindex":
                    self.safe(self.reindex)
                    self.safe(self.merge_playlists)
                    self.safe(self.retry_pending)
                    self.push_q.put("*")
                elif job[0] == "pull_lib":
                    self.safe(lambda: self.pull_lib(job[1], job[2]))
                elif job[0] == "pull_pl":
                    self.safe(lambda: self.pull_pl(job[1], job[2]))
                elif job[0] == "kick":
                    next_watch = 0
                elif job[0] == "resync":
                    for p in self.peers.values():
                        p.pop("lib_seen", None)
                        p.pop("pl_seen", None)
                    self.push_q.put("*")
            now = time.monotonic()
            if now >= next_watch and self.token is not None:
                self.safe(self.watch)
                next_watch = now + 0.5
            if now >= next_theme and self.token is not None and self.flag("sync_theme"):
                self.safe(self.watch_theme)
                next_theme = now + 4

    def retry_pending(self):
        if self.pending_playback:
            self.apply("playback")

    def sync_loop(self):
        last_full = 0.0
        while running.is_set():
            keys = set()
            try:
                keys.add(self.push_q.get(timeout=1.0))
                while True:
                    keys.add(self.push_q.get_nowait())
            except queue.Empty:
                pass
            now = time.monotonic()
            full = now - last_full >= SYNC_S
            if self.token is None or not (keys or full):
                continue
            if full:
                last_full = now
                self.manual_peers()
            push = [k for k in keys if k in ("playback", "volume", "theme")]
            for peer in list(self.peers.values()):
                threading.Thread(target=self.sync_peer, args=(peer, push), daemon=True).start()
            self.expire_peers()

    def sync_peer(self, peer, push):
        if peer.get("busy"):
            return
        peer["busy"] = True
        try:
            self.sync_with(peer, push)
        except (OSError, ValueError, KeyError, TypeError):
            pass  # устройство недоступно — не шумим, повторим на следующем круге
        finally:
            peer["busy"] = False

    def expire_peers(self):
        now = time.monotonic()
        with self.lock:
            for k in [k for k, p in self.peers.items() if not p.get("manual") and now - p["seen"] > 180]:
                del self.peers[k]
        self.update_status()

    # ---------------------------------------------------------------- сообщения ядра
    def on_event(self, name, data):
        if name == "config_changed":
            old_port = self.cfg.get("port")
            self.cfg = dict((data or {}).get("config", self.cfg))
            self.name = self.cfg_str("device_name") or socket.gethostname()[:40]
            if self.cfg.get("port") != old_port:
                send({"type": "status", "level": "warn", "text": "the new port applies after the plugin restarts"})
            self.last_status = None
            self.update_status()
            self.jobs.put(("resync",))
        elif name == "library_changed":
            self.jobs.put(("reindex",))
        elif name in ("state", "track_changed", "queue_changed"):
            self.jobs.put(("kick",))

    def on_command(self, name):
        if name == "sync_now":
            self.jobs.put(("resync",))
        elif name == "peers":
            live = self.live_peers()
            text = ", ".join(f"{p.get('name') or '?'} ({p['host']}:{p['port']})" for p in live) or "nobody in sight"
            send({"type": "status", "level": "info", "text": f"devices: {text}"})
            self.last_status = None

    def start(self):
        self.update_status()
        if self.token is not None:
            self.start_net()
        else:
            log("token not set — sync is off", "warn")

    def start_net(self):
        try:
            self.serve()
        except OSError as e:
            send({"type": "status", "level": "error", "text": f"cannot open port {self.cfg.get('port')}: {e}"})
            return
        for target in (self.worker, self.sync_loop, self.discovery, self.download_worker):
            threading.Thread(target=target, daemon=True).start()
        self.net_started = True


def main():
    hello = json.loads(sys.stdin.readline())
    send({"type": "ready", "subscribe": ["state", "track_changed", "queue_changed", "library_changed"]})
    plugin = Plugin(hello)
    plugin.net_started = False
    plugin.start()
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        kind = msg.get("type")
        if kind == "shutdown":
            break
        if kind == "response":
            w = waiters.pop(msg.get("id"), None)
            if w:
                w[1].update(msg)
                w[0].set()
        elif kind == "event":
            plugin.on_event(msg.get("name"), msg.get("data"))
            if msg.get("name") == "config_changed" and plugin.token is not None and not plugin.net_started:
                plugin.start_net()
        elif kind == "command":
            plugin.on_command(msg.get("name"))
    running.clear()
    plugin.save_state()
    if plugin.server:
        threading.Thread(target=plugin.server.shutdown, daemon=True).start()
    sys.stdout.flush()
    os._exit(0)


if __name__ == "__main__":
    main()
