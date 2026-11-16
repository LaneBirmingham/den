# tailchat

Minimal voice + text chat for people on the same [Tailscale](https://tailscale.com)
tailnet. One Rust binary, three ways to run it:

| You want to… | Run |
| --- | --- |
| Hang out with friends, one of you hosts | `tailchat host` (others: `tailchat join <your-machine>`) |
| A room that's always up | `tailchat serve` on a server, headless, with an admin console |
| Chat from a terminal / over SSH | `tailchat chat <machine>` |

Tailscale does the hard parts: addressing (`join lanes-mac-mini`), encryption
(WireGuard), NAT traversal, and identity — the room checks every connection
with `tailscale whois`, so members show up as who they really are.

The app is deliberately bare. Every visible piece of the UI is a removable
panel placed by a TOML **layout**, colours and shapes come from a TOML
**theme**, and **mods** (any language) add commands, hooks, panels, themes and
layouts. Everything the UI can do is also on a local **control API**, so
scripts and AI agents can drive and reshape the app without touching Rust.

```
 ╭ core.header ──╮ ╭ core.messages ──────────────────╮ ╭ core.members ──╮
 │ ▣ lane's room │ │ # general                       │ │ People      4  │
 ╰───────────────╯ │ ◉ bob  7:04 PM                  │ │ ◉ bob  Online  │
  core.channels    │   hey lane                      │ │ ◉ amy  In lou… │
  # general    2   │ ◉ lane 7:04 PM                  │ ├ mods:* ────────┤
  🔊 lounge  ◉◉    │   rolled 2d6+1 = 6              │ │ Dice           │
 ╭ core.voice ───╮ │ ╭ core.composer ──────────────╮ │ │                │
 │ ◉ lane  🎙 🎧 │ │ │ Message #general         ⬆ │ │ │                │
 ╰───────────────╯ ╰─────────────────────────────────╯ ╰────────────────╯
```

## Quick start

```sh
cargo build --release              # GUI + voice (default features)
./target/release/tailchat          # opens the lobby: host, join, or pick a discovered room
```

Friends on your tailnet run `tailchat join <your-machine-name>` (or click your
room under *On your tailnet* in the lobby). Default port is **8245** (TCP for
chat, UDP for voice).

### Persistent server

```sh
cargo build --release -p tailchat --no-default-features   # headless: no GUI/audio deps
tailchat serve --name "The Den" --channel general --channel lounge:voice --require-tailnet
```

`serve` binds to the machine's tailnet IP, keeps history on disk, runs
server-side mods, and gives you an admin console (`help`, `who`, `say`,
`kick`, `motd`, `reload`, …). Settings live in `server.toml`
(`tailchat serve --write-config` writes one). See [`deploy/`](deploy) for
systemd and Docker (Tailscale sidecar) setups.

### All commands

```
tailchat [gui]                   desktop app
tailchat host [--name --bind --port --persist] [--cli]
tailchat join <addr> [--cli]
tailchat chat <addr> | --host    terminal client
tailchat serve [...]             headless room + admin console
tailchat discover [--json]       find rooms on your tailnet
tailchat api <method> [params]   drive a running instance (see docs/API.md)
tailchat mods list|new|link|unlink|enable|disable|path
tailchat config path|init        where files live / write editable defaults
tailchat devices | voice-test    audio checks
```

Global flags: `--home <dir>` (or `TAILCHAT_HOME`) keeps config/data/state in
one folder — handy for running several instances on one machine.

## Customising

* **Theme / layout:** *View* menu in the app, or `config.toml` → `[ui]`.
  `tailchat config init` exports the built-ins to your config folder; edit
  and save — the app hot-reloads. → [docs/CUSTOMIZING.md](docs/CUSTOMIZING.md)
* **Mods:** `tailchat mods new my.mod` scaffolds one. Examples in
  [`mods/`](mods). → [docs/MODS.md](docs/MODS.md)
* **Turn things off:** delete a panel from the layout, or switch whole
  modules off in `config.toml` → `[modules]` (`voice`, `mods`, `api`,
  `discovery`, `slash_commands`).
* **Using an AI agent:** point it at [AGENTS.md](AGENTS.md). It can edit
  themes/layouts/mods, apply them live, and screenshot the result
  (`tailchat api ui.screenshot`).

## Platforms

macOS, Linux and Windows. Voice uses the system audio stack via `cpal`
(CoreAudio / ALSA / WASAPI) and Opus (built from source; needs `cmake`).
Linux builds need `libasound2-dev` and the usual X11/Wayland dev packages for
the GUI; the headless build (`--no-default-features`) needs none of them.

| Feature | Default | What it pulls in |
| --- | --- | --- |
| `gui` | on | egui/eframe desktop shell (`tailchat-gui`) |
| `voice` | on | cpal + Opus (`tailchat-audio`) |

## Layout of the repo

```
crates/
  tailchat-proto   wire protocol (TCP JSON frames + UDP voice packets)
  tailchat-audio   capture, Opus, jitter buffers, mixing, VAD
  tailchat-core    server, session, Tailscale, mods, control API, UI store
  tailchat-gui     egui shell: layouts, themes, panel registry
  tailchat         the binary (app, terminal client, serve, CLI tools)
assets/            built-in themes and layouts (compiled in)
mods/              example mods
deploy/            systemd, Docker, compose
docs/              architecture, customising, mods, API
```

More: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Status

v0.1. Works: text channels with history, voice channels (Opus, SFU relay
through the host), Tailscale identity, discovery, reconnect, mods, control
API, themes/layouts with hot reload. Not yet: encryption beyond what
WireGuard provides (rooms trust the tailnet), screen share, file transfer,
mobile clients, handing a room to another member when its host leaves
(see [docs/ROADMAP.md](docs/ROADMAP.md)).
