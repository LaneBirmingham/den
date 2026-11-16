# Mods

A mod is a folder containing `tailchat-mod.toml` and whatever it runs. Mods
are separate processes in any language — tailchat never loads code into
itself — so a broken mod can't crash the app, and the same mod works with any
frontend.

```sh
tailchat mods new my.notes          # scaffold into <config>/mods/my.notes
tailchat mods link ./path/to/mod    # use a mod from anywhere
tailchat mods list                  # status, problems, where each came from
tailchat mods disable my.notes      # or enable; running apps reload
```

Mods are found in `<config>/mods/*`, linked folders (`mods.toml`), and
`$TAILCHAT_MODS_PATH` (path list; a mod folder or a folder of mods).
Turn the whole system off with `[modules] mods = false` in `config.toml`.

## Manifest

```toml
id = "example.dice"              # letters, digits, . - _ (max 64); unique
name = "Dice"
version = "0.1.0"
description = "…"
author = "…"
min_tailchat_version = "0.1.0"
platforms = ["macos", "linux", "windows"]   # omit = everywhere
runs_on = ["client"]                         # client (app/terminal) and/or server (`tailchat serve`)

[[commands]]                     # /roll 2d6
name = "roll"
description = "Roll dice"
usage = "[NdM]"
command = ["python3", "dice.py", "roll"]     # run in the mod folder; user args are appended
output = "send"                  # send (post to channel) | notice (toast, default) | none
timeout_seconds = 10

[[hooks]]                        # react to events
on = "message.created"           # exact, "message.*", or "*"
command = ["python3", "hook.py"]
output = "none"
include_self = false             # skip messages this node sent (prevents loops)

[[panels]]                       # a panel layouts can place as "example.dice/history"
id = "history"
title = "Dice"
slot = "right"                   # hint for "mods:<slot>" placeholders
command = ["python3", "dice.py", "panel"]    # stdout = widgets; omit to fill via the API
interval_seconds = 30            # re-run periodically (omit = once at start)

[[startup]]                      # run when the mod loads
command = ["python3", "daemon.py"]
daemon = true                    # keep running (killed on reload/exit)

themes = ["sakura.toml"]         # appear in View → Theme
layouts = ["focus.toml"]         # appear in View → Layout
```

## Environment

| Variable | Set for | Value |
| --- | --- | --- |
| `TAILCHAT_BIN` | all | path to the running tailchat binary (`$TAILCHAT_BIN api …`) |
| `TAILCHAT_API`, `TAILCHAT_API_TOKEN` | all | control API address and token of *this* instance |
| `TAILCHAT_NODE` | all | `client` or `server` |
| `TAILCHAT_MOD_ID`, `TAILCHAT_MOD_ROOT` | all | the mod's id and folder |
| `TAILCHAT_MOD_STATE_DIR`, `TAILCHAT_MOD_CONFIG_DIR` | all | per-mod writable folders |
| `TAILCHAT_VERSION` | all | tailchat version |
| `TAILCHAT_COMMAND`, `TAILCHAT_ARGS`, `TAILCHAT_CONTEXT_JSON` | commands | command name, raw args, `{channel, nick, …}` |
| `TAILCHAT_EVENT`, `TAILCHAT_EVENT_JSON` | hooks | event name and JSON (also on **stdin**) |
| `TAILCHAT_CHANNEL` | commands, hooks | channel the command/event relates to |
| `TAILCHAT_PANEL_ID` | panel commands | full panel id |

## Widgets

Panel commands print widgets as JSON (an array, or `{"widgets": [...]}`);
plain text lines become `text` widgets.

```json
[
  {"type": "heading", "text": "Recent rolls"},
  {"type": "text", "text": "nothing yet", "color": "muted", "mono": false, "small": true},
  {"type": "kv", "key": "2d6", "value": "9"},
  {"type": "progress", "value": 0.6, "text": "bob · 6"},
  {"type": "list", "items": ["one", "two"]},
  {"type": "link", "text": "docs", "url": "https://…"},
  {"type": "separator"},
  {"type": "row", "children": [
    {"type": "button", "label": "d20", "command": "roll", "args": ["1d20"]},
    {"type": "button", "label": "Reset", "action": "reset"}
  ]}
]
```

`color` takes a theme token (`accent`, `muted`, `ok`, `warn`, `danger`,
`link`, `speaking`, `text`) or a hex colour, so mod panels follow the theme.
Buttons run one of the mod's commands (`command` + `args`) and/or emit a
`ui.action` event (`{"panel", "action", "value"}`) that hooks can handle.

## Events

`connection.changed`, `message.created`, `member.joined`, `member.left`,
`member.updated`, `voice.joined`, `voice.left`, `app.message`, `notice`,
`ui.action`, `ui.changed`, `hosting.changed`, `room.moved` (`{to, term}`: the
room changed host), `command.run`, `history.loaded` (`{channel, count}`:
history arrived from the host or was merged from your copy on join).
`connection.changed` with `state: "failed"` carries `refused: true` when the
host turned you away (join rules, full room, version).
Payloads are what `tailchat api events.subscribe` prints — the easiest way to
see them is to run that while you chat.

## Talking back

Everything goes through the control API ([API.md](API.md)):

```sh
$TAILCHAT_BIN api message.send text="hello"
$TAILCHAT_BIN api ui.panel.set '{"panel":"my.mod/main","widgets":[{"type":"text","text":"hi"}]}'
$TAILCHAT_BIN api events.subscribe '{"events":["message.*"]}'    # streams JSON lines
$TAILCHAT_BIN api app.send '{"ns":"my.mod","data":{"move":"e4"}}' # custom room-wide messages
```

`app.send` + an `app.message` hook is how mods build their own multiplayer
features (games, reactions, presence) without changing the server.

## Server-side mods

`tailchat serve` loads mods with `runs_on = ["server"]`. Their `send` output
is posted by the server, `TAILCHAT_NODE=server`, and the API they talk to is
the server's ([API.md](API.md#server-methods)). Use them for bots, welcome
messages, bridges, logging. The in-app host (`tailchat host`) doesn't run
server mods — client mods on the host machine cover that case.

## Examples

See [`mods/`](../mods): `dice` (command + panel), `greeter` (server hook),
`room-stats` (daemon + live panel), `sakura` (theme + layout only).
