//! The shell: turns a layout file + a theme file + registered panels into a window.
//!
//! The shell itself draws nothing but slots. Everything inside them is a
//! [`Panel`](crate::panels::Panel) looked up by id, so the built-in UI can be
//! rearranged, replaced or switched off entirely from TOML.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use denchat_core::proto::NoticeLevel;
use denchat_core::session::{ConnState, Session, SessionState, VoiceStatsView};
use denchat_core::ui::PanelContent;
use egui::{Align, Align2, CornerRadius, Frame, Id, Layout as ELayout, Margin, Order, Stroke, Ui, ViewportCommand};

use crate::agent::{self, AgentPlugin, Kind};
use crate::catalog::{Catalog, Entry};
use crate::layout::{Layout, PttInput, Screen, ShellStyle, Slot, SlotMode, SlotName, parse_key};
use crate::panels::{Actions, Missing, ModPanel, Panel, PanelCx, Registry, ShellCmd, ShellView};
use crate::pictures::{PathPrompt, Pictures};
use crate::theme::{Palette, Theme};

/// Used when the configured theme is missing (keep in sync with `UiConfig::default`).
pub(crate) const DEFAULT_THEME: &str = "catppuccin";
const HIDDEN_KEY: &str = "den.hidden_panels";
const POLL: Duration = Duration::from_millis(800);
/// The panel the settings page shows.
const SETTINGS_PANEL: &str = "core.settings";

struct Instance {
    missing: bool,
    panel: Box<dyn Panel>,
}

#[derive(Clone, Copy, PartialEq)]
enum KeyAction {
    Mute,
    Deafen,
    Reload,
    Settings,
}

pub struct DenApp {
    session: Session,
    registry: Registry,
    catalog: Catalog,
    theme_name: String,
    layout_name: String,
    layout: Layout,
    palette: Palette,
    layout_error: Option<String>,
    instances: HashMap<String, Instance>,
    hidden: HashSet<String>,
    windows_open: HashMap<String, bool>,
    /// Floating windows: auto-stacked until the user drags one.
    floating: HashMap<String, Floating>,
    watched: Vec<(PathBuf, Option<SystemTime>)>,
    last_poll: Instant,
    last_scan: Instant,
    shortcuts: Vec<(KeyAction, egui::Modifiers, egui::Key)>,
    ptt_down: bool,
    settings_open: bool,
    title: String,
    /// `DEN_SCREENSHOT=<png>`: capture once the UI has settled.
    auto_shot: Option<(Instant, PathBuf)>,
    /// Set by the screenshot callback when `DEN_SCREENSHOT_EXIT=1`.
    exit_now: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A window, or headless (no file dialogs there: a path prompt instead).
    kind: Kind,
    pictures: Pictures,
    path_prompt: PathPrompt,
}

/// UI zoom: `DEN_SCALE` (or `--scale`) wins over `[ui] scale`.
fn scale(configured: f32) -> f32 {
    std::env::var("DEN_SCALE").ok().and_then(|v| v.parse::<f32>().ok()).filter(|s| (0.25..=5.0).contains(s)).unwrap_or(configured)
}

impl DenApp {
    pub fn new(cc: &eframe::CreationContext<'_>, session: Session, registry: Registry) -> DenApp {
        DenApp::create(&cc.egui_ctx, cc.storage, session, registry, Kind::Window)
    }

    /// Build the app on any egui context: eframe's window, or the headless runner.
    pub fn create(ctx: &egui::Context, storage: Option<&dyn eframe::Storage>, session: Session, registry: Registry, kind: Kind) -> DenApp {
        let repaint = ctx.clone();
        session.on_change(move || repaint.request_repaint());
        agent::attach(ctx, session.ui(), kind);
        // Panels holding View → Panels: older builds let them be hidden,
        // which left no way to bring anything back.
        let hidden: HashSet<String> = storage
            .and_then(|s| eframe::get_value::<Vec<String>>(s, HIDDEN_KEY))
            .unwrap_or_default()
            .into_iter()
            .filter(|id| crate::panels::header::can_hide(id))
            .collect();
        let catalog = Catalog::scan(session.paths(), &session.mods());
        let ui_cfg = session.config().ui;
        let zoom = scale(ui_cfg.scale);
        if (zoom - 1.0).abs() > f32::EPSILON && zoom > 0.25 {
            ctx.set_zoom_factor(zoom);
        }
        let mut app = DenApp {
            session,
            registry,
            catalog,
            theme_name: String::new(),
            layout_name: String::new(),
            layout: Layout::parse(crate::assets::LAYOUTS[0].1).expect("built-in layout parses"),
            palette: Theme::default().palette(),
            layout_error: None,
            instances: HashMap::new(),
            hidden,
            windows_open: HashMap::new(),
            floating: HashMap::new(),
            watched: Vec::new(),
            last_poll: Instant::now(),
            last_scan: Instant::now(),
            shortcuts: Vec::new(),
            ptt_down: false,
            settings_open: false,
            title: String::new(),
            auto_shot: std::env::var_os("DEN_SCREENSHOT").map(|p| {
                let delay = std::env::var("DEN_SCREENSHOT_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
                (Instant::now() + Duration::from_millis(delay), PathBuf::from(p))
            }),
            exit_now: Default::default(),
            kind,
            pictures: Pictures::default(),
            path_prompt: PathPrompt::default(),
        };
        app.load_theme(ctx, &ui_cfg.theme);
        app.load_layout(&ui_cfg.layout);
        app
    }

    // ----- assets --------------------------------------------------------

    fn load_theme(&mut self, ctx: &egui::Context, name: &str) {
        let (entry, fallback) = match self.catalog.theme(name) {
            Some(e) => (e.clone(), false),
            None => (self.catalog.theme(DEFAULT_THEME).cloned().unwrap_or_else(builtin_theme_entry), true),
        };
        if fallback {
            self.session.notice(NoticeLevel::Warn, format!("theme \"{name}\" not found; using {}", entry.name));
        }
        match entry.text().and_then(|t| Theme::parse(&t)) {
            Ok(theme) => {
                theme.apply(ctx, entry.path().and_then(|p| p.parent()));
                self.palette = theme.palette();
                self.theme_name = entry.name.clone();
            }
            Err(e) => {
                self.session.notice(NoticeLevel::Error, format!("theme {}: {e:#}", entry.name));
                if self.theme_name.is_empty() {
                    let t = Theme::default();
                    t.apply(ctx, None);
                    self.palette = t.palette();
                    self.theme_name = DEFAULT_THEME.into();
                }
            }
        }
        self.rewatch();
    }

    fn load_layout(&mut self, name: &str) {
        let entry = match self.catalog.layout(name) {
            Some(e) => e.clone(),
            None => {
                self.session.notice(NoticeLevel::Warn, format!("layout \"{name}\" not found; using default"));
                self.catalog.layout("default").cloned().unwrap_or_else(builtin_layout_entry)
            }
        };
        match entry.text().and_then(|t| Layout::parse(&t)) {
            Ok(layout) => {
                self.layout = layout;
                self.layout_name = entry.name.clone();
                self.layout_error = None;
                self.windows_open.clear();
                self.floating.clear();
                self.bind_keys();
            }
            Err(e) => {
                // Keep the last good layout on screen and say what's wrong.
                let msg = format!("layout {}: {e:#}", entry.name);
                self.session.notice(NoticeLevel::Error, msg.clone());
                self.layout_error = Some(msg);
                if self.layout_name.is_empty() {
                    self.layout_name = "default".into();
                }
            }
        }
        self.rewatch();
    }

    fn bind_keys(&mut self) {
        let k = &self.layout.keys;
        self.shortcuts = [
            (KeyAction::Mute, &k.mute),
            (KeyAction::Deafen, &k.deafen),
            (KeyAction::Reload, &k.reload),
            (KeyAction::Settings, &k.settings),
        ]
        .into_iter()
        .filter_map(|(a, spec)| spec.as_deref().and_then(parse_key).map(|(m, key)| (a, m, key)))
        .collect();
    }

    /// The push-to-talk key in effect: settings first, then the layout.
    fn ptt_spec(&self) -> Option<String> {
        self.session.config().voice.ptt_key.or_else(|| self.layout.keys.ptt.clone())
    }

    fn set_settings_open(&mut self, open: bool) {
        if self.settings_open == open {
            return;
        }
        self.settings_open = open;
        if !open {
            // The page starts a mic test while it shows; release the devices.
            self.session.voice_test(false);
        }
    }

    /// Track the files behind the current theme and layout. A file we were
    /// already watching keeps its old timestamp, so an edit that lands while
    /// something else reloads is still noticed on the next poll.
    fn rewatch(&mut self) {
        let mut files = Vec::new();
        for e in [self.catalog.theme(&self.theme_name), self.catalog.layout(&self.layout_name)].into_iter().flatten() {
            if let Some(p) = e.path() {
                let seen = self.watched.iter().find(|(q, _)| q == p).map(|(_, t)| *t);
                files.push((p.to_owned(), seen.unwrap_or_else(|| mtime(p))));
            }
        }
        self.watched = files;
    }

    /// Hot reload: re-read changed files, pick up new ones, follow config changes made elsewhere (API, CLI).
    fn poll(&mut self, ctx: &egui::Context) {
        let cfg = self.session.config().ui;
        if self.last_poll.elapsed() < POLL {
            return;
        }
        self.last_poll = Instant::now();
        if self.last_scan.elapsed() > Duration::from_secs(4) {
            self.catalog = Catalog::scan(self.session.paths(), &self.session.mods());
            self.last_scan = Instant::now();
        }
        if cfg.theme != self.theme_name {
            self.load_theme(ctx, &cfg.theme);
        }
        if cfg.layout != self.layout_name {
            self.load_layout(&cfg.layout);
        }
        if !cfg.hot_reload {
            return;
        }
        let changed: Vec<PathBuf> = self.watched.iter().filter(|(p, t)| mtime(p) != *t).map(|(p, _)| p.clone()).collect();
        if changed.is_empty() {
            return;
        }
        for (p, t) in &mut self.watched {
            if changed.contains(p) {
                *t = mtime(p);
            }
        }
        self.catalog = Catalog::scan(self.session.paths(), &self.session.mods());
        let theme = self.theme_name.clone();
        let layout = self.layout_name.clone();
        if self.catalog.theme(&theme).and_then(Entry::path).is_some_and(|p| changed.iter().any(|c| c == p)) {
            self.load_theme(ctx, &theme);
            self.session.ui().toast(NoticeLevel::Info, format!("reloaded theme {theme}"));
        }
        if self.catalog.layout(&layout).and_then(Entry::path).is_some_and(|p| changed.iter().any(|c| c == p)) {
            self.load_layout(&layout);
            if self.layout_error.is_none() {
                self.session.ui().toast(NoticeLevel::Info, format!("reloaded layout {layout}"));
            }
        }
        self.rewatch();
    }

    fn reload_all(&mut self, ctx: &egui::Context) {
        self.catalog = Catalog::scan(self.session.paths(), &self.session.mods());
        let (t, l) = (self.theme_name.clone(), self.layout_name.clone());
        self.load_theme(ctx, &t);
        self.load_layout(&l);
        self.session.ui().toast(NoticeLevel::Info, format!("reloaded {t} + {l}"));
    }

    // ----- input ---------------------------------------------------------

    fn keys(&mut self, ctx: &egui::Context) {
        let mut fired = Vec::new();
        ctx.input_mut(|i| {
            for (a, m, k) in &self.shortcuts {
                if i.consume_key(*m, *k) {
                    fired.push(*a);
                }
            }
        });
        for a in fired {
            match a {
                KeyAction::Mute => {
                    let muted = self.session.state().voice.muted;
                    self.session.set_muted(!muted);
                }
                KeyAction::Deafen => {
                    let d = self.session.state().voice.deafened;
                    self.session.set_deafened(!d);
                }
                KeyAction::Reload => self.reload_all(ctx),
                KeyAction::Settings => self.set_settings_open(!self.settings_open),
            }
        }
        if let Some(input) = self.ptt_spec().as_deref().and_then(PttInput::parse) {
            let down = ctx.input(|i| input.down(i));
            if down != self.ptt_down {
                self.ptt_down = down;
                let s = self.session.state();
                let active = (s.voice.channel.is_some() || s.voice.testing) && s.voice.mode == "push_to_talk";
                drop(s);
                if active || !down {
                    self.session.set_ptt(down);
                }
            }
        }
    }

    fn apply_shell(&mut self, ctx: &egui::Context, cmd: ShellCmd) {
        match cmd {
            ShellCmd::SetTheme(name) => {
                self.load_theme(ctx, &name);
                let n = self.theme_name.clone();
                self.session.update_config(|c| c.ui.theme = n);
            }
            ShellCmd::SetLayout(name) => {
                self.load_layout(&name);
                let n = self.layout_name.clone();
                self.session.update_config(|c| c.ui.layout = n);
            }
            ShellCmd::TogglePanel(id) => {
                if !self.hidden.remove(&id) && crate::panels::header::can_hide(&id) {
                    self.hidden.insert(id);
                }
            }
            ShellCmd::ReloadAssets => self.reload_all(ctx),
            ShellCmd::OpenSettings => self.set_settings_open(true),
            ShellCmd::CloseSettings => self.set_settings_open(false),
            ShellCmd::PickPicture(target) => match self.kind {
                Kind::Window => crate::pictures::pick_with_dialog(&self.session, target),
                Kind::Headless => self.path_prompt.open = Some(target),
            },
            ShellCmd::OpenConfigDir => {
                let paths = self.session.paths();
                for d in [paths.themes_dir(), paths.layouts_dir(), paths.mods_dir()] {
                    let _ = std::fs::create_dir_all(d);
                }
                if let Err(e) = open_path(&paths.config) {
                    self.session.notice(NoticeLevel::Error, format!("could not open {}: {e}", paths.config.display()));
                }
            }
        }
    }

    /// `DEN_SCREENSHOT=<png>`: one screenshot once the UI has settled (then
    /// exit with `DEN_SCREENSHOT_EXIT=1`). `ui.screenshot` goes through the
    /// agent plugin the same way.
    fn screenshots(&mut self, ctx: &egui::Context) {
        use std::sync::atomic::Ordering;
        if self.exit_now.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        let Some((at, path)) = &self.auto_shot else { return };
        if Instant::now() < *at {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
            return;
        }
        let exit = std::env::var("DEN_SCREENSHOT_EXIT").is_ok_and(|v| v == "1");
        let (call, rx) = denchat_core::ui::FrontendCall::new("ui.screenshot", serde_json::json!({"path": path}));
        ctx.with_plugin::<AgentPlugin, _>(|p| p.submit(call));
        self.auto_shot = None;
        let (session, exit_now, repaint) = (self.session.clone(), self.exit_now.clone(), ctx.clone());
        std::thread::spawn(move || {
            match rx.blocking_recv() {
                Ok(Ok(v)) => tracing::info!("screenshot saved to {}", v["path"].as_str().unwrap_or("?")),
                Ok(Err(e)) => session.notice(NoticeLevel::Error, format!("screenshot: {e}")),
                Err(_) => {}
            }
            if exit {
                exit_now.store(true, Ordering::Relaxed);
            }
            repaint.request_repaint();
        });
        ctx.request_repaint();
    }

    // ----- drawing -------------------------------------------------------

    fn sync_title(&mut self, ctx: &egui::Context, state: &SessionState) {
        let title = match (&state.server, &state.conn) {
            (Some(s), ConnState::Connected { .. }) => format!("{} — Den", s.name),
            (Some(s), _) => format!("{} (reconnecting) — Den", s.name),
            _ => "Den".to_owned(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            ctx.with_plugin::<AgentPlugin, _>(|p| p.set_title(&title));
            self.title = title;
        }
    }
}

impl eframe::App for DenApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Agent calls need frames even while eframe is holding them back (see
        // `AgentPlugin::wants_offscreen_pass`): run one here and draw it on the CPU.
        if !ctx.with_plugin::<AgentPlugin, _>(|p| p.wants_offscreen_pass()).unwrap_or(false) {
            return;
        }
        let mut raw = ctx.input(|i| i.raw.clone());
        raw.events.clear();
        raw.hovered_files.clear();
        raw.dropped_files.clear();
        raw.time = None;
        ctx.with_plugin::<AgentPlugin, _>(|p| p.offscreen = true);
        let out = ctx.run_ui(raw, |ui| self.frame(ui));
        ctx.with_plugin::<AgentPlugin, _>(|p| p.offscreen = false);
        // Hand what the pass asked for (window commands, copies) to eframe.
        ctx.output_mut(|o| o.append(out.platform_output));
        for (id, vo) in out.viewport_output {
            for cmd in vo.commands {
                ctx.send_viewport_cmd_to(id, cmd);
            }
            ctx.request_repaint_after_for(vo.repaint_delay, id);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let mut hidden: Vec<String> = self.hidden.iter().cloned().collect();
        hidden.sort();
        eframe::set_value(storage, HIDDEN_KEY, &hidden);
    }
}

impl DenApp {
    /// One frame of the whole app into the root `ui`.
    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if self.session.config().ui.hot_reload {
            ctx.request_repaint_after(POLL);
        }
        if let Some(open) = self.session.ui().take_settings_request() {
            self.set_settings_open(open);
        }
        self.keys(&ctx);
        if self.session.ui().take_raise_request() {
            // `den` started again on this profile: show this window instead.
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        self.screenshots(&ctx);

        let session = self.session.clone();
        let voice = session.voice_stats();
        if let Some(v) = &voice {
            // Level meters and talking rings: smooth while anyone talks.
            let lively = v.transmitting || v.speakers.iter().any(|s| s.speaking) || (self.settings_open && v.input_level > 0.001);
            ctx.request_repaint_after(Duration::from_millis(if lively { 33 } else { 66 }));
        }
        let ptt_key = self.ptt_spec();
        let mut actions = Actions::default();
        {
            let state = session.state();
            self.sync_title(&ctx, &state);
            let in_room = state.server.is_some();
            let screen = if in_room { &self.layout.room } else { &self.layout.lobby };
            let mod_panels = session.ui().panels();
            let plan = Plan::build(screen, &mod_panels, &self.hidden);

            let shell = ShellView {
                theme: self.theme_name.clone(),
                layout: self.layout_name.clone(),
                themes: self.catalog.themes.iter().map(|e| (e.name.clone(), e.origin.clone())).collect(),
                layouts: self.catalog.layouts.iter().map(|e| (e.name.clone(), e.origin.clone())).collect(),
                placed: Vec::new(),
                hidden: self.hidden.clone(),
                layout_error: self.layout_error.clone(),
                ptt_key,
                layout_ptt_key: self.layout.keys.ptt.clone(),
                settings_open: self.settings_open,
            };
            let mut r = Render {
                session: &session,
                state: &state,
                palette: &self.palette,
                voice: voice.as_ref(),
                layout: &self.layout,
                shell,
                registry: &self.registry,
                instances: &mut self.instances,
                mod_panels: &mod_panels,
                actions: &mut actions,
                in_card: false,
                pictures: &self.pictures,
            };
            r.shell.placed = plan.all.iter().map(|id| (id.clone(), r.title(id))).collect();
            r.screen(ui, screen, &plan, &mut self.windows_open, &mut self.floating);
            if self.settings_open && r.registry.contains(SETTINGS_PANEL) && !r.shell.hidden.contains(SETTINGS_PANEL) {
                r.settings_page(ui);
            }
        }

        if let Some((target, path)) = self.path_prompt.show(&ctx) {
            target.apply(&self.session, Some(path));
        }
        for f in actions.session.drain(..) {
            f(&self.session);
        }
        for cmd in actions.shell.drain(..) {
            self.apply_shell(&ctx, cmd);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Floating {
    moved: bool,
    height: f32,
}

/// Which panel ids go where on this frame, after expanding `mods:` placeholders
/// and dropping hidden panels.
struct Plan {
    slots: BTreeMap<&'static str, Vec<String>>,
    windows: Vec<(String, usize)>,
    overlays: Vec<String>,
    /// Every placed panel (for the View → Panels menu), including hidden ones.
    all: Vec<String>,
}

impl Plan {
    fn build(screen: &Screen, mod_panels: &BTreeMap<String, PanelContent>, hidden: &HashSet<String>) -> Plan {
        let explicit: HashSet<String> = screen.explicit_panels().into_iter().collect();
        let placeholders = screen.placeholders();
        let claimed_slots: HashSet<&str> = placeholders.iter().map(String::as_str).filter(|p| *p != "*").collect();
        let expand = |ph: &str| -> Vec<String> {
            mod_panels
                .iter()
                .filter(|(id, _)| !explicit.contains(*id))
                .filter(|(_, c)| match ph {
                    "*" => !c.slot.as_deref().is_some_and(|s| claimed_slots.contains(s)),
                    slot => c.slot.as_deref() == Some(slot),
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        let resolve = |ids: &[String]| -> Vec<String> {
            let mut out = Vec::new();
            for id in ids {
                match id.strip_prefix("mods:") {
                    Some(ph) => out.extend(expand(ph)),
                    None => out.push(id.clone()),
                }
            }
            out
        };
        let mut all = Vec::new();
        let mut slots = BTreeMap::new();
        for name in SlotName::ALL {
            if let Some(slot) = screen.slot(name) {
                let ids = resolve(&slot.panels);
                all.extend(ids.iter().cloned());
                slots.insert(name.as_str(), ids.into_iter().filter(|i| !hidden.contains(i)).collect());
            }
        }
        let mut windows = Vec::new();
        for (i, w) in screen.windows.iter().enumerate() {
            for id in resolve(std::slice::from_ref(&w.panel)) {
                all.push(id.clone());
                if !hidden.contains(&id) {
                    windows.push((id, i));
                }
            }
        }
        let overlays = screen.overlays.iter().filter(|o| !hidden.contains(*o)).cloned().collect();
        all.extend(screen.overlays.iter().cloned());
        let mut seen = HashSet::new();
        all.retain(|id| seen.insert(id.clone()));
        Plan { slots, windows, overlays, all }
    }

    fn slot(&self, name: SlotName) -> &[String] {
        self.slots.get(name.as_str()).map(Vec::as_slice).unwrap_or(&[])
    }
}

struct Render<'a> {
    session: &'a Session,
    state: &'a SessionState,
    palette: &'a Palette,
    voice: Option<&'a VoiceStatsView>,
    layout: &'a Layout,
    shell: ShellView,
    registry: &'a Registry,
    instances: &'a mut HashMap<String, Instance>,
    mod_panels: &'a BTreeMap<String, PanelContent>,
    actions: &'a mut Actions,
    /// The slot being drawn is a card (panels inside shouldn't draw their own).
    in_card: bool,
    pictures: &'a Pictures,
}

impl Render<'_> {
    fn instance(&mut self, id: &str) -> &mut Instance {
        let available = self.registry.contains(id) || self.mod_panels.contains_key(id);
        let stale = self.instances.get(id).is_some_and(|i| i.missing && available);
        if stale || !self.instances.contains_key(id) {
            let panel: Box<dyn Panel> = match self.registry.create(id) {
                Some(p) => p,
                None if self.mod_panels.contains_key(id) => Box::new(ModPanel { id: id.to_owned() }),
                None => Box::new(Missing(id.to_owned())),
            };
            self.instances.insert(id.to_owned(), Instance { missing: !available, panel });
        }
        self.instances.get_mut(id).expect("just inserted")
    }

    fn title(&mut self, id: &str) -> String {
        if let Some(c) = self.mod_panels.get(id) {
            return c.title.clone();
        }
        self.instance(id).panel.title()
    }

    fn fills(&mut self, id: &str) -> bool {
        match self.layout.settings(id).get("fill").and_then(|v| v.as_bool()) {
            Some(f) => f,
            None => self.instance(id).panel.fill(),
        }
    }

    fn panel(&mut self, ui: &mut Ui, id: &str) {
        let session = self.session;
        let state = self.state;
        let palette = self.palette;
        let voice = self.voice;
        let settings = self.layout.settings(id);
        // Split borrows: the instance map and the actions queue are disjoint fields.
        let available = self.registry.contains(id) || self.mod_panels.contains_key(id);
        let stale = self.instances.get(id).is_some_and(|i| i.missing && available);
        if stale || !self.instances.contains_key(id) {
            self.instance(id);
        }
        let inst = self.instances.get_mut(id).expect("instance exists");
        let mut cx = PanelCx {
            id,
            session,
            state,
            palette,
            voice,
            settings,
            shell: &self.shell,
            actions: self.actions,
            in_card: self.in_card,
            pictures: self.pictures,
        };

        ui.push_id(("panel", id), |ui| {
            // A node in the accessibility tree that groups the panel's widgets,
            // so `den ui tree`/`click` can tell panels apart.
            let node = ui.unique_id();
            ui.ctx().accesskit_node_builder(node, |n| {
                n.set_role(egui::accesskit::Role::Group);
                n.set_author_id(format!("{}{id}", agent::PANEL_TAG));
            });
            inst.panel.show(ui, &mut cx);
            let r = ui.min_rect();
            ui.ctx().accesskit_node_builder(node, |n| {
                n.set_bounds(egui::accesskit::Rect { x0: r.min.x.into(), y0: r.min.y.into(), x1: r.max.x.into(), y1: r.max.y.into() });
            });
        });
    }

    /// The settings page: a modal over the whole window.
    fn settings_page(&mut self, ui: &mut Ui) {
        let p = *self.palette;
        let ctx = ui.ctx().clone();
        let screen = ctx.content_rect();
        let size = egui::vec2((screen.width() - 48.0).clamp(320.0, 760.0), (screen.height() - 64.0).clamp(280.0, 640.0));
        let frame = Frame::new()
            .fill(p.panel)
            .corner_radius(CornerRadius::same(p.card_radius.min(18)))
            .inner_margin(Margin::symmetric(18, 14))
            .stroke(Stroke::new(1.0, p.line))
            .shadow(p.shadow);
        let modal = egui::Modal::new(Id::unique("den-settings"))
            .frame(frame)
            .backdrop_color(crate::theme::alpha(egui::Color32::BLACK, 0.45))
            .show(&ctx, |ui| {
                ui.set_width(size.x);
                ui.set_height(size.y);
                self.in_card = true;
                self.panel(ui, SETTINGS_PANEL);
            });
        if modal.should_close() {
            self.actions.shell(ShellCmd::CloseSettings);
        }
    }

    fn screen(
        &mut self,
        ui: &mut Ui,
        screen: &Screen,
        plan: &Plan,
        windows_open: &mut HashMap<String, bool>,
        floating: &mut HashMap<String, Floating>,
    ) {
        let p = *self.palette;
        let pad = p.padding.clamp(0.0, 40.0) as i8;
        let full = ui.max_rect();
        let cards = self.layout.shell.style == ShellStyle::Cards;
        let g = self.layout.shell.gap.clamp(0.0, 40.0) as i8;
        let shown = |name| screen.slot(name).is_some_and(|s| s.visible && !plan.slot(name).is_empty());
        let (side_max, drop_right) = side_widths(full.width(), shown(SlotName::Left), shown(SlotName::Right));
        for name in [SlotName::Top, SlotName::Bottom, SlotName::Left, SlotName::Right] {
            let Some(slot) = screen.slot(name) else { continue };
            let ids = plan.slot(name);
            if ids.is_empty() || !slot.visible || (name == SlotName::Right && drop_right) {
                continue;
            }
            let id = ("slot", name.as_str());
            let frame = if cards {
                // Gaps on the window edge side; the centre slot supplies the inner gap.
                let m = match name {
                    SlotName::Top => Margin { left: g, right: g, top: g, bottom: 0 },
                    SlotName::Bottom => Margin { left: g, right: g, top: 0, bottom: g },
                    SlotName::Left => Margin { left: g, right: 0, top: g, bottom: g },
                    _ => Margin { left: 0, right: g, top: g, bottom: g },
                };
                Frame::new().fill(p.canvas).inner_margin(m)
            } else {
                Frame::new().fill(p.panel).inner_margin(Margin::symmetric(pad, if name.horizontal() { pad / 2 } else { pad }))
            };
            let panel = match name {
                SlotName::Top => egui::Panel::top(id),
                SlotName::Bottom => egui::Panel::bottom(id),
                SlotName::Left => egui::Panel::left(id),
                SlotName::Right => egui::Panel::right(id),
                SlotName::Center => unreachable!(),
            };
            let resizable = slot.resizable.unwrap_or(!name.horizontal());
            let mut panel = panel.frame(frame).resizable(resizable).show_separator_line(!cards);
            if let Some(size) = slot.size {
                panel = panel.default_size(size);
            } else if !name.horizontal() {
                panel = panel.default_size(220.0);
            }
            if !name.horizontal() {
                panel = panel.size_range(SIDE_MIN..=side_max);
            }
            let carded = cards && slot.card.unwrap_or(true);
            panel.show(ui, |ui| {
                self.in_card = carded;
                if carded {
                    let m = if name.horizontal() { Margin::symmetric(pad + 2, pad / 2 + 2) } else { Margin::symmetric(pad, pad + 2) };
                    slot_card(&p, p.panel, m).show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        self.slot(ui, name, slot, ids);
                    });
                } else {
                    self.slot(ui, name, slot, ids);
                }
            });
        }
        let center_ids = plan.slot(SlotName::Center).to_vec();
        let frame = if cards {
            Frame::new().fill(p.canvas).inner_margin(Margin::same(g))
        } else {
            Frame::new().fill(p.bg).inner_margin(Margin::same(pad))
        };
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if let Some(slot) = screen.center.as_ref().filter(|s| s.visible && !center_ids.is_empty()) {
                let carded = cards && slot.card.unwrap_or(true);
                self.in_card = carded;
                if carded {
                    slot_card(&p, p.bg, Margin { left: 10, right: 10, top: 4, bottom: 10 }).show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        self.slot(ui, SlotName::Center, slot, &center_ids);
                    });
                } else {
                    self.slot(ui, SlotName::Center, slot, &center_ids);
                }
            }
        });
        self.in_card = true;

        // Windows stack down the right-hand side (then further columns to the left),
        // re-flowed every frame as their contents grow, until the user drags one.
        let top = full.top() + 56.0;
        let (mut next_top, mut column_right) = (top, full.right() - 20.0);
        for (id, spec_index) in &plan.windows {
            let spec = &screen.windows[*spec_index];
            let open = windows_open.entry(id.clone()).or_insert(spec.open);
            if !*open {
                continue;
            }
            let title = spec.title.clone().unwrap_or_else(|| self.title(id));
            let width = spec.width.unwrap_or(260.0);
            let last = floating.get(id).copied().unwrap_or_default();
            if next_top > top && next_top + last.height.max(80.0) > full.bottom() - 36.0 {
                (next_top, column_right) = (top, column_right - width - 16.0);
            }
            let pos = egui::pos2(column_right - width, next_top);
            let auto = !last.moved;
            let window_id = Id::unique(("window", id.as_str()));
            let ctx = ui.ctx().clone();
            if auto && let Some(mut state) = egui::AreaState::load(&ctx, window_id) {
                // Window::current_pos only applies on the first frame (title-bar drag mode
                // restores the stored position), so move the stored position instead.
                state.pivot_pos = Some(pos);
                ctx.memory_mut(|m| m.areas_mut().set_state(egui::LayerId::new(Order::Middle, window_id), state));
            }
            let mut w = egui::Window::new(title).id(window_id).open(open).resizable(true).default_pos(pos).default_width(width);
            if auto {
                // egui's own screen constraint uses a stale size and drags the
                // window up over its neighbour; the column wrap above keeps it on screen.
                w = w.current_pos(pos).constrain(false);
            }
            if let Some(h) = spec.height {
                w = w.default_height(h);
            }
            if let Some(shown) = w.show(&ctx, |ui| self.panel(ui, id)) {
                let r = shown.response.rect;
                let f = floating.entry(id.clone()).or_default();
                if auto && (f.height - r.height()).abs() > 0.5 {
                    // Re-flow next frame with the new size.
                    ctx.request_repaint();
                }
                f.height = r.height();
                if shown.response.dragged() {
                    f.moved = true;
                } else if auto {
                    next_top = r.bottom() + 12.0;
                }
            }
            if !*open {
                // Closing a window hides the panel; View → Panels brings it back.
                *open = true;
                self.actions.shell(ShellCmd::TogglePanel(id.clone()));
            }
        }
        if !plan.overlays.is_empty() {
            let ctx = ui.ctx().clone();
            egui::Area::new(Id::unique("overlays")).anchor(Align2::RIGHT_BOTTOM, [-14.0, -36.0]).order(Order::Foreground).show(
                &ctx,
                |ui| {
                    ui.with_layout(ELayout::top_down(Align::Max), |ui| {
                        for id in &plan.overlays {
                            self.panel(ui, id);
                        }
                    });
                },
            );
        }
    }

    /// Lay out one slot: a vertical stack (with the first `fill` panel taking
    /// leftover space) or tabs.
    fn slot(&mut self, ui: &mut Ui, name: SlotName, slot: &Slot, ids: &[String]) {
        if slot.mode == SlotMode::Tabs && ids.len() > 1 {
            let key = Id::unique(("tab", name.as_str()));
            let mut current: String = ui.data(|d| d.get_temp::<String>(key)).filter(|c| ids.contains(c)).unwrap_or_else(|| ids[0].clone());
            ui.horizontal_wrapped(|ui| {
                for id in ids {
                    let title = self.title(id);
                    if ui.selectable_label(current == *id, title).clicked() {
                        current = id.clone();
                    }
                }
            });
            ui.separator();
            ui.data_mut(|d| d.insert_temp(key, current.clone()));
            self.stack(ui, name, std::slice::from_ref(&current));
            return;
        }
        self.stack(ui, name, ids);
    }

    fn stack(&mut self, ui: &mut Ui, name: SlotName, ids: &[String]) {
        let fill = ids.iter().position(|id| self.fills(id));
        let cards = self.layout.shell.style == ShellStyle::Cards;
        let (in_card, line) = (self.in_card, self.palette.line);
        let gap = |ui: &mut Ui, i: usize| {
            if i == 0 || name.horizontal() || name == SlotName::Center {
                return;
            }
            if !cards {
                ui.add_space(2.0);
                ui.separator();
            } else if in_card {
                // A hairline between panels sharing a card.
                ui.add_space(10.0);
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, line));
                ui.add_space(10.0);
            } else {
                ui.add_space(6.0);
            }
        };
        let Some(fi) = fill else {
            if name.horizontal() {
                for id in ids {
                    self.panel(ui, id);
                }
            } else {
                egui::ScrollArea::vertical().id_salt(("slot-scroll", name.as_str())).auto_shrink([false, false]).show(ui, |ui| {
                    for (i, id) in ids.iter().enumerate() {
                        gap(ui, i);
                        self.panel(ui, id);
                    }
                });
            }
            return;
        };
        for (i, id) in ids[..fi].iter().enumerate() {
            gap(ui, i);
            self.panel(ui, id);
        }
        // Panels after the filler hug the bottom edge, in list order.
        for (j, id) in ids[fi + 1..].iter().enumerate().rev() {
            egui::Panel::bottom(("after", name.as_str(), j))
                .frame(Frame::NONE.inner_margin(Margin { top: if cards { 8 } else { 6 }, ..Margin::ZERO }))
                .resizable(false)
                .show_separator_line(false)
                .show(ui, |ui| self.panel(ui, id));
        }
        if fi > 0 {
            gap(ui, fi);
        }
        let id = ids[fi].clone();
        egui::CentralPanel::no_frame().show(ui, |ui| self.panel(ui, &id));
    }
}

/// The frame for a slot drawn as a card.
fn slot_card(p: &Palette, fill: egui::Color32, margin: Margin) -> Frame {
    Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(p.card_radius))
        .stroke(Stroke::new(1.0, p.line))
        .shadow(p.shadow)
        .inner_margin(margin)
}

/// Narrowest a left/right slot gets, and the width the centre keeps.
const SIDE_MIN: f32 = 120.0;
const SIDE_MAX: f32 = 640.0;
const CENTER_MIN: f32 = 320.0;

/// How wide side slots may be in a window `width` points wide, and whether
/// the right one must step aside. Display scaling makes windows narrow in
/// points (a half-screen window at 150-200% is ~500-650), and fixed sides
/// would squeeze the chat to nothing.
fn side_widths(width: f32, left: bool, right: bool) -> (f32, bool) {
    let room = (width - CENTER_MIN).max(0.0);
    let drop_right = left && right && room < 2.0 * SIDE_MIN;
    let sides = (left as u8 + (right && !drop_right) as u8).max(1);
    ((room / f32::from(sides)).clamp(SIDE_MIN, SIDE_MAX), drop_right)
}

fn mtime(p: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

fn builtin_theme_entry() -> Entry {
    Entry { name: DEFAULT_THEME.into(), source: crate::catalog::Source::Builtin(crate::assets::THEMES[0].1), origin: "built-in".into() }
}

fn builtin_layout_entry() -> Entry {
    Entry { name: "default".into(), source: crate::catalog::Source::Builtin(crate::assets::LAYOUTS[0].1), origin: "built-in".into() }
}

fn open_path(p: &std::path::Path) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(p);
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(p).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::side_widths;

    #[test]
    fn sides_give_way_to_the_centre() {
        // Roomy: sides keep their layout sizes (up to the cap).
        assert_eq!(side_widths(1100.0, true, true), (390.0, false));
        assert_eq!(side_widths(3000.0, true, true), (640.0, false));
        // 1100 px at 200%: both sides shrink...
        assert_eq!(side_widths(800.0, true, true), (240.0, false));
        // ...and when both can't fit beside the centre, the right one goes.
        assert_eq!(side_widths(550.0, true, true), (230.0, true));
        assert_eq!(side_widths(480.0, true, false), (160.0, false));
        assert_eq!(side_widths(300.0, true, true), (120.0, true));
    }
}
