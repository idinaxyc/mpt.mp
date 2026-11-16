#!/usr/bin/env python3
"""mpt.mp plugin: pause after N minutes. Example of core requests and UI commands."""
import itertools
import json
import sys
import threading

write_lock = threading.Lock()
ids = itertools.count(1)


def send(obj):
    with write_lock:  # the timer and the main thread share one stdout
        sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
        sys.stdout.flush()


def main():
    hello = json.loads(sys.stdin.readline())
    minutes = float(hello.get("config", {}).get("minutes", 30))
    send({"type": "ready", "subscribe": []})

    timer = None

    def fire():
        send({"type": "request", "id": next(ids), "method": "player.pause", "params": {}})
        send({"type": "status", "level": "info", "text": "sleep timer fired"})

    for line in sys.stdin:
        msg = json.loads(line)
        kind = msg.get("type")
        if kind == "shutdown":
            break
        if kind == "command" and msg.get("name") == "start":
            if timer:
                timer.cancel()
            timer = threading.Timer(minutes * 60, fire)
            timer.daemon = True
            timer.start()
            send({"type": "status", "level": "info", "text": f"pausing in {minutes:g} min"})
        elif kind == "command" and msg.get("name") == "cancel":
            if timer:
                timer.cancel()
                timer = None
            send({"type": "status", "level": "info", "text": "timer cancelled"})
        elif kind == "event" and msg.get("name") == "config_changed":
            minutes = float(msg.get("data", {}).get("config", {}).get("minutes", minutes))
        elif kind == "response" and not msg.get("ok"):
            send({"type": "log", "level": "warn", "msg": f"core rejected the request: {msg.get('error')}"})

    if timer:
        timer.cancel()


if __name__ == "__main__":
    main()
