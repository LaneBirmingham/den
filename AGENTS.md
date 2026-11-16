# Guide for AI agents

You may be asked to work on this repo, or to **customise a user's Den**
(look, layout, behaviour). The second rarely needs Rust: themes, layouts and
mods are files, and a running app can be inspected and driven through
`den api`.

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
4. Behaviour and new UI: write a mod (`den mods new <id>`) —
   commands, hooks, declarative panels. See [docs/MODS.md](docs/MODS.md).
   Prefer a mod over patching Rust; mods survive upgrades.
5. Apply and check, with the app running:
   ```sh
   den api ui.theme.set name=<theme>
   den api ui.layout.set name=<layout>
   den api mods.reload
   den api ui.screenshot path=/tmp/den.png   # then look at the PNG
   ```
   Files hot-reload on save, so steps 2–3 apply without the first two calls
   if that theme/layout is already selected. Parse errors show in the app and
   the previous version stays up.
   Without a running app: `DEN_SCREENSHOT=/tmp/t.png DEN_SCREENSHOT_EXIT=1 den`.
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
  `crates/den-gui/src/panels/mod.rs` and placed in `assets/layouts/*`.
  Panels read state and queue `Actions`; they never block or lock.
* Integration tests live in `crates/denchat-core/tests/room.rs` and run real
  servers on loopback; use `bind: "localhost"` and `NetConfig::direct_only()`
  (see the `session` and `test_net` helpers) so tests never touch relays.
* Visual check: `DEN_HOME=$(mktemp -d) DEN_SCREENSHOT=/tmp/x.png
  DEN_SCREENSHOT_EXIT=1 cargo run -- host --bind localhost`.
