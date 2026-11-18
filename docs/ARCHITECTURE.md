Exit code 1
# Architecture

```
            ┌──────────── frontends (swappable) ────────────┐
            │ egui shell   terminal client   your TUI/web/… │
            └───────┬───────────────┬──────────────┬────────┘
                    │ Rust calls    │              │ control API (JSON lines, loopback)
                    ▼               ▼              ▼
              ┌──────────────── Session (denchat-core) ────────────────┐
              │ state · methods · events · UiStore · ModRuntime · voice │
              └───────┬─────────────────────────────────────┬───────────┘
                      │ iroh QUIC: frames + voice datagrams │ spawns
                      ▼                                     ▼
         ┌──── room server (same code) ────┐          mod processes
         │ in-process (`host`) or headless │          (any language)
         │ (`serve` → ServerNode + mods)   │
         └─────────────────────────────────┘
```

## Crates

| Crate | Role | Depends on |
| --- | --- | --- |
| `denchat-proto` | Message types, framing, voice packet format, codec negotiation | serde |
| `denchat-audio` | Devices, resampling, Opus/PCM codecs, VAD gate, jitter buffer + mixer | cpal, opus |
| `denchat-core` | Room server, client `Session`, P2P transport (`net`, iroh), mods, control API, UI store | proto, tokio, iroh, (audio) |
| `denchat-gui` | egui shell: layouts, themes, panel registry, built-in panels | core, eframe |
| `den` | The binary and its subcommands | core, (gui) |

The GUI and audio are optional features, so a server build is just
`tokio` + `serde` + `iroh` and cross-compiles anywhere.

## Transport (`net`)

Every process runs one iroh endpoint (`net::Net`): a QUIC socket named by an
Ed25519 key (the *endpoint id*). Peers dial an id plus hints (a relay URL,
direct `ip:port`s); iroh hole-punches and falls back to the relay, which only
forwards encrypted packets. ALPN is `den/3`.

* **Keys** — an app's endpoint key is `<data>/device.key`; `serve` uses
  `<data>/serve.key`, so an app and a server sharing a data folder are
  different peers. Each key is held by one process at a time (`<key>.lock`,
  `net::lock_key`): a second app on the same profile passes its launch
  (join, host, open) to the running one over the control API and exits.
  Relays: `[net] relays` = `["default"]` (iroh's public
  relays), `[]` (direct only) or URLs; `DEN_RELAYS` overrides it.
  `[net] port_mapping` (default `true`) lets iroh's port mapper ask the
  router for a forwarded UDP port; `false` disables it and the router probe
  behind `reach`. The endpoint uses no DNS/pkarr discovery: links carry the
  addresses.
* **Links** — `den://<endpoint id>?room=<id>&invite=<key>&relay=<url>&ip=<addr>…`
  (`net::Link`). Invite links carry the invite key; the addresses saved in
  rooms, sent in handoffs and shown in discovery leave it out.
* **Routing** — one endpoint can host several rooms and join others. Each
  hosted server registers its room id; an incoming connection's first frame
  (`hello.room`, `probe.room`, `claim.room`) picks the server, and unknown
  rooms get `error {code: "no_room"}`. An endpoint can't dial itself, so
  joining a room you host goes through a second, loopback-only endpoint.
* **Relays** (`relay`) — `serve` runs an `iroh-relay` in-process (plain
  HTTP on TCP, same port number as the room). Its endpoint is registered
  with that relay at a loopback URL, and `Net::advertise_relays` swaps in
  the public URL (`[relay] url`, else `http://<public IP>:<port>` once
  address discovery finds it) wherever links are made. The other relays are
  then dropped. Members need no config: iroh dials through any relay named
  in an address. `den relay` runs the same relay alone. Every relay is
  gated: it admits only clients presenting its token (`relay::TokenGate`,
  derived from `<data>/relay.key`), which rides in the relay URL's
  `?token=` query, so links carry it and iroh sends it on connect without
  the relay ever joining anyone's relay map (or becoming their home relay).
* **Streams** — control is one bidirectional stream per connection carrying
  the same length-prefixed JSON frames as before; voice uses QUIC datagrams
  on the same connection.

## The two modes are one server

`den host` and `den serve` run the same `server::start`. Hosting
embeds it in the app and immediately joins it; `serve` wraps it in a
`ServerNode` that adds server-side mods, an admin console and its own
control API. An app hosts on its shared endpoint (any free UDP port by
default); `serve` binds its own endpoint on UDP 8245 so it can be
port-forwarded.

## Rooms move between hosts (`rooms`)

A room is a save that every member holds, not a process:

* **Save** — `<data>/rooms/<id>/room.toml` (format, id, name, motd, term,
  and app addresses, the room's picture) plus `member.key` (this member's
  key for the room, never sent), `history/<channel>.jsonl`
  (the host's history files and every member's copy use the same format)
  and `pictures/<id>.<ext>` (see *Pictures* below).
  A member's `Session` writes its copy as messages arrive (`rooms::Copy`);
  when it is connected to its own hosted server, the server writes instead.
* **Ids and terms** — the room id is stable; the `term` goes up by one on
  every change of host and is carried in `ServerInfo` (so probes and
  discovery show it). Message ids are `(unix ms << 10) | random`, above the
  previous id, so they stay unique and time-ordered across hosts.
* **Sync on join** — after `welcome`, the client sends `sync {channel, have}`
  per text channel; the host answers `backfill` (what the client lacks) and
  `want` (what it lacks), the client `upload`s those, and the host merges
  them (sorted by id, trimmed to `history_limit`) and backfills everyone
  else. History files are sorted and de-duplicated on load.
* **Planned handoff** — the host sends `host_request {term}` to a member
  that advertised `features: ["host"]`; that member hosts from its save and
  answers `host_ready {term, addrs}`; the host broadcasts
  `handoff {to, term}`, everyone (the old host's own session too)
  reconnects there and syncs, and the old server stops a few seconds later.
  Leaving while hosting does this with the best placed member who can
  host (see *Reachability* below), and so does closing: the app's window, SIGTERM/Ctrl-C to the app
  (`Session::close`, idempotent, ~6 s cap) and to `serve`
  (`ServerNode::close`). The new host gets the same "you're hosting" notice
  as a manual takeover.
* **Manual takeover** — "Host this room" (`Session::open_room`) probes the
  room's last host addresses and every member app it knows of (`peers`,
  learned from `member.addr`) for a live host with the same id;
  if there is none it hosts from the save at `term + 1`. If several hosts
  run the room (two members took over at once without seeing each other),
  `rooms::best_host` picks the same one for everyone: highest term, then a
  `serve` over an app, then the lowest endpoint id. Claims stay strict (a
  higher term than the host's), so equal terms never claim each other.
* **Claim** — a peer already hosting the room at a higher term sends
  `claim {room, term, addrs, proof}` as its first frame; the host checks it
  like a join (invite and allow list), broadcasts `handoff`, and steps down.
  This is how `serve` takes its room back after downtime
  (`serve --adopt <link>` uses it to move an app-hosted room onto a server).
  A server records where the room went when it hands off, so it probes there
  first on its next start.
* **Rejoining** — a member whose host has gone looks for the room (by id)
  at its saved addresses and members' apps while it retries, so it follows
  a handoff it missed. Retries update one "connection lost" line instead of
  adding one per attempt; `-v` logs every attempt and lookup.
* **Addresses** — connect targets (handoff `to`, saves, discovery `addr`)
  are links without the invite; the short endpoint id is for display.
* **History files** never store `author_id`: member ids are per host. A
  member's own uploads (what the host `want`ed) are merged into its own view
  as well as the host's.

Channels work the same way: any member can add one (`create_channel`); the
host appends it, broadcasts `channels_updated`, and every member (and a
`serve` node) writes the new list into its save, so whoever hosts next
serves it.

**Pictures** (`pictures`) work the same way too. Profile pictures and the
room's picture are small image files (256 px squares, at most 256 KB) named
by their BLAKE3 hash, so an id always means the same bytes and anyone can
check a file against it. The room refers to them by id (`Member.picture`,
`Profile.picture`, `ServerInfo.picture`); the files travel apart from that:

* Setting one sends `put_picture {id, data}` (base64) and then
  `set_picture {picture}` or `set_room_picture {picture}`. Anyone may change
  the room's picture; the host broadcasts `room_updated {server, by}`.
* A member who sees an id it lacks asks with `get_pictures {ids}` and the
  host answers `picture {id, data}`. A host that lacks a file the room uses
  asks the members for it (`want_pictures`), and sends it on when it comes.
  `hello` carries the member's own picture id, so whoever hosts gets it back
  from its owner.
* Every member keeps the files its room uses in its copy (`pictures/`), so
  the next host can serve them; a host deletes files nothing uses when it
  starts. Your own picture lives in `<data>/picture.<ext>` and goes to every
  room you join.
* `hello` and `ServerInfo.features` say `pictures`; neither side sends the
  picture messages to a peer that doesn't (older peers would drop the
  connection on an unknown message).

Everyone has the same rights, so there are no roles to sync; join rules
travel in `welcome` (`access`), and with the invite key and profiles, are
enforced by whoever hosts.

## Reachability: who should host

Everyone connects to the host, so the room works best when others can reach
the host directly: then nobody needs a relay, whatever their own NAT. Each
member carries `reach` (`proto::Reach`, worst to best):

| `reach` | meaning |
| --- | --- |
| `strict` | UDP blocked, or a NAT that picks a new port per destination (hole-punching rarely works) |
| `unknown` | not measured yet |
| `nat` | behind a router: connecting out works, connections in need hole-punching |
| `public` | the machine has a public address (not checked yet) |
| `open` | the host connected to it directly, from a fresh endpoint |

* **The app measures itself** (`Net::reach`): iroh's network report (UDP
  over v4/v6, whether the NAT mapping varies by destination; needs relays)
  plus whether this machine's own outgoing address is public, or the router
  forwards it a port (iroh's port mapper asks for one by UPnP / NAT-PMP /
  PCP; `Net` asks the router every 5 min whether it offers that). It sends that
  in `hello` and, when it or its address changes, in `reach {reach, addr}`
  (checked every 5 s). A claim is capped at `public`.
* **The host checks** (`server::check_reach`): it dials the member's public
  addresses from a throwaway endpoint with no relay, no port mapping and no
  history with the member, so nothing can hole-punch for it. Success means
  anyone could connect: `open`. Failure caps the claim at `nat`. One check
  per member every 2 s at most; a newer report wins. The host's own app
  (hosting from it) isn't checked. Loopback-only hosts (tests) check
  loopback addresses too. Caveat: a member on the host's LAN is checked via
  the router (hairpin NAT), and an address-restricted NAT can let the host's
  IP in where a stranger's wouldn't.
* **Uses** — `hand_off(None)` picks the best `reach`, then whoever has been
  here longest. When the host is gone, `SessionState::best_host` /
  `better_host` tell the UI (and `/takeover`) if someone else is better
  placed to take over; it stays a manual choice.

## Identity: a key per room

There are no accounts. Each member holds an Ed25519 key per room
(`rooms/<id>/member.key`, made on first join). On `hello` (and `claim`) the
client sends `proof {key, sig, invite}`: signatures over
`kind || 0 || exporter`, where the exporter is 32 bytes of TLS keying
material from this QUIC connection (label `den/3 proof`, context = room
id). A signature can't be replayed on another connection, and the host only
ever sees public keys, so any member can host without learning anyone's
secret.

* `sig` (kind `member`) proves the member key: `identity.key` is set and
  `verified` is true.
* `invite` (kind `invite`) is signed with the room's invite key, which every
  member saves. A room with an invite key admits only proofs that carry it.
  Rooms made without one (`--open`) admit anyone who can reach them.
* The room keeps profiles (key → nickname, `welcome.profiles`, saved by every
  member). A nickname that belongs to another key gets a ` (2)` suffix, so
  nobody can pass as someone else in that room.
* `allow` lists key prefixes (8+ characters); when set, only those keys get
  in.

What the host can still do: it relays everything, so a malicious host could
drop or alter messages. Signing messages with member keys is the planned
fix.

## Wire protocol (`denchat-proto`)

* **Control stream** — `u32` big-endian length + JSON body on one QUIC
  bidirectional stream. `ClientMsg` / `ServerMsg` are tagged enums
  (`{"type": "send", …}`). A connection either sends `probe {room?}`
  (discovery: get room info, close) or `hello` (version, nick, codecs,
  features, room, proof, the app's own link, its `reach`, its picture) and receives `welcome` with

  members, channels, join rules, profiles and the voice setup (a per-member
  token), followed by history. Room sync and handoff messages are described
  above.
* **Voice datagrams** (same connection) — binary packets: `REGISTER`
  (token keepalive), `AUDIO` (client → server: token, seq, codec, payload),
  `RELAY` (server → client: speaker id, seq, codec, payload), `ACK`. The
  server is a selective forwarding unit: it never decodes audio, takes the
  speaker from the connection (the token must match it), drops
  muted/deafened senders, and relays to others in the same voice channel.
* **Codecs** — 48 kHz mono, 20 ms frames. Opus by default, raw PCM16 as a
  fallback for builds without Opus. Each client advertises what it decodes;
  senders pick the best codec every listener supports.
* **Identity** — members carry `identity: {key, verified}` (see above);
  messages carry `author_key`.
* **App messages** — `ClientMsg::App {ns, data, channel?, to?}` lets mods
  define their own protocols (games, presence, reactions) without server
  changes; the server relays them untouched.

## Session: the only thing a frontend needs

`denchat_core::session::Session` is UI-agnostic:

1. **Read** `session.state()` → `SessionState` (connection, server info,
   channels, members, messages, focus, unread + mentions + "new since", voice
   (incl. when you joined), discovery, our endpoint (`net`)).
2. **Call** methods: `host`, `join`, `leave`, `submit` (slash commands +
   text), `send_message`, `focus_channel`, `voice_join`, `set_muted`,
   `set_voice_devices`, `run_mod_command`, `ui_action`, …
3. **Listen**: `on_change(f)` for repaint, `subscribe()` for named events
   (`message.created`, `member.joined`, `voice.joined`, `ui.action`, …).

The control API exposes the same three things to other processes
([API.md](API.md)), so a frontend in another language — or an agent — is a
peer of the built-in one, not a second-class citizen.

Connection handling lives here too: epoch-tagged connection tasks over a
list of addresses, reconnect with backoff, following a room to its new host,
voice auto-rejoin, latency pings, recent addresses, and the member's room
copy.

## UI store: declarative panels

`UiStore` holds panels (`id → title, slot hint, owner, widgets`) and toasts.
Mods fill panels with widgets (`heading`, `text`, `kv`, `list`, `progress`,
`button`, `link`, `row`, `separator`) via panel commands or `ui.panel.set`.
Frontends render widgets however suits them; mods never draw pixels, so the
same mod works in the GUI, a future TUI, or a web view.

## The egui shell (`denchat-gui`)

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
* Hot reload polls watched files; `ui.theme.set` / `ui.layout.set` make
  the shell scriptable.
* **Driving it** (`agent.rs`): an egui plugin that answers the `ui.*`
  frontend calls core forwards to it (`UiStore::call_frontend`). It reads
  the accesskit tree (each panel is a group tagged `panel:<id>`) to find
  widgets. It injects input as egui events or accesskit actions, so there's
  no OS input and no focus change. It keeps a CPU copy of the textures so
  `soft.rs` can draw a screenshot of a hidden window. The same `DenApp` also
  runs with no window at all (`headless.rs`, `DEN_WINDOW=headless`): plain
  egui frames on a timer, and screenshots drawn on the CPU.

## Mods

A mod is a folder with `den-mod.toml`. Den runs its commands as
child processes with `DEN_*` env vars, feeds hook events on stdin, and
routes stdout (`send` to chat, `notice` as a toast, or `none`). Mods call
back through `$DEN_BIN api …`. Mods can ship themes and layouts.
`runs_on` decides client vs server. See [MODS.md](MODS.md).

## Writing another frontend

Either link `denchat-core` and drive a `Session` directly (Rust), or start
any Den instance and use the control API from any language:
`state.get` + `events.subscribe` to render, `input.submit` / `voice.*` /
`ui.action` to act. `den chat` (≈150 lines, `crates/denchat/src/term.rs`)
is a complete example.
