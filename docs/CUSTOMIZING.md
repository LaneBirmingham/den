# Customising the app

Everything here is plain TOML in your config folder and hot-reloads on save.

```sh
tailchat config path   # where things live
tailchat config init   # write editable copies of the built-in themes/layouts
```

Pick themes and layouts from **View** in the app, or set them in
`config.toml`:

```toml
[ui]
theme = "catppuccin"   # built-in: catppuccin, catppuccin-latte, tokyo-night, rose-pine, rose-pine-dawn, gruvbox
layout = "default"     # built-in: default (three columns), messenger (chat bubbles)
hot_reload = true
scale = 1.0            # UI zoom
```

Lookup order for a name: built-in → mods → `<config>/themes|layouts/<name>.toml`.
A user file with the same name as a built-in replaces it.

## Themes

```toml
name = "tokyo-night"
dark = true

[colors]
bg = "#1a1b26"            # chat area
panel = "#16161e"         # side/top/bottom slots
surface = "#24283b"       # inputs, buttons, cards
surface_hover = "#292e42"
border = "#292e42"
text = "#c0caf5"
muted = "#737aa2"
accent = "#7aa2f7"
accent_text = "#16161e"
ok = "#9ece6a"
warn = "#e0af68"
danger = "#f7768e"
speaking = "#9ece6a"
link = "#7dcfff"
mention = "#2a3150"       # background of messages that mention you

[shape]
rounding = 6
spacing = 6
padding = 10
border_width = 1.0

[text]
size = 14.0
heading_size = 17.0
small_size = 11.5
# font = "fonts/Inter.ttf"        # relative to this file
# mono_font = "fonts/JetBrainsMono.ttf"
```

Every key is optional; missing ones fall back to the defaults above, and any
colour can also be referenced by name from mod widgets (`"color": "accent"`).
Set `monospace = true` under `[text]` to use the monospace font everywhere. A
broken file keeps the previous theme on screen and shows the error.

## Layouts

A layout has two **screens** — `lobby` (not in a room) and `room` — each
with up to five **slots** plus floating **windows** and **overlays**.

```toml
name = "mine"
description = "shown as a tooltip in View → Layout"

[lobby]
center = { panels = ["core.lobby"] }
overlays = ["core.toasts"]

[shell]
style = "cards"                  # floating cards on a canvas (default), or "flat"
gap = 10                         # space around and between cards

[room]
left   = { panels = ["core.header", "core.channels", "core.voice"], size = 250, card = false }
right  = { panels = ["core.members", "mods:*"], size = 240, mode = "tabs" }
center = { panels = ["core.messages", "core.composer"] }
bottom = { panels = ["core.status"] }
overlays = ["core.toasts"]

[[room.windows]]                 # floating, closable
panel = "example.dice/history"
open = true
width = 260
```

Slot options: `panels` (in order), `size` (px), `resizable`, `mode`
(`stack` or `tabs`), `visible`, `card` (in the cards style, `false` leaves
the slot's panels on the canvas; the header pill and voice card then draw
their own cards). In a stack the first panel that *fills*
(messages, lobby) takes the leftover space; panels after it hug the bottom.

**Placeholders:** `"mods:right"` expands to every mod panel that asked for
the right slot; `"mods:*"` to every mod panel not placed elsewhere. Leave
them out and mod panels only appear where you name them explicitly.

**Removing things:** delete the id. No `core.voice` → no voice controls; no
`core.header` → no header. Users can also hide placed panels at runtime with
*View → Panels* (remembered per machine).

### Built-in panels and their settings

Settings go in `[panel."<id>"]`. Every panel also accepts `fill = true|false`.

| Panel | What | Settings |
| --- | --- | --- |
| `core.lobby` | host / join / discovered rooms | — |
| `core.header` | room name, online count; click for invite, View menu, leave | `style = "pill"` (a card for a sidebar) \| `"bar"` (a strip for the top slot, default) |
| `core.channels` | text channels with unread / mention badges, voice channels with who's in them, the voice card for your channel | `style = "list" \| "conversations"` (previews), `voice` (true), `orientation = "horizontal"` (a tab strip for top/bottom slots) |
| `core.voice` | you: mic, deafen, hang up, settings (mode, meter, gate, volume, devices) | `show_devices`, `show_meter` |
| `core.members` | people with status lines, Tailscale identity, voice state | `group_voice` (true: fold your voice channel into one row) |
| `core.messages` | message list with day and "new" dividers | `style = "cozy" \| "bubbles" \| "compact" \| "irc"`, `row = ["time","author","text"]` (compact), `time_format`, `group`, `show_system`, `header` |
| `core.stage` | voice channels as speaker tiles with join/leave | `tile_size` (84), `show_empty` (true) |
| `core.composer` | input with history (↑), `/command` completion (Tab) and a send button | `placeholder` (`{channel}` is substituted), `style = "field" \| "pill"`, `send_button` (true) |
| `core.status` | connection, latency, hosting, voice codec, theme/layout | — |
| `core.toasts` | pop-up notices (use as an overlay) | — |

Mod panels are `<mod id>/<panel id>`, e.g. `example.dice/history`.

### Keys

```toml
[keys]
ptt = "F8"               # hold to talk in push-to-talk mode
mute = "Ctrl+Shift+M"    # Ctrl = Cmd on macOS
deafen = "Ctrl+Shift+D"
reload = "Ctrl+Shift+R"  # reload theme + layout
```

## Scripting the look

```sh
tailchat api ui.theme.set name=rose-pine-dawn
tailchat api ui.layout.set name=messenger
tailchat api ui.screenshot path=/tmp/look.png
```

`TAILCHAT_SCREENSHOT=out.png TAILCHAT_SCREENSHOT_EXIT=1 tailchat` renders the
lobby, saves a PNG and quits (`TAILCHAT_SCREENSHOT_DELAY_MS` to wait longer).

## Going further

* Ship a theme or layout to friends as a mod: `themes = ["x.toml"]`,
  `layouts = ["y.toml"]` in a manifest (see `mods/sakura`).
* Write a Rust panel: implement `tailchat_gui::Panel`, register it on a
  `Registry`, call `tailchat_gui::run_with(session, registry)`.
* Replace the GUI entirely: see "Writing another frontend" in
  [ARCHITECTURE.md](ARCHITECTURE.md).
