# Den

Minimal peer-to-peer voice + text chat. No accounts, no service to sign up
for, nothing to install but the app. One Rust binary, three ways to run it:

| You want to… | Run |
| --- | --- |
| Hang out with friends, one of you hosts | `den host`, then share the invite link (others: `den join '<link>'`) |
| A room that's always up | `den serve` on a server, headless, with an admin console |
| Keep a room going when its host leaves | nothing: everyone keeps a copy, and anyone can host it next |
| Chat from a terminal / over SSH | `den chat '<link>'` |

[iroh](https://iroh.computer) does the networking: every app is a QUIC
endpoint named by a public key, connections are end-to-end encrypted, and NAT
hole-punching gets most peers a direct connection. Peers that can't reach
each other directly (strict NATs, some mobile networks) go through a relay,
which only ever sees encrypted traffic:

* **Rooms with a `serve`** use the server's own relay (built in, same port
  number over TCP). Nothing from a third party.
* **Rooms hosted from an app** use n0's free public relays by default, or
  your own: run `den relay` on any machine with a forwarded port and
  put the URL it prints (token included) in the host's `[net] relays`. Members need no setup either way:
  the invite link says which relay to use.

A relay has to be reachable from outside (a forwarded port or a public IP);
that's what lets two peers behind routers find each other. Your own relays
aren't open to the internet: each has a token, carried in invite links, and
admits only apps that present it.

**Port forwarding.** If your router offers it (UPnP, NAT-PMP or PCP), the app
asks it to forward one UDP port while it runs, so others can connect straight
in. Only Den listens there, and rooms still need their invite key. To
never open ports on your router, set `port_mapping = false` under `[net]` in
`config.toml` (or `server.toml`); peers then hole-punch or use the relay.

**Identity is per room.** There are no global profiles. The first time you
join a room, your app makes a key for that room and keeps it; the room
remembers the public half next to your nick. Signing in is signing a
challenge with that key, so nobody else can show up as you in that room, and
nothing links your profiles across rooms.

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
./target/release/den          # opens the lobby: host, join, or reopen a saved room
```

Host a room, then *room menu → Copy invite link* and send it to your friends;
they paste it into *Join*. A link looks like
`den://<endpoint-id>?room=<id>&invite=<key>&relay=…&ip=…`.

**The invite link is the key to the room.** New rooms are invite-only: the
link carries the room's invite key, and the host admits only apps that prove
they have it. Your app saves it with the room, so you can come back without
the link, and any member can copy it again. Tick *Open room* (or
`den host --open`) for a room anyone with the address can join.

### Rooms belong to everyone in them

A room works like a shared save game. Everyone who joins keeps a copy
(settings and the last 500 messages per channel, under `<data>/rooms/<id>/`),
and their app swaps message ids with the host on every join, so all the
copies fill each other's gaps. That means:

* **The host can leave.** *Leave* (or closing the app, or stopping it with
  Ctrl-C / SIGTERM) hands the room to another member; everyone's app
  follows automatically. *Hand off to…* in the room menu does the same on
  purpose. The header says "you host" while the room runs on your machine.
* **The host can vanish.** If the host drops, anyone can click
  **Host this room** and carry on from their copy; the others find it by
  asking every member's app they know of. Open a room later from *Your rooms* in the lobby (or
  `den host --room <name>`): it joins whoever hosts it now, or hosts
  it from your copy.
* **The best placed member hosts next.** Apps measure how easily others can
  reach them, and the host double-checks by connecting to each member
  directly. A handoff goes to whoever others can reach most easily (they
  need no relay to host), and when the host has vanished, *Host this room*
  says if someone else is better placed. The lobby shows your own network
  (*behind a router*, *strict network*, …).
* Everyone in a room has the same rights: anyone can host it. The invite
  key, members' profiles (key → nick) and join rules (`allow`) travel with
  the room. If a host refuses you, there's no *Host this room*: the host is
  up, and would refuse you again.

### Persistent server

```sh
cargo build --release -p denchat --no-default-features   # headless: no GUI/audio deps
den serve --name "The Den" --channel general --channel lounge:voice
```

`serve` prints the room's invite link, saves the room like any member would,
runs server-side mods, and gives you an admin console (`help`, `who`, `say`,
`kick`, `motd`, `reload`, …). It listens on **UDP 8245** (the room) and
**TCP 8245** (its relay); forward both on your router. It works out its
public IP at start and puts its relay in the invite link; on a dynamic IP,
give it a DNS name instead: `--relay-url box.example.com:8245` (or
`[relay] url` in `server.toml`).
Settings live in `server.toml` (`den serve --write-config` writes one).
See [`deploy/`](deploy) for systemd and Docker setups.

A server is just the room's always-on host. Stopping it (Ctrl-C, SIGTERM,
`quit`) hands the room to a member whose app can host, so nobody is
stranded. If it was down and someone else hosted the room meanwhile, it
takes the room back when it starts, and the members' copies fill in what it
missed.

Join rules are saved with the room, so `--allow` sticks across restarts.
`--allow <key>` (repeatable; the first 8+ characters of a member's key, see
`who`) admits only those members, on top of the invite; it replaces the
saved list, and `--open` clears it. To move a room you started in the app
onto a server: `den serve --adopt '<invite link>'` (or copy
`<data>/rooms/<id>/` over and run `den serve --room <id>`).

### All commands

```
den [gui]                   desktop app
den host [--name | --room <saved>] [--open] [--cli]
den join <link> [--cli]
den chat <link> | --host    terminal client
den serve [--room <saved> | --adopt <link>] [...]   always-on host + admin console + relay
den relay [--port 3340]     a relay on its own, for rooms hosted from apps
den discover [--json]       which of your saved rooms are live
den api <method> [params]   drive a running instance (see docs/API.md)
den ui tree|click|type|key|scroll|hover|wait|shot|window|scale|clip|quit
                            use the app in the background, like a person would (agents, tests)
den status | den wait <event> [--where k=v] [-- cmd]   state summary / block until an event
den sandbox start|list|stop|path   throwaway headless Dens for testing mods and rooms
den doctor [--json]         network self-check: reach, port mapping, relays, firewall
den mods list|new|link|unlink|enable|disable|path
den config path|init        where files live / write editable defaults
den devices | voice-test    audio checks
```

Global flags: `--home <dir>` (or `DEN_HOME`) keeps config/data/state in
one folder — handy for running several instances on one machine.
`-S <name>` does the same for a `den sandbox`. Before the command,
`--window headless` (no window at all), `--size WxH` and `--scale 1.5`
change how the app shows itself for that run.

In the chat box, `/invite` prints the room's invite link and `/whoami` your
key for the room.

## Customising

* **Theme / layout:** *View* menu in the app, or `config.toml` → `[ui]`.
  `den config init` exports the built-ins to your config folder; edit
  and save — the app hot-reloads. → [docs/CUSTOMIZING.md](docs/CUSTOMIZING.md)
* **Mods:** `den mods new --id my.mod` scaffolds one. Examples in
  [`mods/`](mods). → [docs/MODS.md](docs/MODS.md)
* **Turn things off:** delete a panel from the layout, or switch whole
  modules off in `config.toml` → `[modules]` (`voice`, `mods`, `api`,
  `discovery`, `slash_commands`, `agent_control`).
* **Using an AI agent:** point it at [AGENTS.md](AGENTS.md). It can edit
  themes/layouts/mods, try them in a headless sandbox, click through them
  and screenshot the result, then apply them live
  ([docs/AGENT-TESTING.md](docs/AGENT-TESTING.md)).

## Platforms

macOS, Linux and Windows. Voice uses the system audio stack via `cpal`
(CoreAudio / ALSA / WASAPI) and Opus (built from source; needs `cmake`).
Linux builds need `libasound2-dev` and the usual X11/Wayland dev packages for
the GUI; the headless build (`--no-default-features`) needs none of them.

| Feature | Default | What it pulls in |
| --- | --- | --- |
| `gui` | on | egui/eframe desktop shell (`denchat-gui`) |
| `voice` | on | cpal + Opus (`denchat-audio`) |

## Layout of the repo

```
crates/
  denchat-proto   wire protocol (JSON frames on a QUIC stream + voice datagrams)
  denchat-audio   capture, Opus, jitter buffers, mixing, VAD
  denchat-core    server, session, P2P transport (iroh), mods, control API, UI store
  denchat-gui     egui shell: layouts, themes, panel registry
  den         the binary (app, terminal client, serve, CLI tools)
assets/            built-in themes and layouts (compiled in)
mods/              example mods
deploy/            systemd, Docker, compose
docs/              architecture, customising, mods, API
```

More: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Status

v0.1. Works: text channels with history, voice channels (Opus, SFU relay
through the host), P2P transport with relay fallback, invite links,
per-room keys, reconnect, mods, control API, themes/layouts with hot reload,
saved rooms that any member can host (planned handoff, manual takeover,
servers that claim their room back), a relay built into `serve`. Not yet:
signed messages (today the host relays what members say, and a malicious
host could alter it), screen share, file transfer, mobile clients, automatic
takeover when a host vanishes (see [docs/ROADMAP.md](docs/ROADMAP.md)).
