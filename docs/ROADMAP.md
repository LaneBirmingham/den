# Roadmap and design notes

Ideas that are wanted but not built yet, with enough thinking written down
to start from. Nothing here is a promise about wire formats.

## Hand the room over when the host leaves

**Today:** a room lives in one process. With `tailchat host` (host +
client), it's someone's app; when they quit or drop, the room is gone.
Everyone's app reconnects with backoff to the same address and waits.
Always-on rooms (`tailchat serve`, Docker) avoid this, but a group playing
peer-to-peer shouldn't need a server to survive the host leaving.

**Goal:** when the hosting member leaves (cleanly or not), another online
member who is able to host takes over. Everyone reconnects to them
automatically and carries on in the same channels with recent history. Later,
anyone can leave and rejoin the group without caring who's hosting.

### Sketch

1. **Who can take over.** A client advertises `features: ["host"]` in its
   `hello` when it can run a room (the desktop app can; `tailchat chat`
   can't). The host keeps an ordered **succession list**:
   eligible members by join time, with an opt-out setting and the original
   host first if they come back. It shares the list with everyone in a new
   `ServerMsg::Succession { term, hosts: [{id, addr}] }` whenever it changes.
2. **What gets replicated.** Successors need room config (name, motd,
   channels), recent history (capped, like `history_limit`), and the
   allow/require settings. The host streams them to the first one or two
   successors, either as `App`-style messages or a dedicated
   `ServerMsg::Replica` that clients without the feature ignore. Small rooms
   mean this is kilobytes.
3. **Terms, to avoid two hosts.** Every host change bumps a `term`
   (epoch), carried in `welcome`. A client only follows a host whose term is
   at least the highest it has seen. A successor only starts hosting after
   the old host is unreachable for a grace period (for example 3 missed
   pings, about 5 s), and steps down if it discovers a host with a higher or
   equal term and earlier succession rank.
4. **Failover on the client.** On disconnect, a client tries, in order:
   reconnect to the same host (brief blips), then each address in the
   succession list, each with short timeouts. `Session` already has
   epoch-tagged connections, reconnect with backoff and voice auto-rejoin.
   This adds a list of fallback addresses to that loop. The next host
   starts its room on the agreed port, with the replicated state.
5. **Graceful handoff.** When the host quits on purpose ("Leave" or closing
   the app), it picks the first successor, sends it a final replica, sends
   `ServerMsg::Handoff { to: addr, term }` to everyone, waits briefly, and
   then exits. Clients move straight over without waiting for timeouts.
6. **Coming back.** A rejoining member connects to any address it knows. If
   that peer isn't hosting any more, it answers `probe` with the current
   host (`ProbeInfo { redirect }`), so old links and saved recent-rooms
   still work.

### Open questions

* **Split brain** on a partitioned tailnet: two halves can each elect a host.
  Terms and ranks resolve it when they reconnect, but which history wins? The
  simplest answer: higher term wins, and the other half's messages since the
  split are merged by timestamp with their ids rewritten.
* **Message ids** are per-server counters today. Replicas need ids that
  survive a change of host (e.g. `(term, n)` or ULIDs).
* **Voice tokens and UDP addresses** are per host. Clients re-register
  after a handoff, which `Session` already does on reconnect.
* **Identity:** the next host verifies members with its own
  `tailscale whois`, and gets the same answers on the same tailnet. Allow
  lists travel with the replica.
* **Discovery** should show the group under one name whoever hosts it, maybe
  by a room id carried in `ServerInfo`.

## Smaller items

* **A phone client.** A browser client was built and then removed (Nov 2026):
  browsers only allow the microphone on HTTPS pages, which made self-hosting
  fiddly. When phones matter, build a native app on `tailchat-core`'s
  protocol instead.
* Screen share and file transfer.
