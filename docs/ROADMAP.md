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
  succession list (members who can host, by join time) waits a few seconds
  and hosts; others try the list in order. Terms already prevent a stale
  host from winning, and the client already looks for the room by id.
* **Split brain.** Two members clicking *Host this room* at once (or a
  partitioned tailnet) give two hosts of the same room at the same term.
  Today people notice and one leaves; their copies merge by id when they
  meet again. A fix: on `claim` with an equal term, the lower room address
  wins.
* **Roles.** If rooms ever need owners or moderators, the host's admission
  check (`Shared::refusal`) and the save's `access` are where they go, with
  the save format bumped.
* **Copy size.** Copies keep the last 500 messages per channel. Older
  history only lives with whoever kept it longer (a server with a higher
  `history_limit`).

## Smaller items

* **A phone client.** A browser client was built and then removed (Nov 2026):
  browsers only allow the microphone on HTTPS pages, which made self-hosting
  fiddly. When phones matter, build a native app on `tailchat-core`'s
  protocol instead.
* Screen share and file transfer.
