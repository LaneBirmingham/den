//! Layouts: which panels go where, as TOML.
//!
//! The shell has five slots per screen (`top`, `bottom`, `left`, `right`,
//! `center`) plus floating `windows` and `overlays`. A slot is an ordered list
//! of panel ids. That is deliberately simple: a human or an agent can move a
//! panel, remove one (switching it off), or add a mod panel by editing one
//! line, and the app hot-reloads the file.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct Layout {
    pub name: String,
    pub description: Option<String>,
    pub lobby: Screen,
    pub room: Screen,
    /// Per-panel settings, keyed by panel id.
    pub panel: BTreeMap<String, toml::Table>,
    pub keys: Keys,
    pub shell: ShellSpec,
}

/// How the shell frames slots.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ShellSpec {
    /// `cards` (default): slots float as rounded cards on a darker canvas.
    /// `flat`: slots fill the window edge to edge, split by lines.
    pub style: ShellStyle,
    /// Space between cards and around the window edge, in points.
    pub gap: f32,
}

impl Default for ShellSpec {
    fn default() -> Self {
        ShellSpec { style: ShellStyle::Cards, gap: 10.0 }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellStyle {
    #[default]
    Cards,
    Flat,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct Screen {
    pub top: Option<Slot>,
    pub bottom: Option<Slot>,
    pub left: Option<Slot>,
    pub right: Option<Slot>,
    pub center: Option<Slot>,
    pub windows: Vec<WindowSpec>,
    pub overlays: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotMode {
    #[default]
    Stack,
    Tabs,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Slot {
    #[serde(default)]
    pub panels: Vec<String>,
    /// Width (left/right) or height (top/bottom) in points.
    #[serde(default)]
    pub size: Option<f32>,
    #[serde(default)]
    pub resizable: Option<bool>,
    #[serde(default)]
    pub mode: SlotMode,
    #[serde(default = "yes")]
    pub visible: bool,
    /// Cards style only: draw this slot as a card (default true). With
    /// `card = false` its panels sit straight on the canvas.
    #[serde(default)]
    pub card: Option<bool>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct WindowSpec {
    /// Panel id, or a `mods:` placeholder (one window per matching panel).
    pub panel: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "yes")]
    pub open: bool,
    #[serde(default)]
    pub width: Option<f32>,
    #[serde(default)]
    pub height: Option<f32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Keys {
    /// Hold to talk. Settings (`[voice] ptt_key` in config.toml) override it.
    pub ptt: Option<String>,
    pub mute: Option<String>,
    pub deafen: Option<String>,
    pub reload: Option<String>,
    /// Open / close the settings page.
    pub settings: Option<String>,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            ptt: Some("F8".into()),
            mute: Some("Ctrl+Shift+M".into()),
            deafen: Some("Ctrl+Shift+D".into()),
            reload: Some("Ctrl+Shift+R".into()),
            settings: Some("Ctrl+Comma".into()),
        }
    }
}

/// What can be held to talk: a key, or a side mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttInput {
    Key(egui::Key),
    Pointer(egui::PointerButton),
}

impl PttInput {
    /// `F8`, `Space`, `Mouse4`/`Mouse5` (modifiers are ignored: you hold
    /// the key while doing other things).
    pub fn parse(spec: &str) -> Option<PttInput> {
        match spec.trim().to_ascii_lowercase().as_str() {
            "mouse4" | "mousex1" | "mouseback" => Some(PttInput::Pointer(egui::PointerButton::Extra1)),
            "mouse5" | "mousex2" | "mouseforward" => Some(PttInput::Pointer(egui::PointerButton::Extra2)),
            _ => parse_key(spec).map(|(_, k)| PttInput::Key(k)),
        }
    }

    pub fn down(&self, i: &egui::InputState) -> bool {
        match *self {
            PttInput::Key(k) => i.key_down(k),
            PttInput::Pointer(b) => i.pointer.button_down(b),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotName {
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

impl SlotName {
    pub const ALL: [SlotName; 5] = [SlotName::Top, SlotName::Bottom, SlotName::Left, SlotName::Right, SlotName::Center];

    pub fn as_str(self) -> &'static str {
        match self {
            SlotName::Top => "top",
            SlotName::Bottom => "bottom",
            SlotName::Left => "left",
            SlotName::Right => "right",
            SlotName::Center => "center",
        }
    }

    pub fn horizontal(self) -> bool {
        matches!(self, SlotName::Top | SlotName::Bottom)
    }
}

impl Screen {
    pub fn slot(&self, name: SlotName) -> Option<&Slot> {
        match name {
            SlotName::Top => self.top.as_ref(),
            SlotName::Bottom => self.bottom.as_ref(),
            SlotName::Left => self.left.as_ref(),
            SlotName::Right => self.right.as_ref(),
            SlotName::Center => self.center.as_ref(),
        }
    }

    /// Every panel id mentioned (excluding placeholders).
    pub fn explicit_panels(&self) -> Vec<String> {
        let mut out: Vec<String> = SlotName::ALL
            .iter()
            .filter_map(|s| self.slot(*s))
            .flat_map(|s| s.panels.iter().cloned())
            .chain(self.windows.iter().map(|w| w.panel.clone()))
            .chain(self.overlays.iter().cloned())
            .filter(|p| !p.starts_with("mods:"))
            .collect();
        out.dedup();
        out
    }

    pub fn placeholders(&self) -> Vec<String> {
        SlotName::ALL
            .iter()
            .filter_map(|s| self.slot(*s))
            .flat_map(|s| s.panels.iter())
            .chain(self.windows.iter().map(|w| &w.panel))
            .filter_map(|p| p.strip_prefix("mods:").map(str::to_owned))
            .collect()
    }
}

impl Layout {
    pub fn parse(text: &str) -> anyhow::Result<Layout> {
        let l: Layout = toml::from_str(text)?;
        if l.room.center.is_none() && l.room.left.is_none() && l.room.right.is_none() && l.room.top.is_none() && l.room.bottom.is_none() {
            anyhow::bail!("[room] has no slots; add at least `center = {{ panels = [\"core.messages\", \"core.composer\"] }}`");
        }
        Ok(l)
    }

    pub fn load(path: &Path) -> anyhow::Result<Layout> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Layout::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn settings(&self, panel: &str) -> &toml::Table {
        static EMPTY: std::sync::OnceLock<toml::Table> = std::sync::OnceLock::new();
        self.panel.get(panel).unwrap_or_else(|| EMPTY.get_or_init(toml::Table::new))
    }
}

/// Parse `Ctrl+Shift+M` / `F8` into egui terms. `Ctrl` and `Cmd` both mean
/// the platform command key, so layouts are portable.
pub fn parse_key(spec: &str) -> Option<(egui::Modifiers, egui::Key)> {
    let mut mods = egui::Modifiers::NONE;
    let mut key = None;
    for part in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "cmd" | "command" | "super" => mods.command = true,
            "shift" => mods.shift = true,
            "alt" | "option" => mods.alt = true,
            _ => key = egui::Key::from_name(part).or_else(|| egui::Key::from_name(&part.to_ascii_uppercase())),
        }
    }
    key.map(|k| (mods, k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_layouts_parse() {
        for (name, text) in crate::assets::LAYOUTS {
            let l = Layout::parse(text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(&l.name, name);
            assert!(l.lobby.center.is_some(), "{name} needs a lobby");
        }
    }

    #[test]
    fn placeholders_and_settings() {
        let l = Layout::parse(crate::assets::LAYOUTS[0].1).unwrap();
        assert!(l.room.placeholders().contains(&"*".to_string()));
        assert!(l.room.explicit_panels().contains(&"core.messages".to_string()));
        assert_eq!(l.settings("core.messages")["time_format"].as_str(), Some("%-I:%M %p"));
        assert!(l.settings("nope").is_empty());
    }

    #[test]
    fn keys() {
        let (m, k) = parse_key("Ctrl+Shift+M").unwrap();
        assert!(m.command && m.shift && !m.alt);
        assert_eq!(k, egui::Key::M);
        assert_eq!(parse_key("F8").unwrap().1, egui::Key::F8);
        assert!(parse_key("Ctrl+Nope").is_none());
        assert_eq!(parse_key("Ctrl+Comma").unwrap().1, egui::Key::Comma);
        // What the settings page stores round-trips.
        for k in [egui::Key::F8, egui::Key::Space, egui::Key::V, egui::Key::Backtick] {
            assert_eq!(PttInput::parse(k.name()), Some(PttInput::Key(k)), "{}", k.name());
        }
        assert_eq!(PttInput::parse("Mouse4"), Some(PttInput::Pointer(egui::PointerButton::Extra1)));
    }
}
