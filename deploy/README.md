# Running a persistent room

`tailchat serve` is the headless mode: no GUI, no audio libraries, history on
disk, server-side mods, an admin console on stdin and a control API for
scripts (`tailchat api --server …`). Build it with:

```sh
cargo build --release -p tailchat --no-default-features
```

| Option | File | Notes |
| --- | --- | --- |
| systemd on a tailnet machine | [`tailchat.service`](tailchat.service) | binds the machine's tailnet IP; `--no-console` because stdin isn't a terminal |
| Docker + Tailscale sidecar | [`docker-compose.yml`](docker-compose.yml), [`Dockerfile`](Dockerfile) | the room gets its own tailnet node named `tailchat`; needs a `TS_AUTHKEY` |
| By hand | — | `tailchat serve` in `tmux`/`screen` gives you the admin console |

Settings: `tailchat serve --write-config` writes `server.toml` (name, motd,
channels, history limit, `require_tailnet`, `allow` list of tailnet logins,
max members, rate limit). Command-line flags override it.

Admin from the same machine:

```sh
tailchat api --server state.get
tailchat api --server message.send text="restarting in 5 minutes"
tailchat api --server member.kick id=3 reason="be nice"
```

Server-side mods (`runs_on = ["server"]`) go in `$TAILCHAT_HOME/config/mods/`.
