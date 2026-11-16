#!/usr/bin/env python3
"""Long-running mod: stream events from the control API and keep a panel fresh.

Same pattern any external tool (or AI agent) can use:
  tailchat api events.subscribe '{"events": ["message.*"]}'   -> JSON lines
  tailchat api ui.panel.set '{"panel": "...", "widgets": [...]}'
"""
import collections, json, os, subprocess

EXE = os.environ["TAILCHAT_BIN"]
PANEL = "example.stats/top"
counts = collections.Counter()


def api(method, params):
    subprocess.run([EXE, "api", method, json.dumps(params)], capture_output=True)


def render():
    widgets = [{"type": "heading", "text": "Most messages"}]
    total = sum(counts.values()) or 1
    for nick, n in counts.most_common(5):
        widgets.append({"type": "progress", "value": n / total, "text": f"{nick} · {n}"})
    if not counts:
        widgets.append({"type": "text", "text": "waiting for chat…", "color": "muted", "small": True})
    api("ui.panel.set", {"panel": PANEL, "widgets": widgets})


render()
stream = subprocess.Popen([EXE, "api", "events.subscribe", json.dumps({"events": ["message.created", "connection.changed"]})],
                          stdout=subprocess.PIPE, text=True)
for line in stream.stdout:
    try:
        ev = json.loads(line)
    except ValueError:
        continue
    if ev.get("event") == "connection.changed" and ev.get("data", {}).get("state") == "idle":
        counts.clear()
    msg = ev.get("data", {}).get("message", {})
    if msg.get("kind") == "user":
        counts[msg.get("author", "?")] += 1
    render()
