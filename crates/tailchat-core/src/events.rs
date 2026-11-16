//! The event stream every extension point sees.
//!
//! Events are deliberately stringly-typed (`name` + JSON `data`) so mods in
//! any language, the control API and future frontends share one vocabulary
//! without depending on Rust types. Names are dot-separated; subscribers can
//! match exact names, `prefix.*`, or `*`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

impl Event {
    pub fn new(name: &str, data: impl Serialize) -> Event {
        Event { event: name.to_owned(), data: serde_json::to_value(data).unwrap_or(Value::Null) }
    }
}

/// Canonical event names.
pub mod names {
    pub const CONNECTION_CHANGED: &str = "connection.changed";
    pub const MESSAGE_CREATED: &str = "message.created";
    pub const MEMBER_JOINED: &str = "member.joined";
    pub const MEMBER_LEFT: &str = "member.left";
    pub const MEMBER_UPDATED: &str = "member.updated";
    pub const VOICE_JOINED: &str = "voice.joined";
    pub const VOICE_LEFT: &str = "voice.left";
    pub const APP_MESSAGE: &str = "app.message";
    pub const NOTICE: &str = "notice";
    pub const UI_ACTION: &str = "ui.action";
    pub const UI_CHANGED: &str = "ui.changed";
    pub const HOSTING_CHANGED: &str = "hosting.changed";
    pub const COMMAND_RUN: &str = "command.run";

    /// Every name, for docs and `api.methods`.
    pub const ALL: &[&str] = &[
        CONNECTION_CHANGED,
        MESSAGE_CREATED,
        MEMBER_JOINED,
        MEMBER_LEFT,
        MEMBER_UPDATED,
        VOICE_JOINED,
        VOICE_LEFT,
        APP_MESSAGE,
        NOTICE,
        UI_ACTION,
        UI_CHANGED,
        HOSTING_CHANGED,
        COMMAND_RUN,
    ];
}

/// `*`, `prefix.*`, or an exact name.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix(".*") {
        Some(prefix) => name.strip_prefix(prefix).is_some_and(|r| r.starts_with('.')),
        None => pattern == name,
    }
}

pub fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| matches(p, name))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn patterns() {
        assert!(matches("*", "message.created"));
        assert!(matches("message.*", "message.created"));
        assert!(!matches("message.*", "messages.created"));
        assert!(!matches("member.*", "member"));
        assert!(matches("notice", "notice"));
        assert!(!matches("notice", "notice.x"));
    }
}
