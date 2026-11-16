//! Agent control: the `ui.*` control-API methods that read and drive the
//! window — widget tree, clicks and typing, the window itself, screenshots,
//! clipboard — whether the window is in front, behind others, minimised, or
//! never shown at all (`DEN_WINDOW=headless`).
//!
//! It is an [`egui::Plugin`]. At the start of each frame it picks up calls the
//! API queued ([`UiStore::call_frontend`]) and injects their input, the same
//! events a real mouse and keyboard produce, so nothing moves the user's
//! cursor or steals focus. At the end of the frame it answers from what the
//! frame produced: the accessibility tree (every widget's role, text and
//! rectangle), the shapes (for CPU screenshots) and clipboard copies.
//!
//! Widgets are named by **selectors**: `@12` (a ref from `ui.tree`, stable
//! while the widget exists), `Roll` (text: exact, then case-insensitive,
//! then substring), or `button:Roll` (a role, then text).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use denchat_core::ui::{FrontendCall, UiStore};
use egui::accesskit::{self, Action, ActionRequest, NodeId, Role, Toggled, TreeId, TreeUpdate};
use egui::{
    Color32, ColorImage, Context, Event, FullOutput, Key, Modifiers, OutputCommand, PointerButton, Pos2, RawInput, Rect, ViewportCommand,
    ViewportId, pos2, vec2,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::soft;

/// Accesskit `author_id` prefix marking the node that wraps a layout panel.
pub const PANEL_TAG: &str = "panel:";

/// How the app is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A normal OS window.
    Window,
    /// No window: frames run off-screen and screenshots are drawn on the CPU.
    Headless,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Window => "window",
            Kind::Headless => "headless",
        }
    }
}

/// One widget in a [`Tree`].
#[derive(Clone, Debug)]
pub struct Node {
    pub r: u32,
    pub id: NodeId,
    pub role: String,
    pub text: String,
    /// A text box's contents.
    pub value: Option<String>,
    /// In points.
    pub rect: Rect,
    pub panel: Option<String>,
    pub focused: bool,
    pub disabled: bool,
    pub toggled: Option<bool>,
    pub selected: bool,
    pub clickable: bool,
    pub focusable: bool,
    /// Scrolled out of view, clipped away, or outside the window.
    pub hidden: bool,
}

impl Node {
    fn short(&self) -> String {
        format!("@{} {} {}", self.r, self.role, quote(&self.text, 60))
    }

    fn center(&self) -> Pos2 {
        self.rect.center()
    }
}

/// The widgets of one frame, in drawing order.
#[derive(Clone, Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    /// Rectangles of the layout panels, by id.
    pub panels: Vec<(String, Rect)>,
    pub window: Rect,
}

/// Turns accesskit ids into short refs that stay put while a widget exists.
#[derive(Default)]
/// `@ref` numbers. A ref names one widget *with one label*: egui ids are
/// often positional, so when a list shifts the same id can end up on another
/// row; keying on the text too means an old ref then stops matching instead
/// of silently pointing at something else.
struct Refs {
    map: HashMap<(NodeId, String, String), u32>,
    next: u32,
}

impl Refs {
    fn get(&mut self, id: NodeId, role: &str, text: &str) -> u32 {
        if self.map.len() > 20_000 {
            self.map.clear();
        }
        *self.map.entry((id, role.to_owned(), text.to_owned())).or_insert_with(|| {
            self.next += 1;
            self.next
        })
    }
}

fn role_name(role: Role) -> String {
    match role {
        Role::Button | Role::DefaultButton => "button".into(),
        Role::CheckBox => "checkbox".into(),
        Role::RadioButton => "radio".into(),
        Role::Switch => "switch".into(),
        Role::TextInput | Role::MultilineTextInput | Role::SearchInput | Role::PasswordInput => "textbox".into(),
        Role::Label | Role::Paragraph => "text".into(),
        Role::Heading => "heading".into(),
        Role::Link => "link".into(),
        Role::Slider => "slider".into(),
        Role::SpinButton => "number".into(),
        Role::ComboBox => "combobox".into(),
        Role::MenuItem | Role::MenuItemCheckBox | Role::MenuItemRadio => "menuitem".into(),
        Role::Tab => "tab".into(),
        Role::ProgressIndicator => "progress".into(),
        Role::Image => "image".into(),
        Role::Window => "window".into(),
        Role::ListItem => "item".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Roles that only group other nodes; shown only if they carry text.
fn structural(role: Role) -> bool {
    matches!(role, Role::GenericContainer | Role::Unknown | Role::Group | Role::ScrollView | Role::Pane | Role::TextRun | Role::Window)
}

impl Default for Tree {
    fn default() -> Tree {
        Tree { nodes: Vec::new(), panels: Vec::new(), window: Rect::NOTHING }
    }
}

impl Tree {
    fn build(ctx: &Context, update: &TreeUpdate, refs: &mut Refs) -> Tree {
        let window = ctx.viewport_rect();
        let index: HashMap<NodeId, &accesskit::Node> = update.nodes.iter().map(|(id, n)| (*id, n)).collect();
        let mut tree = Tree { window, ..Tree::default() };
        let Some(root) = update.tree.as_ref().map(|t| t.root) else { return tree };
        // Depth-first, children in order, carrying the enclosing panel id.
        let mut stack: Vec<(NodeId, Option<String>, bool)> = vec![(root, None, true)];
        while let Some((id, panel, is_root)) = stack.pop() {
            let Some(n) = index.get(&id) else { continue };
            let mut panel = panel;
            if let Some(tag) = n.author_id().and_then(|a| a.strip_prefix(PANEL_TAG)) {
                panel = Some(tag.to_owned());
                if let Some(b) = n.bounds() {
                    tree.panels.push((tag.to_owned(), rect(b)));
                }
            }
            let role = n.role();
            // A visible label next to the widget names it (`labelled_by`).
            // (A label node keeps its text in `value`.)
            let by = || {
                n.labelled_by()
                    .iter()
                    .filter_map(|l| index.get(l).and_then(|l| l.label().or_else(|| l.value())))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let label = n.label().filter(|l| !l.is_empty()).map(str::to_owned).or_else(|| Some(by()).filter(|l| !l.is_empty()));
            let text = readable(label.as_deref().or_else(|| n.value()).unwrap_or_default());
            let is_panel = n.author_id().is_some_and(|a| a.starts_with(PANEL_TAG));
            let window_title = role == Role::Window && !is_root && !text.is_empty();
            if !is_root && !is_panel && (!structural(role) || window_title) && (!text.is_empty() || !structural(role)) {
                let r = n.bounds().map(rect).unwrap_or(Rect::NOTHING);
                let textbox = matches!(role, Role::TextInput | Role::MultilineTextInput | Role::SearchInput | Role::PasswordInput);
                let (text, value) = if textbox {
                    let hint = readable(label.as_deref().or_else(|| n.placeholder()).unwrap_or_default());
                    (hint, Some(n.value().unwrap_or_default().to_owned()))
                } else {
                    (text, None)
                };
                let clipped = egui_id(id).and_then(|i| ctx.read_response(i)).map(|resp| resp.interact_rect);
                let hidden =
                    !r.is_positive() || !r.intersects(window) || clipped.is_some_and(|c| !c.is_positive() || !c.intersects(window));
                // Skip nodes that carry nothing an agent could read or press.
                if !(text.is_empty() && value.is_none() && !n.supports_action(Action::Click)) {
                    let role = role_name(role);
                    tree.nodes.push(Node {
                        r: refs.get(id, &role, &text),
                        id,
                        role,
                        text,
                        value,
                        rect: r,
                        panel: panel.clone(),
                        focused: update.focus == id,
                        disabled: n.is_disabled(),
                        toggled: n.toggled().map(|t| t == Toggled::True),
                        selected: n.is_selected().unwrap_or(false),
                        clickable: n.supports_action(Action::Click),
                        focusable: n.supports_action(Action::Focus),
                        hidden,
                    });
                }
            }
            for child in n.children().iter().rev() {
                stack.push((*child, panel.clone(), false));
            }
        }
        tree
    }

    fn focused(&self) -> Option<&Node> {
        self.nodes.iter().find(|n| n.focused)
    }

    fn by_ref(&self, r: u32) -> Option<&Node> {
        self.nodes.iter().find(|n| n.r == r)
    }

    /// Resolve a selector. Visible widgets win over hidden ones; among equals,
    /// `want_click` prefers ones that can be clicked.
    fn find(&self, sel: &str, panel: Option<&str>, index: Option<usize>, want_click: bool) -> Result<&Node, String> {
        let matches = self.matches(sel, panel);
        if let Some(i) = index {
            return matches.get(i).copied().ok_or_else(|| format!("{sel:?}: only {} match(es), no index {i}", matches.len()));
        }
        match matches.as_slice() {
            [] if sel.starts_with('@') => Err(format!("{sel} is gone (that widget changed or closed); run `den ui tree` again")),
            [] => Err(format!("nothing on screen matches {sel:?}{}", panel.map(|p| format!(" in panel {p}")).unwrap_or_default())),
            [one] => Ok(one),
            many => {
                let clickable: Vec<&&Node> = many.iter().filter(|n| n.clickable).collect();
                if want_click && clickable.len() == 1 {
                    return Ok(clickable[0]);
                }
                let list: Vec<String> = many.iter().take(6).map(|n| n.short()).collect();
                Err(format!("{sel:?} matches {}: {} — use a @ref, role:text, or index", many.len(), list.join(", ")))
            }
        }
    }

    fn matches(&self, sel: &str, panel: Option<&str>) -> Vec<&Node> {
        let in_panel = |n: &&Node| panel.is_none_or(|p| n.panel.as_deref().is_some_and(|q| q == p || q.starts_with(&format!("{p}/"))));
        if let Some(r) = sel.strip_prefix('@').and_then(|r| r.parse::<u32>().ok()) {
            return self.by_ref(r).into_iter().collect();
        }
        let (role, text) = match sel.split_once(':') {
            Some((r, t)) if KNOWN_ROLES.contains(&r) => (Some(r), t),
            _ => (None, sel),
        };
        let text = text.trim();
        let pool: Vec<&Node> = self.nodes.iter().filter(in_panel).filter(|n| role.is_none_or(|r| n.role == r)).collect();
        let lower = text.to_lowercase();
        let hay = |n: &Node| [Some(n.text.as_str()), n.value.as_deref()].into_iter().flatten().map(str::to_owned).collect::<Vec<_>>();
        let tiers: [&dyn Fn(&str) -> bool; 3] =
            [&|s: &str| s == text, &|s: &str| s.to_lowercase() == lower, &|s: &str| !lower.is_empty() && s.to_lowercase().contains(&lower)];
        for tier in tiers {
            let found: Vec<&Node> = pool.iter().copied().filter(|n| hay(n).iter().any(|s| tier(s))).collect();
            if !found.is_empty() {
                let visible: Vec<&Node> = found.iter().copied().filter(|n| !n.hidden).collect();
                return if visible.is_empty() { found } else { visible };
            }
        }
        if role.is_some() && text.is_empty() {
            return pool;
        }
        Vec::new()
    }

    /// One line per widget, grouped by panel: what `den ui tree` prints.
    pub fn text(&self, header: &str, panel: Option<&str>, all: bool, rects: bool) -> String {
        let mut out = String::from(header);
        let mut current: Option<Option<&str>> = None;
        for n in &self.nodes {
            if (n.hidden && !all) || panel.is_some_and(|p| n.panel.as_deref().is_none_or(|q| q != p && !q.starts_with(&format!("{p}/")))) {
                continue;
            }
            let group = n.panel.as_deref();
            if current != Some(group) {
                out.push('\n');
                out.push_str(group.unwrap_or("(no panel)"));
                current = Some(group);
            }
            out.push_str(&format!("\n  @{} {}", n.r, n.role));
            if !n.text.is_empty() {
                out.push(' ');
                out.push_str(&quote(&n.text, if all { 400 } else { 80 }));
            }
            if let Some(v) = &n.value {
                out.push_str(&format!(" value={}", quote(v, if all { 400 } else { 80 })));
            }
            for (on, word) in [(n.focused, "focused"), (n.disabled, "disabled"), (n.selected, "selected"), (n.hidden, "hidden")] {
                if on {
                    out.push(' ');
                    out.push_str(word);
                }
            }
            match n.toggled {
                Some(true) => out.push_str(" on"),
                Some(false) => out.push_str(" off"),
                None => {}
            }
            if rects {
                let r = n.rect;
                out.push_str(&format!(" [{:.0},{:.0} {:.0}x{:.0}]", r.min.x, r.min.y, r.width(), r.height()));
            }
        }
        out
    }

    fn json(&self, panel: Option<&str>, all: bool) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .filter(|n| all || !n.hidden)
            .filter(|n| panel.is_none_or(|p| n.panel.as_deref().is_some_and(|q| q == p || q.starts_with(&format!("{p}/")))))
            .map(|n| {
                let mut flags = Vec::new();
                for (on, word) in [
                    (n.focused, "focused"),
                    (n.disabled, "disabled"),
                    (n.selected, "selected"),
                    (n.hidden, "hidden"),
                    (n.clickable, "clickable"),
                ] {
                    if on {
                        flags.push(word);
                    }
                }
                let r = n.rect;
                json!({
                    "ref": n.r, "role": n.role, "text": n.text, "value": n.value, "panel": n.panel,
                    "toggled": n.toggled, "flags": flags,
                    "rect": [r.min.x.round(), r.min.y.round(), r.width().round(), r.height().round()],
                })
            })
            .collect();
        json!({"nodes": nodes, "panels": self.panels.iter().map(|(p, _)| p).collect::<Vec<_>>()})
    }
}

const KNOWN_ROLES: &[&str] = &[
    "button", "checkbox", "radio", "switch", "textbox", "text", "heading", "link", "slider", "number", "combobox", "menuitem", "tab",
    "progress", "image", "window", "item",
];

fn rect(b: accesskit::Rect) -> Rect {
    Rect::from_min_max(pos2(b.x0 as f32, b.y0 as f32), pos2(b.x1 as f32, b.y1 as f32))
}

fn egui_id(id: NodeId) -> Option<egui::Id> {
    // SAFETY: accesskit ids in egui's tree are `Id::value()`s, which are never zero.
    (id.0 != 0).then(|| unsafe { egui::Id::from_high_entropy_bits(id.0) })
}

fn quote(s: &str, max: usize) -> String {
    let s: String = if s.chars().count() > max { s.chars().take(max).chain(std::iter::once('…')).collect() } else { s.to_owned() };
    serde_json::to_string(&s).unwrap_or_default()
}

// ----- jobs --------------------------------------------------------------

enum State {
    New,
    /// Input to inject, one batch per frame.
    Steps(VecDeque<Vec<Event>>),
    /// Answer at the end of this frame.
    Answer(Value),
    /// Answer with the tree at the end of this frame.
    Tree,
    /// Answer after `frames` more frames.
    Settle(u32, Value),
    /// Answer once this time has passed (tooltips appear after a delay).
    Until(Instant, Value),
    /// Start over after `frames` more frames (the target was scrolled into view).
    Retry(u32),
    Wait {
        deadline: Instant,
        since: Instant,
    },
    Shot(Shot),
}

struct Shot {
    path: PathBuf,
    crop: Option<Rect>,
    scale: f32,
    gpu: Option<(u64, Instant)>,
}

struct Job {
    call: Option<FrontendCall>,
    state: State,
    /// Answer for input jobs once their steps ran.
    done: Value,
    /// …and how long to keep going after that before answering.
    hold: Duration,
    /// Its target was out of view and has been scrolled to; when the steps
    /// are done, start the job again so it aims at where the target is now.
    retry: bool,
    scrolled: bool,
    started: Instant,
}

impl Job {
    fn answer(&mut self, result: Result<Value, String>) {
        if let Some(c) = self.call.take() {
            c.answer(result);
        }
    }

    fn param(&self, key: &str) -> &Value {
        self.call.as_ref().map(|c| &c.params[key]).unwrap_or(&Value::Null)
    }

    fn str(&self, key: &str) -> Option<String> {
        self.param(key).as_str().map(str::to_owned).filter(|s| !s.is_empty())
    }

    /// Whoever asked stopped waiting (the API call timed out, the CLI was
    /// interrupted): don't act on the app for them any more.
    fn abandoned(&self) -> bool {
        self.call.as_ref().is_some_and(FrontendCall::abandoned)
    }

    /// The pointer can't aim at a widget that's scrolled out of view (it would
    /// land on whatever is drawn there): scroll it into view, then start again.
    fn bring_into_view(&mut self, n: &Node) -> Result<VecDeque<Vec<Event>>, String> {
        if self.scrolled {
            return Err(format!("{} is out of view, and scrolling to it didn't bring it into view", n.short()));
        }
        self.scrolled = true;
        self.retry = true;
        Ok(VecDeque::from([vec![action(n.id, Action::ScrollIntoView)]]))
    }
}

/// GPU screenshots the window delivered, by request number.
type GpuShots = Arc<Mutex<Vec<(u64, Arc<ColorImage>)>>>;

pub struct AgentPlugin {
    ui: Arc<UiStore>,
    kind: Kind,
    textures: soft::Textures,
    tree: Option<Tree>,
    refs: Refs,
    jobs: VecDeque<Job>,
    gpu_shots: GpuShots,
    next_shot: u64,
    clipboard_file: Option<PathBuf>,
    last_call: Option<Instant>,
    pointer: Option<Pos2>,
    /// Accessibility was on before any agent call (a screen reader).
    screen_reader: bool,
    title: String,
}

/// Window mode keeps the accessibility tree switched on this long after the last call.
const TREE_IDLE: Duration = Duration::from_secs(120);
/// The longest `ui.wait` (an hour).
const MAX_WAIT_MS: u64 = 3_600_000;

/// The call gives an explicit point (`x`, `y`) to aim at.
fn has_point(job: &Job) -> bool {
    job.param("x").is_number() && job.param("y").is_number()
}

impl AgentPlugin {
    pub fn new(ui: Arc<UiStore>, kind: Kind) -> AgentPlugin {
        AgentPlugin {
            ui,
            kind,
            textures: Default::default(),
            tree: None,
            refs: Default::default(),
            jobs: VecDeque::new(),
            gpu_shots: Default::default(),
            next_shot: 0,
            clipboard_file: std::env::var_os("DEN_CLIPBOARD").map(PathBuf::from),
            last_call: None,
            pointer: None,
            screen_reader: false,
            title: "Den".into(),
        }
    }

    /// Queue a call from inside the app (e.g. `DEN_SCREENSHOT`).
    pub fn submit(&mut self, call: FrontendCall) {
        self.jobs.push_back(Job {
            call: Some(call),
            state: State::New,
            done: Value::Null,
            hold: Duration::ZERO,
            retry: false,
            scrolled: false,
            started: Instant::now(),
        });
    }

    pub fn set_title(&mut self, title: &str) {
        title.clone_into(&mut self.title);
    }

    fn window_info(&self, ctx: &Context) -> Value {
        let vp = ctx.input(|i| i.viewport().clone());
        let size = ctx.viewport_rect().size();
        let r = |v: Option<Rect>| v.map(|r| json!([r.min.x.round(), r.min.y.round()]));
        json!({
            "renderer": self.kind.name(),
            "title": vp.title.clone().unwrap_or_else(|| self.title.clone()),
            "size": [size.x.round(), size.y.round()],
            "pos": r(vp.outer_rect),
            "pixels_per_point": ctx.pixels_per_point(),
            "zoom": ctx.zoom_factor(),
            "focused": vp.focused,
            "minimized": vp.minimized,
            "maximized": vp.maximized,
            "fullscreen": vp.fullscreen,
            "occluded": vp.occluded,
            "visible": vp.visible(),
            "monitor": vp.monitor_size.map(|m| [m.x.round(), m.y.round()]),
        })
    }

    fn header(&self, ctx: &Context) -> String {
        let size = ctx.viewport_rect().size();
        let vp = ctx.input(|i| i.viewport().clone());
        let mut flags = vec![self.kind.name().to_owned()];
        if vp.minimized == Some(true) {
            flags.push("minimized".into());
        }
        if vp.occluded == Some(true) {
            flags.push("covered".into());
        }
        if vp.focused == Some(true) && self.kind == Kind::Window {
            flags.push("focused".into());
        }
        let zoom = ctx.zoom_factor();
        if (zoom - 1.0).abs() > 0.01 {
            flags.push(format!("zoom {zoom:.2}"));
        }
        format!("window {:.0}x{:.0} {} ({})", size.x, size.y, quote(vp.title.as_deref().unwrap_or(&self.title), 80), flags.join(", "))
    }

    /// Start a new job: validate it and turn it into a state.
    fn start(&mut self, ctx: &Context, job: &mut Job) {
        let method = job.call.as_ref().map(|c| c.method.clone()).unwrap_or_default();
        let needs_tree = matches!(method.as_str(), "ui.click" | "ui.hover" | "ui.scroll" | "ui.type" | "ui.wait")
            || (method == "ui.screenshot" && (job.str("panel").is_some() || job.str("target").is_some()));
        if needs_tree && self.tree.is_none() {
            // The first frame with the tree switched on hasn't ended yet.
            return;
        }
        let result: Result<State, String> = match method.as_str() {
            "ui.tree" => Ok(State::Tree),
            "ui.click" | "ui.hover" | "ui.scroll" | "ui.type" | "ui.key" => self.input_job(ctx, &method, job).map(State::Steps),
            "ui.wait" => {
                let ms = job.param("timeout_ms").as_u64().unwrap_or(10_000).min(MAX_WAIT_MS);
                if job.str("target").is_none() {
                    Err("ui.wait needs a target".into())
                } else {
                    Ok(State::Wait { deadline: Instant::now() + Duration::from_millis(ms), since: Instant::now() })
                }
            }
            "ui.window" => {
                self.window_job(ctx, job).map(|changed| if changed { State::Settle(3, Value::Null) } else { State::Answer(Value::Null) })
            }
            "ui.scale" => match job.param("scale").as_f64() {
                Some(s) if (0.25..=5.0).contains(&s) => {
                    ctx.set_zoom_factor(s as f32);
                    Ok(State::Settle(2, json!({"scale": s})))
                }
                _ => Err("scale must be a number between 0.25 and 5".into()),
            },
            "ui.screenshot" => self.shot_job(ctx, job).map(State::Shot),
            "app.quit" => {
                ctx.send_viewport_cmd(ViewportCommand::Close);
                Ok(State::Answer(json!({"quitting": true})))
            }
            other => Err(format!("{other} isn't supported by this frontend")),
        };
        match result {
            Ok(s) => job.state = s,
            Err(e) => job.answer(Err(e)),
        }
    }

    fn target(&self, job: &Job, want_click: bool) -> Result<Option<Node>, String> {
        let Some(sel) = job.str("target") else { return Ok(None) };
        let tree = self.tree.as_ref().ok_or("no widget tree yet")?;
        let index = job.param("index").as_u64().map(|i| i as usize);
        tree.find(&sel, job.str("panel").as_deref(), index, want_click).cloned().map(Some)
    }

    fn point(&self, ctx: &Context, job: &Job, node: Option<&Node>) -> Pos2 {
        match (job.param("x").as_f64(), job.param("y").as_f64(), node) {
            (Some(x), Some(y), _) => pos2(x as f32, y as f32),
            (_, _, Some(n)) => n.center(),
            _ => self.pointer.unwrap_or_else(|| ctx.viewport_rect().center()),
        }
    }

    fn input_job(&mut self, ctx: &Context, method: &str, job: &mut Job) -> Result<VecDeque<Vec<Event>>, String> {
        let mut steps: VecDeque<Vec<Event>> = VecDeque::new();
        let modifiers = Modifiers::NONE;
        match method {
            "ui.click" => {
                let node = self.target(job, true)?;
                if node.is_none() && (job.param("x").is_null() || job.param("y").is_null()) {
                    return Err("ui.click needs a target or x and y".into());
                }
                let button = match job.param("button").as_str() {
                    Some("right" | "secondary") => PointerButton::Secondary,
                    Some("middle") => PointerButton::Middle,
                    _ => PointerButton::Primary,
                };
                let double = job.param("double").as_bool().unwrap_or(false);
                let pointer = job.param("pointer").as_bool().unwrap_or(false);
                match &node {
                    Some(n) if n.disabled => return Err(format!("{} is disabled", n.short())),
                    // Clicking a text box puts the cursor in it.
                    Some(n) if n.role == "textbox" && n.focusable && button == PointerButton::Primary && !double && !pointer => {
                        steps.push_back(vec![action(n.id, Action::Focus)]);
                        job.done = json!({"clicked": n.short(), "via": "accesskit"});
                    }
                    Some(n) if n.clickable && button == PointerButton::Primary && !double && !pointer => {
                        steps.push_back(vec![action(n.id, Action::Click)]);
                        job.done = json!({"clicked": n.short(), "via": "accesskit"});
                    }
                    Some(n) if n.hidden && !has_point(job) => return job.bring_into_view(n),
                    _ => {
                        let pos = self.point(ctx, job, node.as_ref());
                        self.pointer = Some(pos);
                        steps.push_back(vec![Event::PointerMoved(pos)]);
                        for _ in 0..if double { 2 } else { 1 } {
                            steps.push_back(vec![Event::PointerButton { pos, button, pressed: true, modifiers }]);
                            steps.push_back(vec![Event::PointerButton { pos, button, pressed: false, modifiers }]);
                        }
                        let what = node.as_ref().map(Node::short).unwrap_or_else(|| format!("{:.0},{:.0}", pos.x, pos.y));
                        job.done = json!({"clicked": what, "via": "pointer"});
                    }
                }
            }
            "ui.hover" => {
                let node = self.target(job, false)?;
                if node.is_none() && (job.param("x").is_null() || job.param("y").is_null()) {
                    return Err("ui.hover needs a target or x and y".into());
                }
                if let Some(n) = node.as_ref().filter(|n| n.hidden && !has_point(job)) {
                    return job.bring_into_view(n);
                }
                let pos = self.point(ctx, job, node.as_ref());
                self.pointer = Some(pos);
                steps.push_back(vec![Event::PointerMoved(pos)]);
                // Long enough for its tooltip to show.
                job.hold = Duration::from_secs_f32(ctx.global_style().interaction.tooltip_delay + 0.15);
                job.done = json!({"hovering": node.as_ref().map(Node::short).unwrap_or_else(|| format!("{:.0},{:.0}", pos.x, pos.y))});
            }
            "ui.scroll" => {
                let node = self.target(job, false)?;
                if let Some(n) = node.as_ref().filter(|n| n.hidden && !has_point(job)) {
                    return job.bring_into_view(n);
                }
                let pos = self.point(ctx, job, node.as_ref());
                self.pointer = Some(pos);
                let dy = job.param("dy").as_f64().unwrap_or(0.0) as f32;
                let dx = job.param("dx").as_f64().unwrap_or(0.0) as f32;
                if dx == 0.0 && dy == 0.0 {
                    return Err("ui.scroll needs dy and/or dx".into());
                }
                steps.push_back(vec![Event::PointerMoved(pos)]);
                // egui's delta moves the content: scrolling down moves it up.
                steps.push_back(vec![Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: vec2(-dx, -dy),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Wheel,
                    modifiers,
                }]);
                // Let smooth scrolling finish.
                for _ in 0..6 {
                    steps.push_back(Vec::new());
                }
                job.done = json!({"scrolled": [dx, dy]});
            }
            "ui.type" => {
                let text = job.str("text").ok_or("ui.type needs text")?;
                let into = match self.target(job, false)? {
                    Some(n) => {
                        if n.focusable {
                            steps.push_back(vec![action(n.id, Action::Focus)]);
                        } else if n.hidden {
                            return job.bring_into_view(&n);
                        } else {
                            let pos = n.center();
                            steps.push_back(vec![Event::PointerMoved(pos)]);
                            steps.push_back(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers }]);
                            steps.push_back(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers }]);
                        }
                        n.short()
                    }
                    None => match self.tree.as_ref().and_then(Tree::focused) {
                        Some(n) => n.short(),
                        None => return Err("nothing has keyboard focus; give a target (e.g. textbox:Message)".into()),
                    },
                };
                steps.push_back(vec![Event::Text(text.clone())]);
                if job.param("enter").as_bool().unwrap_or(false) {
                    steps.push_back(vec![key_event(Key::Enter, Modifiers::NONE, true)]);
                    steps.push_back(vec![key_event(Key::Enter, Modifiers::NONE, false)]);
                }
                job.done = json!({"typed": text.chars().count(), "into": into});
            }
            "ui.key" => {
                let chord = job.str("key").ok_or("ui.key needs a key, e.g. Enter or ctrl+a")?;
                let (m, key) = parse_chord(&chord)?;
                let mut down = vec![key_event(key, m, true)];
                if m.command {
                    match key {
                        Key::C => down.push(Event::Copy),
                        Key::X => down.push(Event::Cut),
                        Key::V => {
                            let text = job.str("text").or_else(|| self.ui.copied().map(|c| c.text)).unwrap_or_default();
                            down.push(Event::Paste(text));
                        }
                        _ => {}
                    }
                }
                steps.push_back(down);
                steps.push_back(vec![key_event(key, m, false)]);
                job.done = json!({"pressed": chord});
            }
            _ => unreachable!(),
        }
        Ok(steps)
    }

    /// Apply `ui.window` changes; true if anything was asked for.
    fn window_job(&mut self, ctx: &Context, job: &Job) -> Result<bool, String> {
        let mut cmds = Vec::new();
        let pair = |v: &Value| -> Option<(f32, f32)> {
            match v {
                Value::Array(a) if a.len() == 2 => Some((a[0].as_f64()? as f32, a[1].as_f64()? as f32)),
                Value::String(s) => {
                    let (a, b) = s.split_once(['x', ','])?;
                    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
                }
                _ => None,
            }
        };
        if !job.param("size").is_null() {
            let (w, h) = pair(job.param("size")).ok_or("size is [w, h] or \"WxH\"")?;
            if !(w >= 200.0 && h >= 150.0) {
                return Err("size must be at least 200x150".into());
            }
            cmds.push(ViewportCommand::InnerSize(vec2(w, h)));
        }
        if !job.param("pos").is_null() {
            let (x, y) = pair(job.param("pos")).ok_or("pos is [x, y] or \"X,Y\"")?;
            cmds.push(ViewportCommand::OuterPosition(pos2(x, y)));
        }
        for (key, make) in [
            ("minimized", ViewportCommand::Minimized as fn(bool) -> ViewportCommand),
            ("maximized", ViewportCommand::Maximized),
            ("fullscreen", ViewportCommand::Fullscreen),
            ("visible", ViewportCommand::Visible),
        ] {
            if let Some(b) = job.param(key).as_bool() {
                cmds.push(make(b));
            }
        }
        if job.param("focus").as_bool() == Some(true) {
            cmds.push(ViewportCommand::Minimized(false));
            cmds.push(ViewportCommand::Focus);
        }
        let changed = !cmds.is_empty();
        for c in cmds {
            ctx.send_viewport_cmd(c);
        }
        Ok(changed)
    }

    fn shot_job(&mut self, ctx: &Context, job: &Job) -> Result<Shot, String> {
        let path = job.str("path").map(PathBuf::from).ok_or("ui.screenshot needs a path")?;
        let crop = if let Some(panel) = job.str("panel") {
            let tree = self.tree.as_ref().ok_or("no widget tree yet")?;
            let found = tree
                .panels
                .iter()
                .find(|(p, _)| *p == panel)
                .or_else(|| tree.panels.iter().find(|(p, _)| p.starts_with(&format!("{panel}/"))));
            match found {
                Some((_, r)) => Some(*r),
                None => {
                    let have: Vec<&str> = tree.panels.iter().map(|(p, _)| p.as_str()).collect();
                    return Err(format!("panel {panel} isn't on screen (showing: {})", have.join(", ")));
                }
            }
        } else {
            match self.target(job, false)? {
                Some(n) if n.hidden => {
                    return Err(format!("{} is out of view; scroll to it first (`den ui scroll … --over …`)", n.short()));
                }
                n => n.map(|n| n.rect.expand(4.0)),
            }
        };
        let scale = job.param("scale").as_f64().map(|s| s as f32).unwrap_or(1.0).clamp(0.05, 1.0);
        let renderer = job.str("renderer").unwrap_or_else(|| "auto".into());
        let visible = ctx.input(|i| i.viewport().visible()).unwrap_or(true);
        let gpu = match (self.kind, renderer.as_str()) {
            (Kind::Headless, "gpu") => return Err("a headless app has no GPU renderer; use renderer=cpu (the default)".into()),
            (Kind::Headless, _) | (_, "cpu") => false,
            (Kind::Window, "gpu") => true,
            (Kind::Window, _) => visible,
        };
        let gpu = gpu.then(|| {
            let id = self.next_shot;
            self.next_shot += 1;
            let sink = self.gpu_shots.clone();
            let repaint = ctx.clone();
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                sink.lock().push((id, img));
                repaint.request_repaint();
            })));
            (id, Instant::now() + Duration::from_secs(3))
        });
        Ok(Shot { path, crop, scale, gpu })
    }

    /// Crop, scale and save on another thread, then answer.
    fn finish_shot(&self, mut call: Option<FrontendCall>, shot: &Shot, img: Arc<ColorImage>, ppp: f32, renderer: &'static str) {
        let (path, crop, scale) = (shot.path.clone(), shot.crop, shot.scale);
        let Some(call) = call.take() else { return };
        std::thread::spawn(move || {
            let mut img = match crop {
                Some(r) => soft::crop(&img, Rect::from_min_max((r.min.to_vec2() * ppp).to_pos2(), (r.max.to_vec2() * ppp).to_pos2())),
                None => (*img).clone(),
            };
            if scale < 1.0 {
                img = soft::downscale(&img, scale);
            }
            let res = soft::save_png(&path, &img)
                .map(|()| json!({"path": path, "width": img.size[0], "height": img.size[1], "renderer": renderer}))
                .map_err(|e| format!("saving {}: {e:#}", path.display()));
            call.answer(res);
        });
    }

    fn cpu_frame(&self, ctx: &Context, output: &FullOutput) -> Arc<ColorImage> {
        let ppp = output.pixels_per_point;
        let size = ctx.viewport_rect().size() * ppp;
        let prims = ctx.tessellate(output.shapes.clone(), ppp);
        let clear = ctx.global_style().visuals.panel_fill;
        let clear = Color32::from_rgb(clear.r(), clear.g(), clear.b());
        Arc::new(soft::render(&prims, &self.textures, [size.x.round() as usize, size.y.round() as usize], ppp, clear))
    }

    fn keep_running(&self, ctx: &Context) {
        if self.jobs.is_empty() {
            return;
        }
        // Ask for the frame even if the window is hidden: a minimised or covered
        // window runs no ui otherwise, and agents usually drive it from behind.
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::RequestPaintWhileHidden);
        let waiting = self.jobs.iter().all(|j| matches!(j.state, State::Wait { .. }));
        if waiting {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint();
        }
    }
}

fn action(target: NodeId, action: Action) -> Event {
    Event::AccessKitActionRequest(ActionRequest { action, target_tree: TreeId::ROOT, target_node: target, data: None })
}

fn key_event(key: Key, modifiers: Modifiers, pressed: bool) -> Event {
    Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers }
}

/// `Enter`, `esc`, `ctrl+a`, `cmd+shift+k`, `F2`. `cmd`/`mod` mean Ctrl off macOS.
pub fn parse_chord(chord: &str) -> Result<(Modifiers, Key), String> {
    let parts: Vec<&str> = chord.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let Some((last, mods)) = parts.split_last() else { return Err("empty key".into()) };
    let mac = cfg!(target_os = "macos");
    let mut m = Modifiers::NONE;
    for p in mods {
        match p.to_lowercase().as_str() {
            "ctrl" | "control" => {
                m.ctrl = true;
                m.command |= !mac;
            }
            "shift" => m.shift = true,
            "alt" | "option" | "opt" => m.alt = true,
            "cmd" | "command" | "super" | "meta" | "win" | "mod" | "cmdorctrl" => {
                m.command = true;
                m.mac_cmd = mac;
                m.ctrl |= !mac;
            }
            other => return Err(format!("unknown modifier {other:?} (ctrl, shift, alt, cmd)")),
        }
    }
    let alias = match last.to_lowercase().as_str() {
        "esc" => Some(Key::Escape),
        "return" | "enter" => Some(Key::Enter),
        "del" => Some(Key::Delete),
        "space" | " " => Some(Key::Space),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "pgup" => Some(Key::PageUp),
        "pgdn" | "pgdown" => Some(Key::PageDown),
        _ => None,
    };
    let key = alias
        .or_else(|| Key::from_name(last))
        .or_else(|| Key::ALL.iter().copied().find(|k| k.name().eq_ignore_ascii_case(last)))
        .ok_or_else(|| format!("unknown key {last:?}"))?;
    Ok((m, key))
}

impl egui::Plugin for AgentPlugin {
    fn debug_name(&self) -> &'static str {
        "den_agent"
    }

    fn setup(&mut self, ctx: &Context) {
        if self.kind == Kind::Headless {
            ctx.enable_accesskit();
        }
    }

    fn input_hook(&mut self, ctx: &Context, input: &mut RawInput) {
        for call in self.ui.take_frontend_calls() {
            self.submit(call);
        }
        // Calls nobody waits for any more never touch the app, except input
        // already half sent (a button pressed but not yet released).
        self.jobs.retain(|j| !j.abandoned() || matches!(j.state, State::Steps(_)));
        if self.jobs.is_empty() {
            if self.kind == Kind::Window && self.last_call.is_some_and(|t| t.elapsed() > TREE_IDLE) {
                // Leave it on if a screen reader turned it on: it needs the
                // updates too. (One that starts while agents are driving the
                // app is told apart only by asking again: eframe re-enables it.)
                if !self.screen_reader {
                    ctx.disable_accesskit();
                }
                self.last_call = None;
                self.tree = None;
            }
            return;
        }
        // Every frame: eframe switches it off when a screen reader goes away.
        ctx.enable_accesskit();
        self.last_call = Some(Instant::now());

        // GPU screenshots that arrived (or never will).
        let arrived = std::mem::take(&mut *self.gpu_shots.lock());
        let ppp = ctx.pixels_per_point();
        let mut jobs = std::mem::take(&mut self.jobs);
        for job in &mut jobs {
            if let State::Shot(shot) = &mut job.state
                && let Some((id, deadline)) = shot.gpu
            {
                if let Some((_, img)) = arrived.iter().find(|(i, _)| *i == id) {
                    let call = job.call.take();
                    self.finish_shot(call, shot, img.clone(), ppp, "gpu");
                } else if Instant::now() > deadline {
                    // The OS drew no frame (window hidden on macOS…): draw it ourselves.
                    tracing::debug!("GPU screenshot didn't arrive; using the CPU renderer");
                    shot.gpu = None;
                }
            }
        }
        // Start new jobs; inject the next batch of input for the first input job
        // (one at a time, so clicks and keys never interleave).
        let mut injected = false;
        for job in &mut jobs {
            if matches!(job.state, State::New) {
                self.start(ctx, job);
            }
            if let State::Steps(steps) = &mut job.state {
                if injected {
                    continue;
                }
                injected = true;
                if let Some(batch) = steps.pop_front() {
                    input.events.extend(batch);
                }
                if steps.is_empty() {
                    let done = std::mem::take(&mut job.done);
                    // Answer once what the input caused (a menu opening) is
                    // laid out, so the next call already sees it.
                    job.state = if std::mem::take(&mut job.retry) {
                        State::Retry(2)
                    } else if job.hold.is_zero() {
                        State::Settle(2, done)
                    } else {
                        State::Until(Instant::now() + job.hold, done)
                    };
                }
            }
        }
        jobs.retain(|j| j.call.is_some());
        self.jobs = jobs;
        self.keep_running(ctx);
    }

    fn output_hook(&mut self, ctx: &Context, output: &mut FullOutput) {
        self.textures.apply(&output.textures_delta);
        // Clipboard: remember copies; with $DEN_CLIPBOARD (or no window to own
        // a clipboard) send them there instead of the user's clipboard.
        let intercept = self.clipboard_file.is_some() || self.kind == Kind::Headless;
        let mut copies = Vec::new();
        output.platform_output.commands.retain(|c| match c {
            OutputCommand::CopyText(t) => {
                copies.push(t.clone());
                !intercept
            }
            _ => true,
        });
        for text in copies {
            if let Some(f) = &self.clipboard_file
                && let Err(e) = std::fs::write(f, &text)
            {
                tracing::warn!("DEN_CLIPBOARD {}: {e}", f.display());
            }
            self.ui.set_copied(text);
        }
        if self.kind == Kind::Headless {
            // Screenshots asked for with a viewport command (not through a job).
            let mut shots = Vec::new();
            if let Some(vo) = output.viewport_output.get_mut(&ViewportId::ROOT) {
                vo.commands.retain(|c| match c {
                    ViewportCommand::Screenshot(cb) => {
                        shots.push(cb.clone());
                        false
                    }
                    _ => true,
                });
            }
            if !shots.is_empty() {
                let img = self.cpu_frame(ctx, output);
                for cb in shots {
                    cb.complete(img.clone());
                }
            }
        }
        // Tree updates that we didn't ask for: something else (a screen
        // reader) switched accessibility on.
        if self.kind == Kind::Window && self.last_call.is_none() && output.platform_output.accesskit_update.is_some() {
            self.screen_reader = true;
        }
        if let Some(update) = &output.platform_output.accesskit_update
            && (!self.jobs.is_empty() || self.tree.is_some() || self.kind == Kind::Headless)
        {
            self.tree = Some(Tree::build(ctx, update, &mut self.refs));
        }
        if self.jobs.is_empty() {
            return;
        }
        let ppp = output.pixels_per_point;
        let mut cpu: Option<Arc<ColorImage>> = None;
        let mut jobs = std::mem::take(&mut self.jobs);
        for job in &mut jobs {
            let mut restart = false;
            match &mut job.state {
                State::Retry(n) => {
                    *n = n.saturating_sub(1);
                    restart = *n == 0;
                }
                State::Answer(v) => {
                    let v = if v.is_null() { self.window_info(ctx) } else { std::mem::take(v) };
                    job.answer(Ok(v));
                }
                State::Settle(n, v) => {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        let v = if v.is_null() { self.window_info(ctx) } else { std::mem::take(v) };
                        job.answer(Ok(v));
                    }
                }
                State::Until(at, v) => {
                    if Instant::now() >= *at {
                        let v = std::mem::take(v);
                        job.answer(Ok(v));
                    }
                }
                State::Tree => {
                    let tree = self.tree.clone().unwrap_or_default();
                    let panel = job.str("panel");
                    let all = job.param("all").as_bool().unwrap_or(false);
                    let res = if job.str("format").as_deref() == Some("text") {
                        let rects = job.param("rects").as_bool().unwrap_or(false);
                        json!({"text": tree.text(&self.header(ctx), panel.as_deref(), all, rects)})
                    } else {
                        let mut v = tree.json(panel.as_deref(), all);
                        v["window"] = self.window_info(ctx);
                        v
                    };
                    job.answer(Ok(res));
                }
                State::Wait { deadline, since } => {
                    let (deadline, since) = (*deadline, *since);
                    let sel = job.str("target").unwrap_or_default();
                    let gone = job.param("gone").as_bool().unwrap_or(false);
                    let panel = job.str("panel");
                    let found = self.tree.as_ref().and_then(|t| t.matches(&sel, panel.as_deref()).into_iter().find(|n| !n.hidden).cloned());
                    let ms = since.elapsed().as_millis() as u64;
                    match (found, gone) {
                        (Some(n), false) => job.answer(Ok(json!({"found": n.short(), "after_ms": ms}))),
                        (None, true) => job.answer(Ok(json!({"gone": sel, "after_ms": ms}))),
                        _ if Instant::now() > deadline => {
                            let what = if gone { "to go away" } else { "to appear" };
                            job.answer(Err(format!("timed out after {:.1} s waiting for {sel:?} {what}", ms as f64 / 1000.0)))
                        }
                        _ => {}
                    }
                }
                State::Shot(shot) if shot.gpu.is_none() => {
                    let img = cpu.get_or_insert_with(|| self.cpu_frame(ctx, output)).clone();
                    let call = job.call.take();
                    self.finish_shot(call, shot, img, ppp, "cpu");
                }
                _ => {}
            }
            if restart {
                job.state = State::New;
            }
            // Waits have their own deadline.
            if !matches!(job.state, State::Wait { .. }) && job.started.elapsed() > Duration::from_secs(60) {
                job.answer(Err("gave up after 60 s".into()));
            }
        }
        jobs.retain(|j| j.call.is_some());
        self.jobs = jobs;
        self.keep_running(ctx);
    }

    fn on_exit(&mut self, _ctx: &Context) {
        for mut job in self.jobs.drain(..) {
            job.answer(Err("the app is closing".into()));
        }
        self.ui.detach_frontend();
    }
}

/// Register the plugin and route `ui.*` calls to it.
pub fn attach(ctx: &Context, ui: &Arc<UiStore>, kind: Kind) {
    ctx.add_plugin(AgentPlugin::new(ui.clone(), kind));
    let wake = ctx.clone();
    ui.attach_frontend(kind.name(), move || {
        wake.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::RequestPaintWhileHidden);
        wake.request_repaint();
    });
}

/// Text as an agent should match it: icon glyphs (the icon font lives in
/// Unicode's private use area) dropped, spaces collapsed.
fn readable(s: &str) -> String {
    let kept: String = s.chars().filter(|c| !matches!(*c as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(r: u32, role: &str, text: &str, panel: &str, clickable: bool) -> Node {
        Node {
            r,
            id: NodeId(r as u64),
            role: role.into(),
            text: text.into(),
            value: None,
            rect: Rect::from_min_size(pos2(0.0, r as f32 * 20.0), vec2(100.0, 20.0)),
            panel: Some(panel.into()),
            focused: false,
            disabled: false,
            toggled: None,
            selected: false,
            clickable,
            focusable: false,
            hidden: false,
        }
    }

    fn tree() -> Tree {
        Tree {
            nodes: vec![
                node(1, "button", "Roll", "dice/main", true),
                node(2, "text", "bob rolled 2d6", "core.messages", false),
                node(3, "button", "Roll again", "dice/main", true),
                node(4, "text", "Roll", "core.messages", false),
            ],
            panels: vec![],
            window: Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0)),
        }
    }

    #[test]
    fn selectors() {
        let t = tree();
        assert_eq!(t.find("@3", None, None, true).unwrap().r, 3);
        // Exact text beats substring; two exact matches, one clickable.
        assert_eq!(t.find("Roll", None, None, true).unwrap().r, 1);
        assert!(t.find("Roll", None, None, false).is_err(), "ambiguous without a click preference");
        assert_eq!(t.find("text:Roll", None, None, false).unwrap().r, 4);
        assert_eq!(t.find("roll again", None, None, false).unwrap().r, 3);
        assert_eq!(t.find("2d6", None, None, false).unwrap().r, 2);
        assert_eq!(t.find("Roll", Some("core.messages"), None, false).unwrap().r, 4);
        assert_eq!(t.find("Roll", Some("dice"), Some(0), false).unwrap().r, 1);
        assert!(t.find("nope", None, None, false).is_err());
    }

    #[test]
    fn text_groups_by_panel() {
        let t = tree();
        let s = t.text("window", Some("dice"), false, false);
        assert_eq!(s, "window\ndice/main\n  @1 button \"Roll\"\n  @3 button \"Roll again\"");
    }

    #[test]
    fn text_boxes_take_their_label() {
        let ctx = Context::default();
        ctx.enable_accesskit();
        let mut text = String::from("t");
        let mut out = None;
        for _ in 0..2 {
            let mut o = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.horizontal(|ui| {
                    let label = ui.label("Nick");
                    ui.text_edit_singleline(&mut text).labelled_by(label.id);
                });
            });
            o.textures_delta.clear();
            out = o.platform_output.accesskit_update;
        }
        let update = out.expect("accesskit update");
        let tree = Tree::build(&ctx, &update, &mut Refs::default());
        let tb = tree.nodes.iter().find(|n| n.role == "textbox").expect("a textbox");
        assert_eq!(tb.text, "Nick");
    }

    #[test]
    fn icons_and_spacing_are_dropped() {
        assert_eq!(readable("\u{e1b4}  Copy invite\nlink "), "Copy invite link");
    }

    #[test]
    fn chords() {
        assert_eq!(parse_chord("Enter").unwrap().1, Key::Enter);
        assert_eq!(parse_chord("esc").unwrap().1, Key::Escape);
        let (m, k) = parse_chord("ctrl+shift+a").unwrap();
        assert!(m.ctrl && m.shift && k == Key::A);
        assert!(parse_chord("hyper+a").is_err());
        assert!(parse_chord("F2").is_ok());
    }
}
