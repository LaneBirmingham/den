# Control API

Every running tailchat — the app, the terminal client, a headless server —
listens on a random **loopback** port for newline-delimited JSON. It's how
mods, scripts and agents drive tailchat, and it exposes the same surface the
built-in UI uses.

## Using it

```sh
tailchat api api.methods                 # what this instance supports
tailchat api state.get
tailchat api message.send text="hi"      # key=value params (values parsed as JSON when possible)
tailchat api input.submit '{"text":"/roll 2d6"}'
tailchat api events.subscribe            # stream events until ^C
tailchat api --list                      # running instances
tailchat api --server state.get          # pick the headless server when both run
```

`tailchat api` finds an instance via `$TAILCHAT_API`/`$TAILCHAT_API_TOKEN`
(set for mods), else `<state>/instances/<pid>.json` (newest first; stale files
are pruned). Instance files are `0600` and contain the token.

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
| `state.get` | | connection, room, members, channels, voice, focus, discovery |
| `messages.list` | `channel?, limit?` | recent messages (default: focused channel) |
| `message.send` | `text, channel?` | send plain text |
| `input.submit` | `text, channel?` | as if typed: slash commands work |
| `channel.focus` | `channel` | switch the focused text channel |
| `nick.set` | `nick` | |
| `voice.join` / `voice.leave` | `channel?` | |
| `voice.mute` / `voice.deafen` | `muted` / `deafened` | |
| `voice.stats` | | mic level, speakers, devices, codec |
| `app.send` | `ns, data, channel?, to?` | relay a mod-defined payload |
| `ui.panel.set` | `panel, widgets, title?` | create/update a panel ([widgets](MODS.md#widgets)) |
| `ui.panel.clear` | `panel` | |
| `ui.panels.list` | | |
| `ui.notify` | `text, level?` | toast (`info`, `warn`, `error`) |
| `ui.action` | `panel, action, value?` | emit a `ui.action` event |
| `ui.theme.set` / `ui.layout.set` | `name` | switch look; the app follows within a second |
| `ui.screenshot` | `path?` | PNG of the app window (GUI only) |
| `mods.list` / `mods.reload` | | |
| `mods.run` | `command, args?` | run a mod command |
| `session.host` | `name?, bind?, port?, persist?` | host a room |
| `session.join` | `addr` | |
| `session.leave` | | |
| `tailnet.status` / `tailnet.discover` | | peers / find rooms |
| `config.get` | | |
| `api.methods`, `api.ping`, `events.subscribe` | | built in everywhere |

## Server methods

`tailchat serve` exposes: `state.get`, `members.list`, `channels.list`,
`messages.list`, `message.send` (as the server), `notice.send`,
`member.kick {id, reason?}`, `app.send`, `motd.set`, `mods.list`,
`mods.reload`.

## Events

See [MODS.md](MODS.md#events). Filters accept exact names, `prefix.*`, or `*`.
