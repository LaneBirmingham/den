//! Built-in themes and layouts, embedded at compile time from `/assets`.

use std::path::PathBuf;

use tailchat_core::paths::Paths;

pub const THEMES: &[(&str, &str)] = &[
    ("catppuccin", include_str!("../../../assets/themes/catppuccin.toml")),
    ("catppuccin-latte", include_str!("../../../assets/themes/catppuccin-latte.toml")),
    ("tokyo-night", include_str!("../../../assets/themes/tokyo-night.toml")),
    ("rose-pine", include_str!("../../../assets/themes/rose-pine.toml")),
    ("rose-pine-dawn", include_str!("../../../assets/themes/rose-pine-dawn.toml")),
    ("gruvbox", include_str!("../../../assets/themes/gruvbox.toml")),
];

/// Bundled fonts (`assets/fonts`): Inter in three weights and the Phosphor icon font.
pub const FONTS: &[(&str, &[u8])] = &[
    ("inter", include_bytes!("../../../assets/fonts/Inter-Regular.ttf")),
    ("inter-medium", include_bytes!("../../../assets/fonts/Inter-Medium.ttf")),
    ("inter-semibold", include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf")),
    ("phosphor", include_bytes!("../../../assets/fonts/Phosphor.ttf")),
];

pub const LAYOUTS: &[(&str, &str)] = &[
    ("default", include_str!("../../../assets/layouts/default.toml")),
    ("messenger", include_str!("../../../assets/layouts/messenger.toml")),
];

/// Copy built-ins into the config dir as editable starting points. Never overwrites.
pub fn export(paths: &Paths) -> anyhow::Result<Vec<PathBuf>> {
    let mut wrote = Vec::new();
    for (dir, items) in [(paths.themes_dir(), THEMES), (paths.layouts_dir(), LAYOUTS)] {
        std::fs::create_dir_all(&dir)?;
        for (name, text) in items {
            let path = dir.join(format!("{name}.toml"));
            if !path.exists() {
                std::fs::write(&path, text)?;
                wrote.push(path);
            }
        }
    }
    Ok(wrote)
}
