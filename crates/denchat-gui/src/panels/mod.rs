//! Panels: every visible piece of the app is one of these.
//!
//! The built-in UI has no special status. `core.messages`, `core.voice` and
//! friends are registered in the same [`Registry`] a custom shell would use,
//! placed by the same layout file, and removed the same way (delete the id
//! from the layout). Mod panels (`<mod id>/<panel id>`) are rendered from
//! declarative widgets that mods push through the control API.
//!
//! Panels read state and *queue* intents on [`Actions`]; the shell applies
//! them after the frame. That keeps panels free of locking concerns and
//! means everything a panel can do is expressible as data.

mod channels;
mod composer;
pub(crate) mod header;
mod lobby;
mod members;
mod messages;
mod modpanel;
mod stage;
mod status;
mod toasts;
mod voice;

use std::collections::{BTreeMap, HashSet};

use denchat_core::session::{Session, SessionState, VoiceStatsView};
use egui::{RichText, Ui};

pub use modpanel::{ModPanel, render_widgets};

use crate::theme::Palette;

/// One piece of UI that a layout can place.
pub trait Panel {
    /// Shown in tabs, window titles and the View menu.
    fn title(&self) -> String;
    /// Draw. Called every frame the panel is visible.
    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>);
    /// Whether the panel wants the leftover space in a stack (lists do, bars don't).
    fn fill(&self) -> bool {
        false
    }
}

/// Shell-level changes a panel can request.
#[derive(Clone, Debug, PartialEq)]
pub enum ShellCmd {
    SetTheme(String),
    SetLayout(String),
    TogglePanel(String),
    ReloadAssets,
    OpenConfigDir,
}

/// A deferred call against the session.
pub type SessionAction = Box<dyn FnOnce(&Session)>;

/// Intents queued during a frame.
#[derive(Default)]
pub struct Actions {
    pub(crate) session: Vec<SessionAction>,
    pub(crate) shell: Vec<ShellCmd>,
}

impl Actions {
    /// Run something against the session after this frame.
    pub fn session(&mut self, f: impl FnOnce(&Session) + 'static) {
        self.session.push(Box::new(f));
    }

    pub fn shell(&mut self, cmd: ShellCmd) {
        self.shell.push(cmd);
    }
}

/// Read-only facts about the shell, for menus.
pub struct ShellView {
    pub theme: String,
    pub layout: String,
    pub themes: Vec<(String, String)>,
    pub layouts: Vec<(String, String)>,
    /// `(id, title)` of panels placed on the current screen.
    pub placed: Vec<(String, String)>,
    pub hidden: HashSet<String>,
    pub layout_error: Option<String>,
    /// The push-to-talk key from the layout, for hints.
    pub ptt_key: Option<String>,
}

/// Everything a panel may look at this frame.
pub struct PanelCx<'a> {
    pub id: &'a str,
    pub session: &'a Session,
    pub state: &'a SessionState,
    pub palette: &'a Palette,
    pub voice: Option<&'a VoiceStatsView>,
    /// This panel's `[panel."<id>"]` table from the layout.
    pub settings: &'a toml::Table,
    pub shell: &'a ShellView,
    pub actions: &'a mut Actions,
    /// The panel sits inside a card the shell drew (so it shouldn't raise its
    /// own cards); false on the bare canvas or in the flat style.
    pub in_card: bool,
}

impl PanelCx<'_> {
    pub fn setting_str(&self, key: &str) -> Option<&str> {
        self.settings.get(key).and_then(|v| v.as_str())
    }

    pub fn setting_bool(&self, key: &str, default: bool) -> bool {
        self.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }

    pub fn setting_list(&self, key: &str) -> Option<Vec<String>> {
        self.settings.get(key).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
    }

    /// A section label (sentence case, muted).
    pub fn section(&self, ui: &mut Ui, text: &str) {
        crate::kit::section(ui, text, self.palette);
    }

    pub fn muted(&self, text: impl Into<String>) -> RichText {
        RichText::new(text).color(self.palette.muted)
    }

    pub fn small(&self, text: impl Into<String>) -> RichText {
        RichText::new(text).size(self.palette.small_size).color(self.palette.muted)
    }
}

pub type Factory = Box<dyn Fn() -> Box<dyn Panel>>;

/// Panel id → constructor. Custom shells can add their own Rust panels here.
pub struct Registry {
    factories: BTreeMap<String, (String, Factory)>,
}

impl Registry {
    pub fn empty() -> Registry {
        Registry { factories: BTreeMap::new() }
    }

    /// All built-in panels.
    pub fn with_builtins() -> Registry {
        let mut r = Registry::empty();
        r.register("core.lobby", "Host or join a room", || Box::new(lobby::Lobby::default()));
        r.register(header::ID, "Room header and View menu", || Box::new(header::Header));
        r.register("core.channels", "Text and voice channels", || Box::new(channels::Channels));
        r.register("core.voice", "Voice controls", || Box::new(voice::Voice::default()));
        r.register("core.members", "People in the room", || Box::new(members::Members));
        r.register("core.messages", "Messages in the current channel", || Box::new(messages::Messages));
        r.register("core.composer", "Message input with slash commands", || Box::new(composer::Composer::default()));
        r.register("core.stage", "Voice participants as tiles", || Box::new(stage::Stage));
        r.register("core.status", "Status bar", || Box::new(status::Status));
        r.register("core.toasts", "Pop-up notices", || Box::new(toasts::Toasts));
        r
    }

    pub fn register(&mut self, id: &str, description: &str, f: impl Fn() -> Box<dyn Panel> + 'static) {
        self.factories.insert(id.to_owned(), (description.to_owned(), Box::new(f)));
    }

    pub fn unregister(&mut self, id: &str) {
        self.factories.remove(id);
    }

    pub fn contains(&self, id: &str) -> bool {
        self.factories.contains_key(id)
    }

    pub fn create(&self, id: &str) -> Option<Box<dyn Panel>> {
        self.factories.get(id).map(|(_, f)| f())
    }

    /// `(id, description)` for docs and the View menu.
    pub fn list(&self) -> Vec<(String, String)> {
        self.factories.iter().map(|(k, (d, _))| (k.clone(), d.clone())).collect()
    }
}

/// Shown for ids nobody provides (typo, or a mod that isn't installed).
pub struct Missing(pub String);

impl Panel for Missing {
    fn title(&self) -> String {
        self.0.clone()
    }

    fn show(&mut self, ui: &mut Ui, cx: &mut PanelCx<'_>) {
        ui.label(cx.small(format!("panel \"{}\" isn't available — is its mod installed?", self.0)));
    }
}

/// A small filled circle (status / speaking indicator). Drawn, not a glyph,
/// so it works with any font a theme picks.
pub fn dot(ui: &mut Ui, color: egui::Color32, radius: f32) -> egui::Response {
    let h = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(radius * 2.0 + 2.0, h), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
    resp
}

pub use crate::kit::nick_color;
