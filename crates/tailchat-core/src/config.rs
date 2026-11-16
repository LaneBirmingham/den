//! User-editable TOML configuration.
//!
//! `config.toml` drives the app (nick, voice, theme/layout choice, which
//! built-in modules are on). `server.toml` drives a room server. Both are
//! optional; every field has a default.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tailchat_proto::{Channel, ChannelKind, DEFAULT_PORT};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    /// Display nick. Defaults to the tailnet display name, then the OS user.
    pub nick: Option<String>,
    /// Port used when hosting and when an address has no port.
    pub port: u16,
    /// Path to the `tailscale` CLI if it is not on PATH.
    pub tailscale_bin: Option<PathBuf>,
    /// Recently joined addresses, newest first.
    pub recent: Vec<String>,
    pub voice: VoiceConfig,
    pub ui: UiConfig,
    pub modules: Modules,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            nick: None,
            port: DEFAULT_PORT,
            tailscale_bin: None,
            recent: Vec::new(),
            voice: VoiceConfig::default(),
            ui: UiConfig::default(),
            modules: Modules::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VoiceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// `voice_activity`, `push_to_talk` or `open`.
    pub mode: String,
    /// RMS gate for voice activity, 0..1.
    pub vad_threshold: f32,
    /// Opus bitrate in bits per second.
    pub bitrate: u32,
    pub input_gain: f32,
    pub output_volume: f32,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            input_device: None,
            output_device: None,
            mode: "voice_activity".into(),
            vad_threshold: 0.015,
            bitrate: 32_000,
            input_gain: 1.0,
            output_volume: 1.0,
        }
    }
}

/// Frontend presentation choices. The GUI reads these; other frontends may ignore them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Theme name (built-in, `themes/<name>.toml`, or one shipped by a mod).
    pub theme: String,
    /// Layout name (built-in, `layouts/<name>.toml`, or one shipped by a mod).
    pub layout: String,
    /// Reload theme/layout files automatically when they change on disk.
    pub hot_reload: bool,
    /// UI zoom factor.
    pub scale: f32,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { theme: "catppuccin".into(), layout: "default".into(), hot_reload: true, scale: 1.0 }
    }
}

/// Built-in modules that can be switched off entirely.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Modules {
    /// Voice chat (needs a build with the `voice` feature).
    pub voice: bool,
    /// Load mods from the mods directory.
    pub mods: bool,
    /// Expose the local control API (mods need this to call back in).
    pub api: bool,
    /// Probe tailnet peers for rooms.
    pub discovery: bool,
    /// Built-in slash commands (`/nick`, `/voice`, …).
    pub slash_commands: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Modules { voice: true, mods: true, api: true, discovery: true, slash_commands: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    pub name: String,
    pub motd: Option<String>,
    /// `tailnet` (default: the machine's Tailscale IPv4), `localhost`, `all`, or an IP.
    pub bind: String,
    pub port: u16,
    pub channels: Vec<ChannelSpec>,
    /// Messages kept per channel (memory, disk and replay on join).
    pub history_limit: usize,
    /// Reject peers that cannot be identified through Tailscale.
    pub require_tailnet: bool,
    /// If non-empty, only these tailnet logins may join.
    pub allow: Vec<String>,
    pub max_members: usize,
    /// Messages a member may send per 10 seconds.
    pub rate_limit: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            name: "tailchat".into(),
            motd: None,
            bind: "tailnet".into(),
            port: DEFAULT_PORT,
            channels: vec![
                ChannelSpec { id: "general".into(), name: None, kind: ChannelKind::Text, topic: None },
                ChannelSpec { id: "lounge".into(), name: None, kind: ChannelKind::Voice, topic: None },
            ],
            history_limit: 500,
            require_tailnet: false,
            allow: Vec::new(),
            max_members: 64,
            rate_limit: 20,
        }
    }
}

impl ServerConfig {
    /// Channels with clean, unique ids.
    pub fn channel_list(&self) -> Vec<Channel> {
        let mut seen = std::collections::HashSet::new();
        self.channels.iter().map(ChannelSpec::to_channel).filter(|c| !c.id.is_empty() && seen.insert(c.id.clone())).collect()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChannelSpec {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_kind")]
    pub kind: ChannelKind,
    #[serde(default)]
    pub topic: Option<String>,
}

fn default_kind() -> ChannelKind {
    ChannelKind::Text
}

impl From<&Channel> for ChannelSpec {
    fn from(c: &Channel) -> ChannelSpec {
        ChannelSpec { id: c.id.clone(), name: (c.name != c.id).then(|| c.name.clone()), kind: c.kind, topic: c.topic.clone() }
    }
}

impl ChannelSpec {
    pub fn to_channel(&self) -> Channel {
        let id = tailchat_proto::clean_channel_id(&self.id);
        Channel { name: self.name.clone().unwrap_or_else(|| id.clone()), id, kind: self.kind, topic: self.topic.clone() }
    }
}

/// Load a TOML file, returning defaults when it does not exist.
pub fn load_or_default<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(value)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Best-effort local username.
pub fn os_username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "anon".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_fills_defaults() {
        let cfg: AppConfig = toml::from_str("nick = \"lane\"\n[ui]\ntheme = \"paper\"").unwrap();
        assert_eq!(cfg.nick.as_deref(), Some("lane"));
        assert_eq!(cfg.ui.theme, "paper");
        assert_eq!(cfg.ui.layout, "default");
        assert!(cfg.modules.voice);
    }

    #[test]
    fn server_channels_parse() {
        let cfg: ServerConfig =
            toml::from_str("name = \"den\"\n[[channels]]\nid = \"Dev Talk\"\n[[channels]]\nid = \"radio\"\nkind = \"voice\"").unwrap();
        let ch: Vec<_> = cfg.channels.iter().map(ChannelSpec::to_channel).collect();
        assert_eq!(ch[0].id, "dev-talk");
        assert_eq!(ch[1].kind, ChannelKind::Voice);
    }
}
