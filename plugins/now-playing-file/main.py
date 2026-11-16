#!/usr/bin/env python3
"""mpt.mp plugin: writes "artist — title" to a file.

Protocol: one JSON object per line on stdin/stdout. Anything that is not a
JSON message must not go to stdout — use stderr or {"type": "log"} for logs.
"""
import json
import os
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()  # without flush the core receives nothing


def main():
    hello = json.loads(sys.stdin.readline())
    config = hello.get("config", {})
    out_path = os.path.expanduser(config.get("output", "~/.cache/mpt-now-playing.txt"))
    template = config.get("template", "{artist} — {title}")
    clear_on_pause = config.get("clear_on_pause", True)

    send({"type": "ready", "subscribe": ["track_changed", "state"]})

    track = None
    playing = False

    def write():
        if track is None or (clear_on_pause and not playing):
            text = ""
        else:
            text = template.format(
                artist=track.get("artist") or "",
                title=track.get("title") or "",
                album=track.get("album") or "",
            ).strip(" —")
        os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
        tmp = out_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(tmp, out_path)  # atomic: readers never see half a file
        send({"type": "status", "level": "info", "text": text or "paused"})

    for line in sys.stdin:
        msg = json.loads(line)
        kind = msg.get("type")
        if kind == "shutdown":
            break
        if kind != "event":
            continue  # ignore unknown message types
        data = msg.get("data", {})
        name = msg.get("name")
        if name == "track_changed":
            track = data.get("track")
            write()
        elif name == "state":
            now = bool(data.get("playing"))
            if now != playing:
                playing = now
                write()


if __name__ == "__main__":
    main()
