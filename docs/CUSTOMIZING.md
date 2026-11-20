# Customising the app

Everything here is plain TOML in your config folder and hot-reloads on save.

```sh
den config path   # where things live
den config init   # write editable copies of the built-in themes/layouts
```

Pick themes and layouts from **View** in the app, or set them in
`config.toml`:

```toml
[ui]
theme = "catppuccin"   # built-in: catppuccin, catppuccin-latte, tokyo-night, rose-pine, rose-pine-dawn, gruvbox, studio
layout = "default"     # built-in: default (three columns), messenger (chat bubbles), console (a rack, for studio)
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
# canvas = "#16161e"      # behind the cards (optional: derived from panel)

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

### Material (skeuomorphic themes)

Themes are flat unless they have a `[material]` table. With one, Den draws
things the way light falls on real parts: cards become bevelled plates on a
textured canvas, the chat sits in a recessed screen, the current channel
and text fields are pressed in, counts show on little LCDs, lit things glow,
your voice card is a brushed-metal strip with lever toggles and an LED
meter for your mic, and section labels are engraved. The built-in `studio`
theme (with the `console` layout) shows it off.

```toml
[material]
texture = "brushed"       # canvas texture: brushed | grain | none
texture_strength = 0.75   # 0..1
bevel = 0.9               # lit top edges, shaded bottoms (0..1)
inset = 0.9               # depth of wells: chat screen, fields, current channel (0..1)
glow = 0.65               # backlights and LEDs (0..1)
engrave = true            # engraved uppercase section labels
screws = true             # screws on your voice card
```

Every key is optional (an empty `[material]` table turns it on with these
defaults). It uses the same colour tokens: `canvas` is the chassis, `panel`
the plates, `bg` the chat screen, `surface` the caps and keys, `accent` the
backlight and `ok` / `warn` / `danger` the LEDs. Mod panels sit on the same
plates and use the same colours; built-in widgets mods reach through the
kit (section labels, for one) are drawn the material way too.

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
| `core.channels` | text channels with unread / mention badges, voice channels with who's in them, the voice card for your channel; `+` on each section adds a channel | `style = "list" \| "conversations"` (previews), `voice` (true), `create` (true: the `+` buttons), `orientation = "horizontal"` (a tab strip for top/bottom slots) |
| `core.voice` | you (your avatar's ring follows how loud you are), mic, deafen, hang up; the gear opens the settings page | — |
| `core.settings` | the settings page: microphone and headset, input volume, mic test with a sensitivity marker and "hear myself", input mode (voice activity / push to talk with a rebindable key / always on), output volume, theme and layout. The app shows it over everything (gear, room menu, `[keys] settings`, `den api ui.settings`); hide it with *View → Panels* to switch it off | — |
| `core.members` | people with status lines, room key (hover), voice state | `group_voice` (true: fold your voice channel into one row) |
| `core.messages` | message list with day and "new" dividers | `style = "cozy" \| "bubbles" \| "compact" \| "irc"`, `row = ["time","author","text"]` (compact), `time_format`, `group`, `show_system`, `header` |
| `core.stage` | voice channels as speaker tiles with join/leave | `tile_size` (84), `show_empty` (true) |
| `core.composer` | input with history (↑), `/command` completion (Tab) and a send button | `placeholder` (`{channel}` is substituted), `style = "field" \| "pill"`, `send_button` (true) |
| `core.status` | connection, latency, hosting, voice codec, theme/layout | — |
| `core.toasts` | pop-up notices (use as an overlay) | — |

Mod panels are `<mod id>/<panel id>`, e.g. `example.dice/history`.

### Keys

```toml
[keys]
ptt = "F8"               # hold to talk in push-to-talk mode (Mouse4 / Mouse5 work too)
mute = "Ctrl+Shift+M"    # Ctrl = Cmd on macOS
deafen = "Ctrl+Shift+D"
reload = "Ctrl+Shift+R"  # reload theme + layout
settings = "Ctrl+Comma"  # open / close the settings page
```

A push-to-talk key picked in Settings is saved as `[voice] ptt_key` in
`config.toml` and wins over the layout's.

### Talking indicators

Everyone talking gets a ring around their avatar (voice card, people list,
stage, your own card). It is always clearly visible while someone talks and
grows thicker, with a glow, the louder they are. Loudness is measured in dB
on what you actually receive, i.e. after the speaker's own input volume, so
each person's ring follows how loud their settings make them
(`denchat_core::loudness`). Colour: the theme's `speaking` token.

## Scripting the look

```sh
den api ui.theme.set name=rose-pine-dawn
den api ui.layout.set name=messenger
den ui shot /tmp/look.png
```

To try a look without touching the running app, start a sandbox from your
own setup: `den sandbox start t --copy-config --theme rose-pine-dawn`, then
`den -S t ui shot /tmp/t.png` ([AGENT-TESTING.md](AGENT-TESTING.md)).
`DEN_SCREENSHOT=out.png DEN_SCREENSHOT_EXIT=1 den` renders the lobby,
saves a PNG and quits (`DEN_SCREENSHOT_DELAY_MS` to wait longer; add
`DEN_WINDOW=headless` for no window).

## Going further

* Ship a theme or layout to friends as a mod: `themes = ["x.toml"]`,
  `layouts = ["y.toml"]` in a manifest (see `mods/sakura`).
* Write a Rust panel: implement `denchat_gui::Panel`, register it on a
  `Registry`, call `denchat_gui::run_with(session, registry)`.
* Replace the GUI entirely: see "Writing another frontend" in
  [ARCHITECTURE.md](ARCHITECTURE.md).
