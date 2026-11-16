#!/usr/bin/env python3
"""/roll for Den. Output of `roll` is posted to the channel (output = "send")."""
import json, os, random, re, subprocess, sys
from pathlib import Path

STATE = Path(os.environ.get("DEN_MOD_STATE_DIR", ".")) / "rolls.json"


def load():
    try:
        return json.loads(STATE.read_text())
    except (OSError, ValueError):
        return []


def widgets(rolls):
    out = [{"type": "heading", "text": "Recent rolls"}]
    if not rolls:
        out.append({"type": "text", "text": "nothing rolled yet", "color": "muted", "small": True})
    for r in rolls[-6:][::-1]:
        out.append({"type": "kv", "key": r["spec"], "value": str(r["total"])})
    out.append({"type": "row", "children": [
        {"type": "button", "label": "d20", "command": "roll", "args": ["1d20"]},
        {"type": "button", "label": "2d6", "command": "roll", "args": ["2d6"]},
    ]})
    return out


def push_panel(rolls):
    """Refresh our panel right away instead of waiting for a poll."""
    exe = os.environ.get("DEN_BIN")
    if not exe:
        return
    params = {"panel": "example.dice/history", "widgets": widgets(rolls)}
    subprocess.run([exe, "api", "ui.panel.set", json.dumps(params)], capture_output=True)


def roll(spec):
    m = re.fullmatch(r"(\d*)d(\d+)([+-]\d+)?", spec.strip().lower() or "1d6")
    if not m:
        print(f"can't roll {spec!r}; try 2d6 or d20+3", file=sys.stderr)
        sys.exit(1)
    n, sides, mod = int(m[1] or 1), int(m[2]), int(m[3] or 0)
    n, sides = min(n, 50), max(2, min(sides, 1000))
    dice = [random.randint(1, sides) for _ in range(n)]
    total = sum(dice) + mod
    rolls = load() + [{"spec": spec or "1d6", "total": total}]
    STATE.parent.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps(rolls[-50:]))
    push_panel(rolls)
    detail = " + ".join(map(str, dice)) + (f" {'+' if mod >= 0 else '-'} {abs(mod)}" if mod else "")
    print(f"rolled {spec or '1d6'}: {detail} = {total}")


mode = sys.argv[1] if len(sys.argv) > 1 else ""
if mode == "roll":
    roll(" ".join(sys.argv[2:]))
elif mode == "panel":
    print(json.dumps(widgets(load())))
