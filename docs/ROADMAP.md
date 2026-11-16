# Roadmap and design notes

Ideas that are wanted but not built yet, with enough thinking written down
to start from. Nothing here is a promise about wire formats.

## Rooms that outlive their host

**Built (Nov 2026):** rooms are saves every member keeps; the host can hand
the room to another member (on purpose, or by leaving), anyone can *Host
this room* when the host has vanished, and `serve` acts as the room's
always-on host that claims it back after downtime. See
[ARCHITECTURE.md](ARCHITECTURE.md#rooms-move-between-hosts-rooms). Kept
deliberately simple: everyone in a room has the same rights, and takeover
after a crash is a button, not automatic.

### Next, if wanted

* **Automatic takeover.** When the host vanishes, the first member in a
  succession list (members who can host, best `reach` first, then by join
  time; `SessionState::best_host` already computes it) waits a few seconds
  and hosts; others try the list in order. Terms already prevent a stale
  host from winning, and the client already looks for the room by id.
* **Split brain.** Two members clicking *Host this room* at once (or a
  a partitioned network) give two hosts of the same room at the same term.
  Today people notice and one leaves; their copies merge by id when they
  meet again. Members already pick the same host (`rooms::best_host`:
  highest term, then `serve`, then the lowest endpoint id); a fix is to let
  that host claim the other at an equal term.
* **Roles.** If rooms ever need owners or moderators, the host's admission
  check (`Shared::refusal`) and the save's `access` are where they go, with
  the save format bumped.
* **Copy size.** Copies keep the last 500 messages per channel. Older
  history only lives with whoever kept it longer (a server with a higher
  `history_limit`).

## Transport and identity

* **Signed messages.** Members sign what they say with their room key, so a
  host can relay but not forge or alter. Copies would keep signatures and
  check them on sync.
* **Rooms remember their relay.** A room hosted by `serve` uses its relay;
  when an app hosts the room instead, it falls back to its own `[net]
  relays` (n0 by default). Saving the room's relay in the save would keep a
  room on a standalone `tailchat relay` without per-host config.
* **Relay access per member.** Relays admit only apps with their token,
  which travels in invite links, so anyone with a link (or a saved room)
  can relay through them, and the token can only change for everyone at
  once (delete `relay.key`). Tokens bound to a member key and endpoint id
  would let `--allow` and kicks reach the relay too, and wouldn't be worth
  anything to someone who read one off the wire.
* **Key backup / new device.** A member's room keys live in
  `<data>/rooms/*/member.key`; copying the data folder moves them. A
  "link this device" flow (export one room's key as a QR code or code
  phrase) would make that friendlier.
* **Invite rotation.** A new invite key for a room that leaked its link,
  while letting known profiles back in without it.

## Smaller items

* **A phone client.** A browser client was built and then removed (Nov 2026):
  browsers only allow the microphone on HTTPS pages, which made self-hosting
  fiddly. When phones matter, build a native app on `tailchat-core`'s
  protocol instead.
* Screen share and file transfer.
