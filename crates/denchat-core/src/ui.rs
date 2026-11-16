//! Declarative UI content that mods and scripts push to frontends.
//!
//! A mod never draws pixels. It writes a list of [`Widget`]s into a named
//! panel (via `ui.panel.set` on the control API, or a manifest panel
//! command's stdout). Each frontend decides how to render them: the egui
//! shell draws them in whatever layout slot the user put the panel in, the
//! terminal client can print them, a future web UI can map them to HTML.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use denchat_proto::NoticeLevel;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Widget {
    Heading {
        text: String,
    },
    Text {
        text: String,
        /// `#RRGGBB` or a theme token (`accent`, `muted`, `danger`, `ok`, `warn`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default)]
        mono: bool,
        #[serde(default)]
        small: bool,
    },
    Separator,
    /// Clicking runs `command` (a slash command name of the same mod) with
    /// `args`, and/or emits a `ui.action` event carrying `action`.
    Button {
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
    },
    Kv {
        key: String,
        value: String,
    },
    Progress {
        value: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    List {
        items: Vec<String>,
    },
    Link {
        text: String,
        url: String,
    },
    /// Lay children out horizontally.
    Row {
        children: Vec<Widget>,
    },
}

/// Parse panel command output: a JSON widget array, `{"widgets": [...]}`, or plain text lines.
pub fn parse_widgets(output: &str) -> Vec<Widget> {
    let trimmed = output.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(w) = serde_json::from_str::<Vec<Widget>>(trimmed) {
            return w;
        }
        #[derive(Deserialize)]
        struct Wrapped {
            widgets: Vec<Widget>,
        }
        if let Ok(w) = serde_json::from_str::<Wrapped>(trimmed) {
            return w.widgets;
        }
    }
    trimmed.lines().map(|l| Widget::Text { text: l.to_owned(), color: None, mono: false, small: false }).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PanelContent {
    pub title: String,
    /// Suggested slot (`left`, `right`, `bottom`, …); layouts decide for real.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// Mod id that owns the panel, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub widgets: Vec<Widget>,
    pub updated: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub text: String,
    pub level: NoticeLevel,
    pub ts: i64,
}

/// A control-API call only a graphical frontend can answer: the widget
/// tree, injected input, the window, screenshots (`ui.tree`, `ui.click`, …).
/// The frontend answers through `reply`.
pub struct FrontendCall {
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

impl FrontendCall {
    /// A call made from inside the app; await the receiver for the answer.
    pub fn new(method: &str, params: Value) -> (FrontendCall, oneshot::Receiver<Result<Value, String>>) {
        let (reply, rx) = oneshot::channel();
        (FrontendCall { method: method.to_owned(), params, reply }, rx)
    }

    pub fn answer(self, result: Result<Value, String>) {
        let _ = self.reply.send(result);
    }
}

struct Frontend {
    /// `window` or `headless`.
    kind: String,
    /// Wakes the frontend so it runs a frame (even with its window hidden).
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// Text the app put on the clipboard, newest last.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Copied {
    pub text: String,
    pub ts: i64,
}

#[derive(Default)]
pub struct UiStore {
    panels: RwLock<BTreeMap<String, PanelContent>>,
    toasts: RwLock<VecDeque<Toast>>,
    revision: AtomicU64,
    next_toast: AtomicU64,
    raise: AtomicBool,
    frontend: RwLock<Option<Frontend>>,
    calls: Mutex<Vec<FrontendCall>>,
    copied: Mutex<Option<Copied>>,
}

impl UiStore {
    /// A graphical frontend registers itself so `ui.*` calls reach it.
    /// `wake` must make it run a frame soon, from any thread.
    pub fn attach_frontend(&self, kind: &str, wake: impl Fn() + Send + Sync + 'static) {
        *self.frontend.write() = Some(Frontend { kind: kind.to_owned(), wake: Arc::new(wake) });
    }

    /// The frontend is going away: fail whatever it hasn't answered.
    pub fn detach_frontend(&self) {
        self.frontend.write().take();
        for c in self.calls.lock().drain(..) {
            c.answer(Err("the window closed".into()));
        }
    }

    /// `window`, `headless`, or `None` (terminal client, server).
    pub fn frontend_kind(&self) -> Option<String> {
        self.frontend.read().as_ref().map(|f| f.kind.clone())
    }

    /// Queue a call for the frontend and wake it. Errors at once if there is none.
    pub fn call_frontend(&self, method: &str, params: Value) -> Result<oneshot::Receiver<Result<Value, String>>, String> {
        let wake = match &*self.frontend.read() {
            Some(f) => f.wake.clone(),
            None => {
                return Err(format!(
                    "{method} needs the app's window: this instance has none (terminal client or server). \
                     Start the app (`den`), or one in the background (`den sandbox start`)"
                ));
            }
        };
        let (call, rx) = FrontendCall::new(method, params);
        self.calls.lock().push(call);
        wake();
        Ok(rx)
    }

    /// Frontends call this each frame.
    pub fn take_frontend_calls(&self) -> Vec<FrontendCall> {
        std::mem::take(&mut *self.calls.lock())
    }

    /// The frontend copied `text` to the clipboard (or to `$DEN_CLIPBOARD`).
    pub fn set_copied(&self, text: String) {
        *self.copied.lock() = Some(Copied { text, ts: crate::now_ms() });
    }

    /// What the app last copied, if anything.
    pub fn copied(&self) -> Option<Copied> {
        self.copied.lock().clone()
    }

    /// Ask the graphical frontend (if any) to bring its window to the front.
    pub fn request_raise(&self) {
        self.raise.store(true, Ordering::Relaxed);
        self.bump();
    }

    /// Frontends call this each frame; true once per request.
    pub fn take_raise_request(&self) -> bool {
        self.raise.swap(false, Ordering::Relaxed)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Register a panel so frontends can offer it before it has content.
    pub fn declare_panel(&self, id: &str, title: &str, slot: Option<String>, owner: Option<String>) {
        let mut p = self.panels.write();
        p.entry(id.to_owned()).or_insert_with(|| PanelContent { title: title.to_owned(), slot, owner, widgets: Vec::new(), updated: 0 });
        drop(p);
        self.bump();
    }

    pub fn set_panel(&self, id: &str, title: Option<&str>, widgets: Vec<Widget>) {
        let mut p = self.panels.write();
        let entry = p.entry(id.to_owned()).or_insert_with(|| PanelContent {
            title: id.to_owned(),
            slot: None,
            owner: None,
            widgets: Vec::new(),
            updated: 0,
        });
        if let Some(t) = title {
            entry.title = t.to_owned();
        }
        entry.widgets = widgets;
        entry.updated = crate::now_ms();
        drop(p);
        self.bump();
    }

    pub fn clear_panel(&self, id: &str) -> bool {
        let mut panels = self.panels.write();
        let found = match panels.get_mut(id) {
            // Manifest-declared panels stay registered, just empty.
            Some(p) if p.owner.is_some() => {
                p.widgets.clear();
                true
            }
            // Panels created ad hoc through the API disappear entirely.
            Some(_) => {
                panels.remove(id);
                true
            }
            None => false,
        };
        drop(panels);
        self.bump();
        found
    }

    pub fn panel(&self, id: &str) -> Option<PanelContent> {
        self.panels.read().get(id).cloned()
    }

    pub fn with_panel<R>(&self, id: &str, f: impl FnOnce(Option<&PanelContent>) -> R) -> R {
        f(self.panels.read().get(id))
    }

    pub fn panels(&self) -> BTreeMap<String, PanelContent> {
        self.panels.read().clone()
    }

    pub fn panel_ids(&self) -> Vec<(String, String)> {
        self.panels.read().iter().map(|(k, v)| (k.clone(), v.title.clone())).collect()
    }

    pub fn toast(&self, level: NoticeLevel, text: impl Into<String>) -> u64 {
        let id = self.next_toast.fetch_add(1, Ordering::Relaxed) + 1;
        let mut t = self.toasts.write();
        t.push_back(Toast { id, text: text.into(), level, ts: crate::now_ms() });
        while t.len() > 20 {
            t.pop_front();
        }
        drop(t);
        self.bump();
        id
    }

    /// Toasts newer than `max_age_ms`.
    pub fn toasts(&self, max_age_ms: i64) -> Vec<Toast> {
        let now = crate::now_ms();
        self.toasts.read().iter().filter(|t| now - t.ts <= max_age_ms).cloned().collect()
    }

    pub fn dismiss_toast(&self, id: u64) {
        self.toasts.write().retain(|t| t.id != id);
        self.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_output_forms() {
        let json = r#"[{"type":"heading","text":"Hi"},{"type":"button","label":"Go","command":"roll","args":["6"]}]"#;
        let w = parse_widgets(json);
        assert_eq!(w.len(), 2);
        assert!(matches!(&w[1], Widget::Button { command: Some(c), .. } if c == "roll"));
        let wrapped = parse_widgets(r#"{"widgets":[{"type":"separator"}]}"#);
        assert_eq!(wrapped, vec![Widget::Separator]);
        let text = parse_widgets("line one\nline two\n");
        assert_eq!(text.len(), 2);
    }

    #[test]
    fn frontend_calls_need_a_frontend() {
        let s = UiStore::default();
        assert!(s.call_frontend("ui.tree", Value::Null).is_err());
        let woken = Arc::new(AtomicU64::new(0));
        let w = woken.clone();
        s.attach_frontend("headless", move || {
            w.fetch_add(1, Ordering::Relaxed);
        });
        let mut rx = s.call_frontend("ui.tree", Value::Null).unwrap();
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        let calls = s.take_frontend_calls();
        assert_eq!(calls.len(), 1);
        calls.into_iter().for_each(|c| c.answer(Ok(Value::Bool(true))));
        assert_eq!(rx.try_recv().unwrap(), Ok(Value::Bool(true)));
        let mut pending = s.call_frontend("ui.tree", Value::Null).unwrap();
        s.detach_frontend();
        assert!(pending.try_recv().unwrap().is_err());
        assert_eq!(s.frontend_kind(), None);
    }

    #[test]
    fn store_roundtrip() {
        let s = UiStore::default();
        s.declare_panel("m/p", "Panel", Some("right".into()), Some("m".into()));
        s.set_panel("m/p", None, vec![Widget::Separator]);
        assert_eq!(s.panel("m/p").unwrap().title, "Panel");
        s.set_panel("adhoc", Some("Adhoc"), vec![Widget::Separator]);
        assert!(s.clear_panel("adhoc"));
        assert!(s.panel("adhoc").is_none());
        assert!(s.clear_panel("m/p"));
        assert!(s.panel("m/p").unwrap().widgets.is_empty());
    }
}
