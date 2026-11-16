# Guide for AI agents

You may be asked to work on this repo, or to **customise a user's Den**
(look, layout, behaviour). The second rarely needs Rust: themes, layouts and
mods are files, and a running app can be inspected and driven through
`den ui`, `den status` and `den api`.

## Seeing and driving the app

You can use the app the way a person does, in the background, with short
text output. Full manual: [docs/AGENT-TESTING.md](docs/AGENT-TESTING.md).

```sh
den status                            # connection, room, channels, members, voice, mods
den ui tree [--panel id]              # widgets on screen: @ref role "text" flags, by panel
den ui click "Room menu"              # or @12, button:Join, X,Y; --wait <target> after
den ui type "hi" --enter              # into the focused text box (--into <target>)
den ui key Escape | ctrl+a | cmd+k
den ui scroll 300 --over <target> · den ui hover <target> · den ui wait <target> [--gone]
den ui shot [path] [--panel id] [--scale 0.5]    # PNG (works hidden/minimised/headless)
den ui window [--size 900x600 --minimize …] · den ui scale 1.5 · den ui clip · den ui quit
den wait message.created --where message.text=hi -- den ui type hi --enter   # listen, act, wait
```

On the **user's own app** these never move the mouse or take focus. Still,
avoid `den ui window --focus`, and remember that "Copy" buttons in their
app copy to their clipboard. To test anything that changes state (a mod, a
theme, room flows), use a **sandbox**. A sandbox is a throwaway headless
Den with its own home, test audio devices and a file clipboard:

```sh
den sandbox start t --mod ./my-mod [--copy-config]   # hosts a room; --join <other> / --lobby
den -S t ui tree --panel my.mod
den sandbox start u --join t                         # a second member
den sandbox stop --all --rm
```

## Customising a user's install (no Rust)

1. Find their files: `den config path`. If `themes/` and `layouts/` are
   empty, run `den config init` to export editable copies of the
   built-ins (it never overwrites).
2. Look: edit `<config>/themes/<name>.toml` (palette, shape, text) — see
   [docs/CUSTOMIZING.md](docs/CUSTOMIZING.md#themes). Create a new file
   rather than changing a built-in name if the user may want to switch back.
3. Arrangement: edit or create `<config>/layouts/<name>.toml`. Panels are
   ids in slots; remove an id to remove that feature from the UI. Built-in
   panel ids and their settings are listed in CUSTOMIZING.md.
4. Behaviour and new UI: write a mod (`den mods new --id <id>`) —
   commands, hooks, declarative panels. See [docs/MODS.md](docs/MODS.md).
   Prefer a mod over patching Rust; mods survive upgrades.
5. Try it in a sandbox first (`den sandbox start t --copy-config --mod
   <dir> --theme <name>`), then apply to the running app:
   ```sh
   den api ui.theme.set name=<theme>
   den api ui.layout.set name=<layout>
   den api mods.reload
   den ui shot /tmp/den.png --scale 0.5     # then look at the PNG
   den ui tree --panel <mod id>             # and check the panel's widgets
   ```
   Files hot-reload on save, so steps 2–3 apply without the first two calls
   if that theme/layout is already selected. Parse errors show in the app and
   the previous version stays up.
6. Ship it: put the theme/layout in a mod (`themes = [...]`, `layouts = [...]`)
   so it can be shared or removed as one unit (example: `mods/sakura`).

Rules of thumb: use theme colour tokens (`accent`, `muted`, …) in mod
widgets, never hard-coded colours; keep every panel removable; don't assume
the GUI exists (`den serve` and `den chat` have none).

## Working on the code

* Build/test: `cargo build`, `cargo test --workspace`,
  `cargo clippy --workspace --all-targets` (keep it warning-free),
  headless check: `cargo build -p denchat --no-default-features`.
* Map: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Frontend-facing logic
  belongs in `denchat-core::session` (and its API), not in the GUI, so every
  frontend and the API get it.
* Anything a built-in panel can do should be reachable through the control
  API; add the method next to `SESSION_METHODS` in `session.rs`.
* New built-in UI = a new `Panel` registered in
  `crates/denchat-gui/src/panels/mod.rs` and placed in `assets/layouts/*`.
  Panels read state and queue `Actions`; they never block or lock.
* Keep the UI drivable: a hand-painted widget (`allocate_exact_size`,
  `interact`) gets a name with `kit::name(&resp, Role::Button, "what it
  says")`, or it is invisible to `den ui tree`/`click` and screen readers.
  Stock egui widgets name themselves. `ui.*` driving lives in
  `crates/denchat-gui/src/agent.rs`; core forwards to it through
  `UiStore::call_frontend`.
* Integration tests live in `crates/denchat-core/tests/room.rs` and run real
  servers on loopback; use `bind: "localhost"` and `NetConfig::direct_only()`
  (see the `session` and `test_net` helpers) so tests never touch relays.
* GUI tests: `crates/denchat-gui/tests/headless.rs` runs the real app
  headless and drives it like `den ui` does.
* Visual check: `cargo run -- sandbox start a && cargo run -- -S a ui shot
  /tmp/x.png` (or `DEN_HOME=$(mktemp -d) DEN_WINDOW=headless
  DEN_SCREENSHOT=/tmp/x.png DEN_SCREENSHOT_EXIT=1 cargo run -- host`). What
  agents can and can't test in the background:
  [docs/AGENT-TESTING.md](docs/AGENT-TESTING.md).
