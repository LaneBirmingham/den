//! User-editable TOML configuration.
//!
//! `config.toml` drives the app (nickname, voice, theme/layout choice, which
//! built-in modules are on). `server.toml` drives a room server. Both are
//! optional; every field has a default.

use std::path::Path;

use anyhow::Context;
use denchat_proto::{Channel, ChannelKind, DEFAULT_PORT};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    /// Display nickname. Defaults to the OS user. `nick = …` (the old key) still works.
    #[serde(rename = "nickname", alias = "nick")]
    pub nick: Option<String>,
    /// Where the app listens: `all` (default), `localhost`, or an IP.
    pub bind: String,
    /// UDP port the app listens on (for rooms it hosts). 0 picks a free one;
    /// a taken port falls back to a free one.
    pub port: u16,
    /// Relays and other network settings.
    pub net: NetConfig,
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
            bind: "all".into(),
            port: 0,
            net: NetConfig::default(),
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
    /// Linear mic gain (1.0 = as captured).
    pub input_gain: f32,
    /// Linear playback gain (1.0 = as received).
    pub output_volume: f32,
    /// Push-to-talk key (`F8`, `Space`, `Mouse4`, …). Unset: the layout's `[keys] ptt`.
    pub ptt_key: Option<String>,
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
            ptt_key: None,
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
    /// Look for live hosts of your saved rooms (in the lobby).
    pub discovery: bool,
    /// Built-in slash commands (`/nickname`, `/voice`, …).
    pub slash_commands: bool,
    /// Let agents and scripts see and drive the app through the control API:
    /// the `ui.*` calls behind `den ui` (widget tree, click, type, keys,
    /// scroll, hover, wait, screenshots, window, zoom, clipboard) and
    /// `app.quit`. Off, those calls are refused; the rest of the API (mods,
    /// `den status`, `den wait`) still works.
    pub agent_control: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Modules { voice: true, mods: true, api: true, discovery: true, slash_commands: true, agent_control: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    pub name: String,
    pub motd: Option<String>,
    /// `all` (default), `localhost`, or an IP to listen on.
    pub bind: String,
    /// UDP port (`den serve`; forward it on your router for the most
    /// direct connections). Apps use their own `port` setting.
    pub port: u16,
    pub channels: Vec<ChannelSpec>,
    /// Messages kept per channel (memory, disk and replay on join).
    pub history_limit: usize,
    /// If non-empty, only these member keys may join (a key or a prefix of
    /// at least 8 characters; members see theirs with `/whoami`).
    pub allow: Vec<String>,
    pub max_members: usize,
    /// Messages a member may send per 10 seconds.
    pub rate_limit: u32,
    /// Relays and other network settings (`den serve`).
    pub net: NetConfig,
    /// The relay `den serve` runs for its room's members.
    pub relay: RelayServeConfig,
}

/// `den serve` runs a relay next to the room, so members who can't
/// reach it directly (strict NATs, UDP blocked) come in through the server
/// itself instead of a third party. It needs its TCP port reachable from
/// outside (forwarded on the router).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct RelayServeConfig {
    pub enabled: bool,
    /// TCP port. 0: the same number as the room's UDP `port`.
    pub port: u16,
    /// The address members use to reach it, e.g. `http://home.example.com:8245`
    /// (a DNS name survives IP changes), or an `https://` URL when a reverse
    /// proxy terminates TLS in front of it. Unset: `http://<public IP>:<port>`,
    /// with the public IP worked out at start.
    pub url: Option<String>,
}

impl Default for RelayServeConfig {
    fn default() -> Self {
        RelayServeConfig { enabled: true, port: 0, url: None }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            name: "den".into(),
            motd: None,
            bind: "all".into(),
            port: DEFAULT_PORT,
            channels: vec![
                ChannelSpec { id: "general".into(), name: None, kind: ChannelKind::Text, topic: None },
                ChannelSpec { id: "lounge".into(), name: None, kind: ChannelKind::Voice, topic: None },
            ],
            history_limit: 500,
            allow: Vec::new(),
            max_members: 64,
            rate_limit: 20,
            net: NetConfig::default(),
            relay: RelayServeConfig::default(),
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

/// How Den reaches people. Connections are peer-to-peer (hole-punched
/// QUIC); a relay carries the traffic only when two peers can't reach each
/// other directly, and never sees inside it (it's end-to-end encrypted).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct NetConfig {
    /// `["default"]`: n0's free public relays. `[]`: no relays (direct
    /// connections only: LAN, public IPs, forwarded ports). Or your own relay
    /// URLs, e.g. `["http://box.example.com:3340"]` from `den relay`.
    /// The `DEN_RELAYS` environment variable overrides this
    /// (`default`, `none`, or comma-separated URLs).
    pub relays: Vec<String>,
    /// Ask the router to forward a UDP port to this machine (UPnP, NAT-PMP or
    /// PCP), so others can connect straight in. On by default; turn it off
    /// to never open ports on the router (connections then hole-punch, or go
    /// through a relay).
    pub port_mapping: bool,
    /// Use direct (IP) paths. `false` makes this endpoint relay-only: it
    /// opens no UDP sockets, advertises no addresses and every connection
    /// goes through a relay (one side being relay-only is enough), so a
    /// relayed link can be tested on demand. Needs a relay. The
    /// `DEN_NO_DIRECT=1` environment variable does the same.
    pub direct: bool,
    /// Relays added by the program (a server's own relay), kept whatever
    /// `DEN_RELAYS` says.
    #[serde(skip)]
    pub extra_relays: Vec<String>,
    /// `DEN_RELAYS` and `DEN_NO_DIRECT` apply. True for configs read from a
    /// file; false for the ones built in code ([`NetConfig::direct_only`],
    /// [`NetConfig::relays`]), so a test means what it says whatever the
    /// shell running it has set.
    #[serde(skip, default = "env_applies")]
    pub env_overrides: bool,
}

fn env_applies() -> bool {
    true
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig { relays: vec!["default".into()], port_mapping: true, direct: true, extra_relays: Vec::new(), env_overrides: true }
    }
}

impl NetConfig {
    /// No relays and no port mapping: direct connections only, and nothing
    /// asked of the router (tests, LAN-only setups).
    pub fn direct_only() -> NetConfig {
        NetConfig { relays: Vec::new(), port_mapping: false, env_overrides: false, ..NetConfig::default() }
    }

    /// Just these relays.
    pub fn relays(urls: impl IntoIterator<Item = String>) -> NetConfig {
        NetConfig { relays: urls.into_iter().collect(), env_overrides: false, ..NetConfig::default() }
    }

    /// Direct paths are on: `direct`, unless `DEN_NO_DIRECT` is set (to
    /// anything but `0`, `false`, `no` or `off`).
    pub fn direct_enabled(&self) -> bool {
        let off = self.env_overrides
            && std::env::var("DEN_NO_DIRECT")
                .is_ok_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no" | "off"));
        self.direct && !off
    }

    /// The relay list after applying `DEN_RELAYS`, plus `extra_relays`.
    pub fn effective_relays(&self) -> Vec<String> {
        let mut v = match std::env::var("DEN_RELAYS").ok().filter(|_| self.env_overrides) {
            Some(v) if !v.trim().is_empty() => match v.trim() {
                "none" | "off" => Vec::new(),
                other => other.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
            },
            _ => self.relays.clone(),
        };
        v.extend(self.extra_relays.iter().cloned());
        v
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
        let id = denchat_proto::clean_channel_id(&self.id);
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
    fn nickname_key_and_old_nick_key() {
        let cfg: AppConfig = toml::from_str("nickname = \"lane\"").unwrap();
        assert_eq!(cfg.nick.as_deref(), Some("lane"));
        assert!(toml::to_string(&cfg).unwrap().contains("nickname = \"lane\""));
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
