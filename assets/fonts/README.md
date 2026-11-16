# Bundled fonts

Embedded in the GUI (`crates/den-gui/src/assets.rs`), so the app looks the
same everywhere. A theme can still swap the text font with `text.font`.

| File | What | License |
|---|---|---|
| `Inter-Regular.ttf`, `Inter-Medium.ttf`, `Inter-SemiBold.ttf` | [Inter](https://rsms.me/inter/) 4.1, the UI typeface. Each weight is its own family because egui can't synthesise bold. Inter's private-use code points (U+E000–F8FF) are unmapped so they don't shadow the icons. | SIL OFL 1.1 (`LICENSE-Inter.txt`) |
| `Phosphor.ttf` | [Phosphor Icons](https://phosphoricons.com), regular weight, as an icon font. Code points are in `crates/den-gui/src/icons.rs`. | MIT (`LICENSE-Phosphor.txt`) |
