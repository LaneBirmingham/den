#!/usr/bin/env python3
"""The event arrives as JSON on stdin; whatever we print is said by the server."""
import json, sys

event = json.load(sys.stdin)
member = event.get("data", {}).get("member", {})
nick = member.get("nick", "friend")
login = (member.get("identity") or {}).get("login")
print(f"welcome, {nick}!" + (f" (signed in as {login})" if login else ""))
