# Example mods

Each folder is a self-contained mod: a `den-mod.toml` manifest plus
whatever scripts it runs. Mods can be written in any language; these use
Python 3 so they run on macOS, Linux and Windows unchanged.

| Mod | Shows |
| --- | --- |
| [`dice`](dice) | a slash command (`/roll 2d6`) and a panel it refreshes through the API |
| [`greeter`](greeter) | a **server-side** hook that welcomes people (`runs_on = ["server"]`) |
| [`room-stats`](room-stats) | a long-running daemon that streams events and pushes a live panel |
| [`sakura`](sakura) | a reskin: a theme and a layout, no code at all |

Try one without installing it:

```sh
den mods link ./mods/dice        # or: DEN_MODS_PATH=./mods den
den mods list
```

The app picks up linked mods on start, or immediately via *View → Reload mods*
(`den api mods.reload` also works). See [docs/MODS.md](../docs/MODS.md).
