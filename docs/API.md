# Control API

Every running Den — the app, the terminal client, a headless server —
listens on a random **loopback** port for newline-delimited JSON. It's how
mods, scripts and agents drive Den, and it exposes the same surface the
built-in UI uses.

## Using it

```sh
den api api.methods                 # what this instance supports
den api state.get
den api message.send text="hi"      # key=value params (values parsed as JSON when possible)
den api input.submit '{"text":"/roll 2d6"}'
den api events.subscribe            # stream events until ^C
den api --list                      # running instances
den api --server state.get          # pick the headless server when both run
```

`den api` finds an instance via `$DEN_API`/`$DEN_API_TOKEN`
(set for mods), else `<state>/instances/<pid>.json` (newest first; stale files
are pruned). Instance files are `0600` and contain the token. A profile named
on the command line (`-S <sandbox>`, `--home`) wins over `$DEN_API`.

## Wire format

```text
→ {"id":1,"token":"…","method":"message.send","params":{"text":"hi"}}
← {"id":1,"ok":true,"result":{"sent":true}}
← {"id":2,"ok":false,"error":"missing text"}
→ {"id":3,"token":"…","method":"events.subscribe","params":{"events":["message.*"]}}
← {"id":3,"ok":true,"result":{"subscribed":["message.*"]}}
← {"event":"message.created","data":{"message":{…}}}
```

One request per line; a connection can carry many. After `events.subscribe`
the connection streams events. The token is compared in constant time.

## Client methods (app / terminal client)

| Method | Params | |
| --- | --- | --- |
| `state.get` | | connection, room, members, channels, voice, focus, discovery; `hosting` (what you host) and `is_host` (you host the room you're in); `conn.refused` on a failed connection the host turned away; `latency_ms` and `via` (`direct` or `relay`: how the link to the host travels) |
| `messages.list` | `channel?, limit?` | recent messages (default: focused channel) |
| `message.send` | `text, channel?` | send plain text |
| `input.submit` | `text, channel?` | as if typed: slash commands work |
| `channel.focus` | `channel` | switch the focused text channel |
| `channel.create` | `name, kind?` | add a channel (`kind`: `text`, the default, or `voice`); anyone in the room may. Returns the `id` it asks for; the host adds `-2`… if it's taken. Everyone gets a `channels.updated` event |
| `nickname.set` | `nickname` | change your nickname (`nick.set` with `nick` still works) |
| `profile.picture.set` | `path \| data` | your profile picture, from an image file or base64 `data` (PNG, JPEG, GIF, WebP, BMP: cut to a square and made small). Returns `{picture}` (its id); it goes to the room you're in and every room you join |
| `profile.picture.clear` | | remove your profile picture |
| `room.picture.set` | `path \| data` | the picture of the room you're in; anyone in the room may. Everyone gets a `room.updated` event |
| `room.picture.clear` | | remove the room's picture |
| `picture.get` | `id, path?` | a picture file by id (`state.get` has them: `picture`, `members[].picture`, `profile_pictures`, `server.picture`): written to `path`, or returned as base64 `data`, with its `kind` |

| `voice.join` / `voice.leave` | `channel?` | |
| `voice.mute` / `voice.deafen` | `muted` / `deafened` | |
| `voice.stats` | | mic level (`input_level` RMS, `input_intensity` 0..1), speakers (`level`, `intensity`, `speaking`), devices, codec, `test` (from a mic test, not a call), `tx_packets` / `rx_packets` (counted since voice started; `open` mode sends even when silent) |
| `voice.devices` | | `inputs`, `outputs`, `default_input`, `default_output` |
| `voice.settings` | `input_device?, output_device?, mode?, vad_threshold?, input_gain?, output_volume?, ptt_key?` | change any of them (`null` or `""` device/key = default; `mode` is `voice_activity`, `push_to_talk` or `open`); returns the voice config. Applies live, restarting audio for a device change |
| `voice.test` | `on?, loopback?` | mic test outside a call (default `on: true`): `voice.stats` then reports the mic and whether the gate would open; `loopback` plays you back. Joining voice ends it |
| `app.send` | `ns, data, channel?, to?` | relay a mod-defined payload |
| `ui.panel.set` | `panel, widgets, title?` | create/update a panel ([widgets](MODS.md#widgets)) |
| `ui.panel.clear` | `panel` | |
| `ui.panels.list` | | |
| `ui.notify` | `text, level?` | toast (`info`, `warn`, `error`) |
| `ui.action` | `panel, action, value?` | emit a `ui.action` event |
| `ui.theme.set` / `ui.layout.set` | `name` | switch look; the app follows within a second |
| `ui.settings` | `open?` | open (default) or close the desktop app's settings page (GUI only) |
| `ui.raise` | | bring the app window to the front (GUI only; a second `den`/`den join` on the same profile uses it) |
| `ui.clipboard` | | `{copied: {text, ts}}`: what the app last copied (Copy invite link, Copy text). With `$DEN_CLIPBOARD=<file>`, or headless, copies go there instead of the system clipboard |
| `state.get` → `frontend` | | `"window"`, `"headless"`, or null (terminal client, `den serve`): whether the methods below work |

### Driving the interface

These go to the app's interface (a window, or `DEN_WINDOW=headless`), so they
fail with a clear error in `den chat` and `den serve`. They work while the
window is minimised or behind others, never move the real mouse or take
focus, and answer once the frame they caused is drawn. `den ui …` wraps them
with short output; see [AGENT-TESTING.md](AGENT-TESTING.md).

`[modules] agent_control = false` in `config.toml` refuses all of these,
plus `ui.clipboard` and `app.quit`. The rest of the API keeps working, and
`state.get` reports it as `agent_control`.

A **target** is `@12` (a ref from `ui.tree`; a ref stops matching when that
widget's text changes), text (exact, then any case, then a substring), or
`role:text` (`button:Join`). Add `panel` (`core.header`, `my.mod`) and/or
`index` (0-based) when several match; an ambiguous target fails and lists the
candidates.

| Method | Params | |
| --- | --- | --- |
| `ui.tree` | `panel?, all?, rects?, format?` | widgets on screen: `{nodes: [{ref, role, text, value, panel, toggled, flags, rect}], panels, window}`. `flags` lists any of `focused`, `disabled`, `selected`, `hidden`, `clickable`; `rect` is `[x, y, w, h]` in points; `toggled` is null for widgets that don't toggle. `format: "text"` gives `{text}`, one line per widget grouped by panel. Scrolled-away widgets only with `all` |
| `ui.click` | `target \| x,y, panel?, index?, button?, double?, pointer?` | click: the widget's accessibility action, or a pointer click at its centre (`pointer`, `button: "right"`, `double`, or a point). Pointer actions (`click`, `hover`, `scroll`, `type` into a target) scroll a target that's out of view into view first |
| `ui.type` | `text, target?, enter?` | type into the focused text box, or focus `target` first; `enter` presses Enter after |
| `ui.key` | `key` | a key or chord: `Enter`, `Escape`, `Tab`, `ctrl+a`, `cmd+k`, `shift+F2` (`cmd` is Ctrl off macOS) |
| `ui.scroll` | `dy?, dx?, target? \| x,y` | scroll by points (dy > 0 scrolls down) over a widget or point |
| `ui.hover` | `target \| x,y` | move the pointer there; answers after the tooltip delay |
| `ui.wait` | `target, gone?, timeout_ms?, panel?` | until a widget matching `target` is on screen (or, with `gone`, isn't): `{found, after_ms}`; an error on timeout (default 10 s, at most an hour) |
| `ui.screenshot` | `path?, panel?, target?, scale?, renderer?` | PNG of the window, a panel or a widget: `{path, width, height, renderer}`. A visible window is captured from the GPU; a hidden, minimised or headless one is drawn on the CPU (`renderer: "cpu"` forces that). Default path: `<state>/screenshots/<ms>.png` |
| `ui.window` | `size?, pos?, minimized?, maximized?, fullscreen?, visible?, focus?` | window state (`size`, `pos`, `title`, `pixels_per_point`, `zoom`, flags, `renderer`); params change it first. `size`/`pos` as `"800x600"` / `"40,40"` or `[w, h]` |
| `ui.scale` | `scale` | UI zoom, like display scaling (1.0 = 100 %) |
| `app.quit` | | close like the close button (hands the room on first) |
| `mods.list` / `mods.reload` | | |
| `mods.run` | `command, args?` | run a mod command |
| `session.host` | `name?, open?` | host a new room (invite-only unless `open`) |
| `session.join` | `addr` | an invite link, or a room's address (uses the saved invite) |
| `session.invite` | | `{invite}`: the invite link for the room you're in |
| `session.leave` | | leave; if you host, another member takes the room over |
| `session.handoff` | `to?` | while hosting: hand the room to member `to` (default: the best placed member who can host, by `reach`) |
| `rooms.list` | | rooms saved on this machine |
| `rooms.open` | `room` | id or name: join its live host, or host it from your copy ("Host this room") |
| `rooms.forget` | `room` | delete a saved room and its history |
| `net.status` | | this app's endpoint: `id`, `short`, `relay`, `direct` addresses, `reach` (`strict`/`unknown`/`nat`/`public`: how easily others could connect to it; see ARCHITECTURE.md) |
| `rooms.discover` | | look for live hosts of your saved rooms; results in `state.get` → `discovery` (`addr` is the link to join; show `host` to people) |
| `config.get` | | |
| `api.methods`, `api.ping`, `events.subscribe` | | built in everywhere |

## Server methods

`den serve` exposes: `state.get` (includes the room save), `members.list`, `channels.list`,
`messages.list`, `message.send` (as the server), `notice.send`,
`member.kick {id, reason?}`, `app.send`, `motd.set`, `mods.list`,
`mods.reload`.

## Events

See [MODS.md](MODS.md#events). Filters accept exact names, `prefix.*`, or `*`.
