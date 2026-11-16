# Running a persistent room

`den serve` is the headless mode: no GUI, no audio libraries, the room
saved on disk, server-side mods, an admin console on stdin and a control API
for scripts (`den api --server …`). Build it with:

```sh
cargo build --release -p denchat --no-default-features
```

| Option | File | Notes |
| --- | --- | --- |
| systemd | [`den.service`](den.service) | `--no-console` because stdin isn't a terminal; the invite link is in the journal |
| Docker | [`docker-compose.yml`](docker-compose.yml), [`Dockerfile`](Dockerfile) | publishes UDP 8245; the invite link is in the container log |
| By hand | — | `den serve` in `tmux`/`screen` gives you the admin console |

Settings: `den serve --write-config` writes `server.toml` (name, motd,
channels, history limit, `allow` list of member keys, max members, rate
limit, `bind`, `port`, `[net] relays`). Command-line flags override it.

**Networking.** Forward two ports to the server:

| Port | What |
| --- | --- |
| UDP 8245 (`--port`) | the room: members connect here directly |
| TCP 8245 (`[relay] port`, default: same number) | the room's own relay, for members who can't connect directly |

At start `serve` works out its public IP (by asking n0's relays what they
see, once) and puts `http://<public IP>:8245` in the invite link as the
room's relay; after that it uses only its own relay. On a dynamic IP, give
it a name that follows the IP (DDNS): `--relay-url box.example.com:8245` or
`[relay] url` in `server.toml`; that also skips the n0 lookup. Behind a TLS
reverse proxy, use the `https://` URL. `--no-relay` / `[relay] enabled =
false` turns it off (members then fall back to `[net] relays`).

The relay isn't open to the internet: it admits only apps that present its
token, and the token travels in the room's invite links (`relay=…?token=…`),
so it reaches exactly the people the room was shared with. It's derived from
`data/relay.key`; delete that to change it (links made earlier lose the
relay, not the room). Each client is rate-limited to 4 MB/s, and the relay
only ever sees encrypted packets. Over plain HTTP, someone on the network
path could read the token and relay through you; a TLS proxy in front stops
that. They still couldn't join a room: rooms check the invite key and member
keys themselves.

Its endpoint key is `data/serve.key`: keep it, and the room's address stays
the same across restarts.

**Rooms without a server.** `den relay` runs just a relay (TCP 3340 by
default) on any machine with a forwarded port. Whoever hosts a room from the
app adds the URL it prints, token included
(`[net] relays = ["http://box.example.com:3340/?token=…"]`), to `config.toml`;
members need nothing, the invite link carries it.

**The invite link** (printed at start, `join with:`) is the key to the
room: anyone with it can join. It's saved in the room (`invite` in
`room.toml`), so it survives restarts, and every member's app can share it.

Join rules are saved in the room (`data/rooms/<id>/room.toml`, `[access]`)
and enforced by whoever hosts it, so flags you pass once stay in force:

| Flag | Effect on the saved rules |
| --- | --- |
| `--allow <key>` (repeatable) | only admit these member keys (8+ leading characters, as `who` shows them), on top of the invite; **replaces** the saved list |
| `--open` | clear the allow list first; add `--allow` after it to set a new one |
| (none) | keep what's saved |

Locked yourself out? `den serve --room <id> --open`. Refused joins are
logged with the peer's key.

Which room it hosts:

* default: the saved room named like `server.toml`'s `name`, created on the
  first run (in `$DEN_HOME/data/rooms/<id>/`);
* `--room <id|name>`: a saved room, e.g. a folder copied from a member's
  `data/rooms/`; its name, channels and join rules win over `server.toml`;
* `--adopt '<invite link>'`: the room that link points at, hosted by
  someone right now. The server takes it over and members' copies fill in
  the history.

The server is the room's preferred host, not its only one. On SIGTERM /
Ctrl-C / `quit` it hands the room to a member whose app can host (waiting
up to ~6 s), so `systemctl stop` or a container restart doesn't strand
anyone. While it's down, any member can *Host this room* from their copy;
when it comes back it finds the room where it went (or at members' apps)
and claims it (members move over automatically). An app on the same
machine is a separate peer (its own key), so the server can take the room
from it too.

Admin from the same machine:

```sh
den api --server state.get
den api --server message.send text="restarting in 5 minutes"
den api --server member.kick id=3 reason="be nice"
```

Server-side mods (`runs_on = ["server"]`) go in `$DEN_HOME/config/mods/`.
