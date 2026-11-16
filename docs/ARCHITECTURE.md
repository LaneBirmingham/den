Exit code 1
# Architecture

```
            ┌──────────── frontends (swappable) ────────────┐
            │ egui shell   terminal client   your TUI/web/… │
            └───────┬───────────────┬──────────────┬────────┘
                    │ Rust calls    │              │ control API (JSON lines, loopback)
                    ▼               ▼              ▼
              ┌──────────────── Session (tailchat-core) ────────────────┐
              │ state · methods · events · UiStore · ModRuntime · voice │
              └───────┬─────────────────────────────────────┬───────────┘
                      │ TCP frames + UDP voice              │ spawns
                      ▼                                     ▼
         ┌──── room server (same code) ────┐          mod processes
         │ in-process (`host`) or headless │          (any language)
         │ (`serve` → ServerNode + mods)   │
         └─────────────────────────────────┘
```

## Crates

| Crate | Role | Depends on |
| --- | --- | --- |
| `tailchat-proto` | Message types, framing, voice packet format, codec negotiation | serde |
| `tailchat-audio` | Devices, resampling, Opus/PCM codecs, VAD gate, jitter buffer + mixer | cpal, opus |
| `tailchat-core` | Room server, client `Session`, Tailscale CLI wrapper, mods, control API, UI store | proto, tokio, (audio) |
| `tailchat-gui` | egui shell: layouts, themes, panel registry, built-in panels | core, eframe |
| `tailchat` | The binary and its subcommands | core, (gui) |

The GUI and audio are optional features, so a server build is just
`tokio` + `serde` and cross-compiles anywhere.

## The two modes are one server

`tailchat host` and `tailchat serve` run the same `server::start`. Hosting
embeds it in the app and immediately joins it; `serve` wraps it in a
`ServerNode` that adds server-side mods, an admin console and its own
control API. Both bind to the machine's **tailnet IPv4** by default, so a room
is only reachable from your tailnet.

## Wire protocol (`tailchat-proto`)

* **TCP** — `u32` big-endian length + JSON body. `ClientMsg` / `ServerMsg`
  are tagged enums (`{"type": "send", …}`). A connection either sends
  `probe` (discovery: get room info, close) or `hello` (version, nick,
  codecs, features) and receives `welcome` with members, channels and the
  voice setup (UDP port + per-member token), followed by history.
* **UDP** (same port) — binary voice packets: `REGISTER` (token keepalive),
  `AUDIO` (client → server: token, seq, codec, payload), `RELAY` (server →
  client: speaker id, seq, codec, payload), `ACK`. The server is a selective
  forwarding unit: it never decodes audio, it checks the token *and* the
  source IP, drops muted/deafened senders, and relays to others in the same
  voice channel.
* **Codecs** — 48 kHz mono, 20 ms frames. Opus by default, raw PCM16 as a
  fallback for builds without Opus. Each client advertises what it decodes;
  senders pick the best codec every listener supports.
* **Identity** — the server runs `tailscale whois` on each peer IP (cached
  5 min). Members carry `identity: {login, display_name, node, verified}`.
  `require_tailnet` rejects unverified peers; `allow` restricts to logins.
* **App messages** — `ClientMsg::App {ns, data, channel?, to?}` lets mods
  define their own protocols (games, presence, reactions) without server
  changes; the server relays them untouched.

## Session: the only thing a frontend needs

`tailchat_core::session::Session` is UI-agnostic:

1. **Read** `session.state()` → `SessionState` (connection, server info,
   channels, members, messages, focus, unread + mentions + "new since", voice
   (incl. when you joined), discovery, tailnet).
2. **Call** methods: `host`, `join`, `leave`, `submit` (slash commands +
   text), `send_message`, `focus_channel`, `voice_join`, `set_muted`,
   `set_voice_devices`, `run_mod_command`, `ui_action`, …
3. **Listen**: `on_change(f)` for repaint, `subscribe()` for named events
   (`message.created`, `member.joined`, `voice.joined`, `ui.action`, …).

The control API exposes the same three things to other processes
([API.md](API.md)), so a frontend in another language — or an agent — is a
peer of the built-in one, not a second-class citizen.

Connection handling lives here too: epoch-tagged connection tasks, reconnect
with backoff, voice auto-rejoin, latency pings, recent addresses.

## UI store: declarative panels

`UiStore` holds panels (`id → title, slot hint, owner, widgets`) and toasts.
Mods fill panels with widgets (`heading`, `text`, `kv`, `list`, `progress`,
`button`, `link`, `row`, `separator`) via panel commands or `ui.panel.set`.
Frontends render widgets however suits them; mods never draw pixels, so the
same mod works in the GUI, a future TUI, or a web view.

## The egui shell (`tailchat-gui`)

* `Registry` maps panel ids to constructors. All built-ins (`core.*`) are
  registered exactly like a third-party Rust panel would be; `run_with`
  accepts a custom registry.
* `Layout` (TOML) assigns panel ids to slots (`top/bottom/left/right/center`),
  windows and overlays, per screen (`lobby`, `room`). `mods:<slot>` and
  `mods:*` expand to mod panels.
* `Theme` (TOML) → egui visuals, spacing, fonts. Panels read colour tokens
  from a `Palette`, never hard-coded colours.
* `Catalog` finds themes/layouts: built-in < mods < user files.
* Panels get a read-only `PanelCx` and queue intents on `Actions`; the shell
  applies them after the frame (no locks held while user code runs).
* Hot reload polls watched files; `ui.theme.set` / `ui.layout.set` /
  `ui.screenshot` make the shell scriptable.

## Mods

A mod is a folder with `tailchat-mod.toml`. tailchat runs its commands as
child processes with `TAILCHAT_*` env vars, feeds hook events on stdin, and
routes stdout (`send` to chat, `notice` as a toast, or `none`). Mods call
back through `$TAILCHAT_BIN api …`. Mods can ship themes and layouts.
`runs_on` decides client vs server. See [MODS.md](MODS.md).

## Writing another frontend

Either link `tailchat-core` and drive a `Session` directly (Rust), or start
any tailchat instance and use the control API from any language:
`state.get` + `events.subscribe` to render, `input.submit` / `voice.*` /
`ui.action` to act. `tailchat chat` (≈150 lines, `crates/tailchat/src/term.rs`)
is a complete example.
