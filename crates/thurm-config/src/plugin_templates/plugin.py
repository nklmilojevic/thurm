#!/usr/bin/env python3
"""NAME: a Thurm plugin that runs as a program. After editing: thurm plugin reload NAME

Thurm starts it with THURM_PLUGIN, THURM_PLUGIN_DIR and THURM_PLUGIN_SETTINGS (JSON) set, and
talks to it in JSON-RPC 2.0, one message per line, on its standard input and output. Its
requests are methods of Thurm's API; Thurm sends events as notifications and asks it to run
its actions with `action.invoke`. When its standard input closes, it should exit.
"""

import json
import os
import sys
import threading

NAME = os.environ.get("THURM_PLUGIN", "NAME")
SETTINGS = json.loads(os.environ.get("THURM_PLUGIN_SETTINGS") or "null") or {}

_lock = threading.Lock()
_next_id = 0
_waiting = {}
_actions = {}


def send(msg):
    msg["jsonrpc"] = "2.0"
    with _lock:
        sys.stdout.write(json.dumps(msg) + "\n")
        sys.stdout.flush()


def call(method, params=None):
    """Calls a method of Thurm's API and returns its result (raises on an error)."""
    global _next_id
    done = threading.Event()
    with _lock:
        _next_id += 1
        id = _next_id
        _waiting[id] = [done, None]
    send({"id": id, "method": method, "params": params})
    done.wait()
    reply = _waiting.pop(id)[1]
    if "error" in reply:
        raise RuntimeError(reply["error"]["message"])
    return reply.get("result")


def action(name, title, run):
    """Adds an action: `run(ctx, args)` returns its result."""
    _actions[name] = run
    call("actions.register", {"actions": [{"name": name, "title": title}]})


def _invoke(msg):
    p = msg["params"]
    try:
        result = _actions[p["name"]](p.get("ctx") or {}, p.get("args"))
        send({"id": msg["id"], "result": result})
    except Exception as e:  # noqa: BLE001 - the error goes back to whoever ran the action
        send({"id": msg["id"], "error": {"code": 1, "message": str(e)}})


def on_event(event):
    """An event this plugin subscribed to."""
    call("log", {"message": f"{event['event']} in pane {event.get('pane', {}).get('id')}"})


def setup():
    action(f"{NAME}.hello", f"{NAME}: Say hello",
           lambda ctx, args: call("ui.toast", {"text": f"Hello from {NAME}", "pane": ctx.get("pane")}))
    call("events.subscribe", {"names": ["agent_finished"]})


def main():
    # Calls wait for answers that come in on stdin: make them off the reading loop.
    threading.Thread(target=setup, daemon=True).start()
    for line in sys.stdin:
        msg = json.loads(line)
        method = msg.get("method")
        if method is None:
            entry = _waiting.get(msg.get("id"))
            if entry:
                entry[1] = msg
                entry[0].set()
        elif method == "action.invoke":
            threading.Thread(target=_invoke, args=(msg,), daemon=True).start()
        elif method == "event":
            threading.Thread(target=on_event, args=(msg["params"],), daemon=True).start()


if __name__ == "__main__":
    main()
