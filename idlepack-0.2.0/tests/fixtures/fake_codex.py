#!/usr/bin/env python3
"""A stand-in for `codex app-server`, speaking just enough of the v2 protocol.

Each request it receives is appended to $FAKE_CODEX_LOG, one JSON line each.

  initialize            -> a reply
  turn/start            -> turn/started, thread/tokenUsage/updated (the thread's
                           context: 312000, or the "context" param), turn/completed,
                           and a reply
  thread/compact/start  -> a reply {}, then a compaction turn: turn/started, a
                           re-estimated tokenUsage (no input/output, 18000 total),
                           item/completed contextCompaction, turn/completed
  --version (argv)      -> prints and exits 3, to test the pass-through
"""
import json
import os
import sys

if "--version" in sys.argv:
    print("codex-fake 1.0 " + " ".join(sys.argv[1:]))
    sys.exit(3)

LOG = os.environ.get("FAKE_CODEX_LOG")


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def usage(thread, turn, total, inp, out, window=400000):
    send({"method": "thread/tokenUsage/updated", "params": {"threadId": thread, "turnId": turn, "tokenUsage": {
        "total": {"totalTokens": total, "inputTokens": inp, "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                  "outputTokens": out, "reasoningOutputTokens": 0},
        "last": {"totalTokens": total, "inputTokens": inp, "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                 "outputTokens": out, "reasoningOutputTokens": 0},
        "modelContextWindow": window}}})


turns = 0
for raw in sys.stdin:
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    if LOG:
        with open(LOG, "a") as fh:
            fh.write(json.dumps(msg) + "\n")
    method, rid, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
    thread = params.get("threadId")
    if method == "initialize":
        send({"id": rid, "result": {"userAgent": "fake"}})
    elif method == "turn/start":
        turns += 1
        turn = f"u{turns}"
        send({"id": rid, "result": {"turn": {"id": turn, "items": [], "status": "inProgress"}}})
        send({"method": "turn/started", "params": {"threadId": thread, "turn": {"id": turn, "items": [], "status": "inProgress"}}})
        context = int(params.get("context", 312000))
        usage(thread, turn, context, context - 2000, 2000)
        send({"method": "item/completed", "params": {"threadId": thread, "turnId": turn,
              "item": {"type": "agentMessage", "id": "m", "text": "done"}, "completedAtMs": 1}})
        send({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "items": [], "status": "completed"}}})
    elif method == "thread/compact/start":
        turns += 1
        turn = f"u{turns}"
        send({"id": rid, "result": {}})
        send({"method": "turn/started", "params": {"threadId": thread, "turn": {"id": turn, "items": [], "status": "inProgress"}}})
        usage(thread, turn, 18000, 0, 0)
        item = {"type": "contextCompaction", "id": "c1"}
        send({"method": "item/completed", "params": {"threadId": thread, "turnId": turn, "item": item, "completedAtMs": 2}})
        send({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "items": [item], "status": "completed"}}})
    elif rid is not None:
        send({"id": rid, "error": {"code": -32601, "message": f"unknown method {method}"}})
