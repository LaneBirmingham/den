//! A user's session: the one object every frontend drives.
//!
//! A frontend (the egui shell, the terminal client, a future web/TUI shell)
//! does three things with a [`Session`]:
//!
//! 1. **Read** [`SessionState`] (`session.state()`) and render it however it likes.
//! 2. **Call** methods (`submit`, `voice_join`, `host`, `join`, …).
//! 3. **Listen** via `on_change` (repaint) or `subscribe` (typed-by-name events).
//!
//! The control API exposes exactly the same surface to other processes, so
//! anything a built-in panel can do, a mod or an agent can do too.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use serde::Serialize;
use serde_json::{Value, json};
use tailchat_proto::voice::Codec;
use tailchat_proto::{
    Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, Member, MemberId, MessageKind, NoticeLevel, PROTOCOL_VERSION, ServerInfo,
    ServerMsg, VoiceSetup,
};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;

use crate::api::{self, ApiFuture, ApiHandler, ApiServer, MethodDoc, doc, param_str, ready, require_str};
use crate::config::{self, AppConfig, ServerConfig};
use crate::events::{Event, names};
use crate::mods::{ModHost, ModRuntime};
use crate::net::{read_frame, resolve, write_frame};
use crate::paths::Paths;
use crate::server::{self, ServerHandle, ServerMode, ServerOptions};
use crate::tailscale::{TailnetPeer, TailnetStatus, Tailscale};
use crate::ui::{UiStore, Widget};

const MAX_LOCAL_MESSAGES: usize = 2000;

/// Built-in slash commands: `(usage, description)`. Frontends use this for help and completion.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("/help", "list commands"),
    ("/nick <name>", "change your nick"),
    ("/me <action>", "emote"),
    ("/ch <channel>", "switch text channel"),
    ("/clear", "clear this channel locally"),
    ("/voice [channel]", "join voice"),
    ("/hangup", "leave voice"),
    ("/mute", "mute your mic"),
    ("/unmute", "unmute your mic"),
    ("/deafen", "mute everything"),
    ("/undeafen", "hear again"),
    ("/connect <host[:port]>", "join another room"),
    ("/disconnect", "leave the room"),
    ("/mods", "list mods"),
];

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Idle,
    Connecting { addr: String, attempt: u32 },
    Connected { addr: String },
    Failed { addr: String, error: String },
}

impl ConnState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected { .. })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct HostInfo {
    pub name: String,
    pub listen: SocketAddr,
    /// Addresses friends can paste into *Join*, best first.
    pub share: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct VoiceView {
    /// Voice channel we are in (confirmed by the server).
    pub channel: Option<ChannelId>,
    pub muted: bool,
    pub deafened: bool,
    pub ptt: bool,
    /// Audio devices are open.
    pub engine: bool,
    pub error: Option<String>,
    pub mode: String,
    /// When we joined the current voice channel (unix ms, local clock), for call timers.
    pub joined_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SpeakerView {
    pub id: MemberId,
    pub level: f32,
    pub speaking: bool,
}

/// Serializable voice telemetry (frontends poll this ~15×/s while in voice).
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceStatsView {
    pub input_level: f32,
    pub transmitting: bool,
    pub tx_codec: Option<Codec>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub speakers: Vec<SpeakerView>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DiscoveredRoom {
    pub peer: TailnetPeer,
    /// What to type into *Join* (`host:port`).
    pub addr: String,
    pub server: ServerInfo,
    pub members: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoveryView {
    pub scanning: bool,
    pub rooms: Vec<DiscoveredRoom>,
    pub error: Option<String>,
    pub scanned_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionState {
    pub conn: ConnState,
    pub hosting: Option<HostInfo>,
    pub server: Option<ServerInfo>,
    pub me: Option<MemberId>,
    pub nick: String,
    pub channels: Vec<Channel>,
    pub members: BTreeMap<MemberId, Member>,
    #[serde(skip)]
    pub messages: HashMap<ChannelId, Vec<ChatMessage>>,
    /// Text channel the user is looking at (default target for input).
    pub focus: Option<ChannelId>,
    pub unread: HashMap<ChannelId, u32>,
    /// Unread messages that mention our nick, per channel (a subset of `unread`).
    pub mentions: HashMap<ChannelId, u32>,
    /// Per channel: timestamp of the first message that arrived while we were
    /// looking elsewhere. Frontends draw a "new messages" line above it. Kept
    /// after focusing the channel; cleared when we move away or post there.
    pub new_since: HashMap<ChannelId, i64>,
    pub voice: VoiceView,
    pub latency_ms: Option<u32>,
    pub tailnet: Option<TailnetStatus>,
    pub discovery: DiscoveryView,
    /// Increments on every change.
    pub revision: u64,
}

impl SessionState {
    fn new(nick: String) -> SessionState {
        SessionState {
            conn: ConnState::Idle,
            hosting: None,
            server: None,
            me: None,
            nick,
            channels: Vec::new(),
            members: BTreeMap::new(),
            messages: HashMap::new(),
            focus: None,
            unread: HashMap::new(),
            mentions: HashMap::new(),
            new_since: HashMap::new(),
            voice: VoiceView::default(),
            latency_ms: None,
            tailnet: None,
            discovery: DiscoveryView::default(),
            revision: 0,
        }
    }

    pub fn me(&self) -> Option<&Member> {
        self.me.and_then(|id| self.members.get(&id))
    }

    pub fn text_channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.iter().filter(|c| c.kind == ChannelKind::Text)
    }

    pub fn voice_channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.iter().filter(|c| c.kind == ChannelKind::Voice)
    }

    pub fn members_in_voice<'a>(&'a self, channel: &'a str) -> impl Iterator<Item = &'a Member> {
        self.members.values().filter(move |m| m.voice.as_deref() == Some(channel))
    }

    pub fn messages(&self, channel: &str) -> &[ChatMessage] {
        self.messages.get(channel).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `m` is someone else's message that names us.
    pub fn mentions_me(&self, m: &ChatMessage) -> bool {
        let nick = self.nick.to_lowercase();
        m.kind == MessageKind::User
            && nick.chars().count() >= 2
            && (m.author_id.is_none() || m.author_id != self.me)
            && m.author.to_lowercase() != nick
            && m.text.to_lowercase().contains(&nick)
    }

    fn clear_unread(&mut self) {
        self.unread.clear();
        self.mentions.clear();
        self.new_since.clear();
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    fn first_text(&self) -> Option<ChannelId> {
        self.text_channels().next().map(|c| c.id.clone())
    }
}

pub struct SessionOptions {
    pub paths: Paths,
    pub config: AppConfig,
    /// e.g. `tailchat-gui/0.1.0`.
    pub client_name: String,
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub name: String,
    /// `tailnet`, `localhost`, `all`, or an IP.
    pub bind: String,
    pub port: u16,
    pub persist: bool,
}

struct ConnSlot {
    task: Option<AbortHandle>,
    outbox: Option<mpsc::UnboundedSender<ClientMsg>>,
    server_ip: Option<IpAddr>,
    voice: Option<VoiceSetup>,
}

type Listener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    rt: Handle,
    paths: Paths,
    client_name: String,
    tailscale: Option<Tailscale>,
    config: RwLock<AppConfig>,
    state: RwLock<SessionState>,
    events: broadcast::Sender<Event>,
    listeners: RwLock<Vec<Listener>>,
    epoch: AtomicU64,
    conn: Mutex<ConnSlot>,
    hosted: Mutex<Option<ServerHandle>>,
    ui: Arc<UiStore>,
    mods: RwLock<Arc<ModRuntime>>,
    api: Mutex<Option<ApiServer>>,
    api_env: RwLock<Vec<(String, String)>>,
    voice_wanted: Mutex<Option<ChannelId>>,
    ping: Mutex<Option<(u64, Instant)>>,
    #[cfg(feature = "voice")]
    voice: Mutex<Option<crate::voice::VoiceLink>>,
}

/// Cloneable handle; all clones share one session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

enum End {
    /// Don't reconnect (kicked, rejected).
    Final(String),
    /// Reconnect if we had been connected.
    Retry(String),
}

impl Session {
    /// Create a session on `rt`, start the control API and mods per config.
    pub async fn start(opts: SessionOptions) -> anyhow::Result<Session> {
        let tailscale = Tailscale::locate(opts.config.tailscale_bin.as_deref());
        let tailnet = match &tailscale {
            Some(ts) => ts.status().await.ok(),
            None => None,
        };
        let nick = opts
            .config
            .nick
            .clone()
            .or_else(|| tailnet.as_ref().and_then(|t| t.me.as_ref()?.owner_name.clone()))
            .map(|n| n.split_whitespace().next().unwrap_or(&n).to_owned())
            .unwrap_or_else(config::os_username);
        let mut state = SessionState::new(tailchat_proto::clean_nick(&nick));
        state.tailnet = tailnet;
        state.voice.mode = opts.config.voice.mode.clone();
        let (events, _) = broadcast::channel(1024);
        let modules = opts.config.modules.clone();
        let inner = Arc::new(Inner {
            rt: Handle::current(),
            paths: opts.paths.clone(),
            client_name: opts.client_name,
            tailscale,
            config: RwLock::new(opts.config),
            state: RwLock::new(state),
            events,
            listeners: RwLock::new(Vec::new()),
            epoch: AtomicU64::new(0),
            conn: Mutex::new(ConnSlot { task: None, outbox: None, server_ip: None, voice: None }),
            hosted: Mutex::new(None),
            ui: Arc::new(UiStore::default()),
            mods: RwLock::new(Arc::new(ModRuntime::empty(&opts.paths))),
            api: Mutex::new(None),
            api_env: RwLock::new(Vec::new()),
            voice_wanted: Mutex::new(None),
            ping: Mutex::new(None),
            #[cfg(feature = "voice")]
            voice: Mutex::new(None),
        });
        let session = Session { inner };
        if modules.api {
            let handler: Arc<dyn ApiHandler> = Arc::new(SessionApi(Arc::downgrade(&session.inner)));
            match ApiServer::start(handler, &session.inner.paths).await {
                Ok(server) => {
                    *session.inner.api_env.write() = server.env();
                    *session.inner.api.lock() = Some(server);
                }
                Err(e) => tracing::warn!("control API disabled: {e:#}"),
            }
        }
        if modules.mods {
            session.reload_mods();
        }
        Ok(session)
    }

    // ----- reading -------------------------------------------------------

    pub fn state(&self) -> RwLockReadGuard<'_, SessionState> {
        self.inner.state.read()
    }

    pub fn config(&self) -> AppConfig {
        self.inner.config.read().clone()
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn ui(&self) -> &Arc<UiStore> {
        &self.inner.ui
    }

    pub fn mods(&self) -> Arc<ModRuntime> {
        self.inner.mods.read().clone()
    }

    pub fn tailscale(&self) -> Option<&Tailscale> {
        self.inner.tailscale.as_ref()
    }

    pub fn runtime(&self) -> &Handle {
        &self.inner.rt
    }

    /// Address/token of this session's control API.
    pub fn api_env(&self) -> Vec<(String, String)> {
        self.inner.api_env.read().clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Called (from any thread) after every state change. Use it to repaint.
    pub fn on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        self.inner.listeners.write().push(Arc::new(f));
    }

    pub fn voice_stats(&self) -> Option<VoiceStatsView> {
        self.inner.voice_stats()
    }

    // ----- config --------------------------------------------------------

    /// Mutate and save `config.toml`.
    pub fn update_config(&self, f: impl FnOnce(&mut AppConfig)) {
        let snapshot = {
            let mut c = self.inner.config.write();
            f(&mut c);
            c.clone()
        };
        if let Err(e) = config::save(&self.inner.paths.config_file(), &snapshot) {
            self.notice(NoticeLevel::Error, format!("saving config: {e:#}"));
        }
        self.inner.changed();
    }

    // ----- connecting ----------------------------------------------------

    /// Connect to a room (`host`, `host:port`, MagicDNS name, IP).
    pub fn join(&self, addr: impl Into<String>) {
        let addr = addr.into().trim().to_owned();
        self.inner.stop_connection();
        let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let inner = self.inner.clone();
        let task = self.inner.rt.spawn(conn_task(inner, epoch, addr)).abort_handle();
        self.inner.conn.lock().task = Some(task);
    }

    /// Start a room in this process and join it.
    pub fn host(&self, opts: HostOptions) {
        self.leave();
        let me = self.clone();
        self.inner.update(|s| s.conn = ConnState::Connecting { addr: "starting room…".into(), attempt: 0 });
        self.inner.rt.spawn(async move {
            match me.inner.start_hosting(opts).await {
                Ok(addr) => me.join(addr),
                Err(e) => {
                    let error = format!("{e:#}");
                    me.inner.update(|s| s.conn = ConnState::Failed { addr: "host".into(), error: error.clone() });
                    me.notice(NoticeLevel::Error, format!("could not host: {error}"));
                }
            }
        });
    }

    /// Leave the room (and stop hosting, if we were).
    pub fn leave(&self) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.stop_connection();
        if let Some(h) = self.inner.hosted.lock().take() {
            h.shutdown();
        }
        *self.inner.voice_wanted.lock() = None;
        self.inner.update(|s| {
            s.conn = ConnState::Idle;
            s.hosting = None;
            s.server = None;
            s.me = None;
            s.members.clear();
            s.channels.clear();
            s.messages.clear();
            s.clear_unread();
            s.focus = None;
            s.voice.channel = None;
            s.voice.joined_at = None;
            s.latency_ms = None;
        });
        self.inner.emit(names::CONNECTION_CHANGED, json!({"state": "idle"}));
        self.inner.emit(names::HOSTING_CHANGED, json!({"hosting": null}));
    }

    /// Probe tailnet peers for rooms; results land in `state().discovery`.
    pub fn scan(&self) {
        let Some(ts) = self.inner.tailscale.clone() else {
            self.inner.update(|s| s.discovery.error = Some("Tailscale CLI not found".into()));
            return;
        };
        if !self.config().modules.discovery {
            return;
        }
        let inner = self.inner.clone();
        let port = self.config().port;
        inner.update(|s| {
            s.discovery.scanning = true;
            s.discovery.error = None;
        });
        self.inner.rt.spawn(async move {
            let status = ts.status().await;
            let res = match &status {
                Ok(st) => Ok(discover_rooms(st, port).await),
                Err(e) => Err(format!("{e:#}")),
            };
            inner.update(|s| {
                if let Ok(st) = status {
                    s.tailnet = Some(st);
                }
                s.discovery.scanning = false;
                s.discovery.scanned_at = Some(crate::now_ms());
                match res {
                    Ok(rooms) => s.discovery.rooms = rooms,
                    Err(e) => s.discovery.error = Some(e),
                }
            });
        });
    }

    // ----- chat ----------------------------------------------------------

    pub fn focus_channel(&self, channel: &str) {
        let channel = tailchat_proto::clean_channel_id(channel);
        self.inner.update(|s| {
            if s.channel(&channel).is_some_and(|c| c.kind == ChannelKind::Text) {
                if let Some(old) = s.focus.take().filter(|f| *f != channel) {
                    s.new_since.remove(&old);
                }
                s.unread.remove(&channel);
                s.mentions.remove(&channel);
                s.focus = Some(channel);
            }
        });
    }

    /// Send plain text (no slash-command handling).
    pub fn send_message(&self, channel: Option<&str>, text: &str) {
        let Some(channel) = self.inner.target_channel(channel) else {
            self.notice(NoticeLevel::Warn, "not in a room".into());
            return;
        };
        self.inner.send(ClientMsg::Send { channel, text: text.to_owned() });
    }

    /// Handle composer input: `/commands` (built-in, then mods) or plain text.
    pub fn submit(&self, channel: Option<&str>, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        if let Some(target) = self.inner.target_channel(channel) {
            self.inner.update(|s| s.new_since.remove(&target));
        }
        let Some(rest) = input.strip_prefix('/') else {
            self.send_message(channel, input);
            return;
        };
        if rest.starts_with('/') {
            self.send_message(channel, rest);
            return;
        }
        let (name, arg_str) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let arg_str = arg_str.trim();
        let args = split_args(arg_str);
        if self.config().modules.slash_commands && self.builtin(name, arg_str, channel) {
            return;
        }
        let mods = self.mods();
        if mods.find_command(name).is_some() {
            let host: Arc<dyn ModHost> = Arc::new(self.mod_host());
            let channel = self.inner.target_channel(channel);
            let name = name.to_owned();
            let me = self.clone();
            self.inner.emit(names::COMMAND_RUN, json!({"command": name, "args": args}));
            self.inner.rt.spawn(async move {
                let ctx = json!({"channel": channel, "nick": me.state().nick});
                if let Err(e) = mods.run_command(host, &name, args, channel, ctx).await {
                    me.notice(NoticeLevel::Error, format!("/{name}: {e:#}"));
                }
            });
            return;
        }
        self.notice(NoticeLevel::Warn, format!("unknown command /{name} — try /help"));
    }

    fn builtin(&self, name: &str, arg: &str, channel: Option<&str>) -> bool {
        match name {
            "help" => {
                let mut lines: Vec<String> = BUILTIN_COMMANDS.iter().map(|(u, d)| format!("{u} — {d}")).collect();
                lines.push("//text sends a literal slash".into());
                for (id, c) in self.mods().commands() {
                    let usage = c.usage.clone().unwrap_or_else(|| format!("/{}", c.name));
                    lines.push(format!("{usage} — {} [{id}]", c.description.unwrap_or_default()));
                }
                self.notice(NoticeLevel::Info, lines.join("\n"));
            }
            "nick" if !arg.is_empty() => self.set_nick(arg),
            "me" if !arg.is_empty() => {
                let nick = self.state().me().map(|m| m.nick.clone()).unwrap_or_default();
                self.send_message(channel, &format!("_{nick} {arg}_"));
            }
            "ch" | "channel" if !arg.is_empty() => self.focus_channel(arg),
            "clear" => {
                let ch = self.inner.target_channel(channel);
                self.inner.update(|s| {
                    if let Some(c) = ch {
                        s.messages.remove(&c);
                    }
                });
            }
            "voice" => self.voice_join((!arg.is_empty()).then(|| arg.to_owned())),
            "hangup" => self.voice_leave(),
            "mute" => self.set_muted(true),
            "unmute" => self.set_muted(false),
            "deafen" => self.set_deafened(true),
            "undeafen" => self.set_deafened(false),
            "connect" if !arg.is_empty() => self.join(arg),
            "disconnect" => self.leave(),
            "mods" => {
                let mods = self.mods();
                let mut lines: Vec<String> = mods
                    .mods()
                    .iter()
                    .map(|m| {
                        let status =
                            if !m.enabled { "disabled".to_owned() } else { m.inactive_reason.clone().unwrap_or_else(|| "active".into()) };
                        format!("{} {} — {status}", m.manifest.id, m.manifest.version)
                    })
                    .collect();
                lines.extend(mods.problems().iter().map(|p| format!("! {}: {}", p.path.display(), p.error)));
                if lines.is_empty() {
                    lines.push(format!("no mods installed (put them in {})", self.paths().mods_dir().display()));
                }
                self.notice(NoticeLevel::Info, lines.join("\n"));
            }
            _ => return false,
        }
        true
    }

    pub fn set_nick(&self, nick: &str) {
        let nick = tailchat_proto::clean_nick(nick);
        self.update_config(|c| c.nick = Some(nick.clone()));
        self.inner.update(|s| s.nick = nick.clone());
        if self.state().conn.is_connected() {
            self.inner.send(ClientMsg::SetNick { nick });
        }
    }

    /// Relay a mod-defined payload to the room (or one member).
    pub fn send_app(&self, ns: &str, channel: Option<String>, to: Option<MemberId>, data: Value) {
        self.inner.send(ClientMsg::App { ns: ns.into(), channel, to, data });
    }

    /// Show a local line in the current channel and emit a `notice` event.
    pub fn notice(&self, level: NoticeLevel, text: String) {
        self.inner.notice(level, text);
    }

    /// A widget button or other UI element was activated.
    pub fn ui_action(&self, panel: &str, action: &str, value: Value) {
        self.inner.emit(names::UI_ACTION, json!({"panel": panel, "action": action, "value": value}));
    }

    /// Run a mod command (e.g. from a panel button) as if typed.
    pub fn run_mod_command(&self, mod_id: Option<&str>, command: &str, args: &[String]) {
        let name = match mod_id {
            Some(id) => format!("{id}:{command}"),
            None => command.to_owned(),
        };
        let line = std::iter::once(format!("/{name}")).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
        self.submit(None, &line);
    }

    // ----- voice ---------------------------------------------------------

    pub fn voice_join(&self, channel: Option<String>) {
        if !crate::voice_supported() {
            self.notice(NoticeLevel::Warn, "this build of tailchat has no voice support".into());
            return;
        }
        if !self.config().modules.voice {
            self.notice(NoticeLevel::Warn, "voice is switched off in config ([modules] voice = false)".into());
            return;
        }
        let ch = {
            let s = self.state();
            if !s.conn.is_connected() {
                drop(s);
                self.notice(NoticeLevel::Warn, "not in a room".into());
                return;
            }
            match channel {
                Some(c) => Some(tailchat_proto::clean_channel_id(&c)),
                None => s.voice_channels().next().map(|c| c.id.clone()),
            }
        };
        let Some(ch) = ch else {
            self.notice(NoticeLevel::Warn, "this room has no voice channel".into());
            return;
        };
        *self.inner.voice_wanted.lock() = Some(ch.clone());
        self.inner.send(ClientMsg::VoiceJoin { channel: ch });
        self.inner.ensure_engine();
    }

    pub fn voice_leave(&self) {
        *self.inner.voice_wanted.lock() = None;
        self.inner.send(ClientMsg::VoiceLeave);
        self.inner.stop_engine();
    }

    pub fn set_muted(&self, muted: bool) {
        let deafened = self.state().voice.deafened && muted;
        self.inner.set_voice_flags(muted, deafened);
    }

    /// Deafening also mutes; undeafening restores the mic.
    pub fn set_deafened(&self, deafened: bool) {
        self.inner.set_voice_flags(deafened, deafened);
    }

    /// Push-to-talk key state.
    pub fn set_ptt(&self, pressed: bool) {
        if self.state().voice.ptt == pressed {
            return;
        }
        #[cfg(feature = "voice")]
        if let Some(l) = self.inner.voice.lock().as_ref() {
            l.engine().set_ptt(pressed);
        }
        self.inner.update(|s| s.voice.ptt = pressed);
    }

    /// `voice_activity`, `push_to_talk` or `open`.
    pub fn set_voice_mode(&self, mode: &str) {
        self.update_config(|c| c.voice.mode = mode.to_owned());
        #[cfg(feature = "voice")]
        if let Some(l) = self.inner.voice.lock().as_ref() {
            l.engine().set_mode(crate::voice::tx_mode(mode));
        }
        self.inner.update(|s| s.voice.mode = mode.to_owned());
    }

    pub fn set_vad_threshold(&self, t: f32) {
        self.update_config(|c| c.voice.vad_threshold = t);
        #[cfg(feature = "voice")]
        if let Some(l) = self.inner.voice.lock().as_ref() {
            l.engine().set_vad_threshold(t);
        }
    }

    pub fn set_output_volume(&self, v: f32) {
        self.update_config(|c| c.voice.output_volume = v);
        #[cfg(feature = "voice")]
        if let Some(l) = self.inner.voice.lock().as_ref() {
            l.engine().set_output_volume(v);
        }
    }

    /// Choose audio devices by name (`None` = system default). Restarts the
    /// audio engine if we're in voice so the change applies immediately.
    pub fn set_voice_devices(&self, input: Option<String>, output: Option<String>) {
        self.update_config(|c| {
            c.voice.input_device = input;
            c.voice.output_device = output;
        });
        if self.state().voice.engine {
            self.inner.stop_engine();
            self.inner.ensure_engine();
        }
    }

    // ----- mods ----------------------------------------------------------

    fn mod_host(&self) -> SessionHost {
        SessionHost { inner: Arc::downgrade(&self.inner), ui: self.inner.ui.clone() }
    }

    /// Rescan mod directories and restart mod processes.
    pub fn reload_mods(&self) {
        let enabled = self.config().modules.mods;
        let fresh = Arc::new(ModRuntime::load(&self.inner.paths, "client", enabled));
        let old = std::mem::replace(&mut *self.inner.mods.write(), fresh.clone());
        old.stop();
        let host: Arc<dyn ModHost> = Arc::new(self.mod_host());
        let _guard = self.inner.rt.enter();
        fresh.start(host, self.subscribe());
        self.inner.changed();
    }

    /// Stop everything: room, hosting, voice, mods, API.
    pub fn shutdown(&self) {
        self.leave();
        self.inner.mods.read().stop();
        self.inner.api.lock().take();
    }
}

/// Split `a "b c" d` into `["a", "b c", "d"]`.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// Probe every online peer (and ourselves) for a room on `port`.
pub async fn discover_rooms(status: &TailnetStatus, port: u16) -> Vec<DiscoveredRoom> {
    let mut set = tokio::task::JoinSet::new();
    for peer in status.me.iter().chain(status.peers.iter()).filter(|p| p.online) {
        let Some(ip) = peer.ipv4() else { continue };
        let peer = peer.clone();
        set.spawn(async move {
            let addr = SocketAddr::new(ip, port);
            match crate::net::probe(addr, Duration::from_millis(900)).await {
                Ok(ServerMsg::ProbeInfo { server, members, .. }) => Some(DiscoveredRoom {
                    addr: if port == tailchat_proto::DEFAULT_PORT { peer.short_name() } else { format!("{}:{port}", peer.short_name()) },
                    peer,
                    server,
                    members,
                }),
                _ => None,
            }
        });
    }
    let mut rooms = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(room)) = r {
            rooms.push(room);
        }
    }
    rooms.sort_by(|a, b| b.peer.is_self.cmp(&a.peer.is_self).then(a.peer.host_name.cmp(&b.peer.host_name)));
    rooms
}

async fn conn_task(inner: Arc<Inner>, epoch: u64, addr: String) {
    let mut attempt = 0u32;
    let mut was_connected = false;
    loop {
        if inner.stale(epoch) {
            return;
        }
        inner.update(|s| s.conn = ConnState::Connecting { addr: addr.clone(), attempt });
        let res = run_once(&inner, epoch, &addr, &mut was_connected).await;
        if inner.stale(epoch) {
            return;
        }
        inner.teardown(epoch);
        let (reason, retry) = match res {
            Ok(End::Final(r)) => (r, false),
            Ok(End::Retry(r)) => (r, was_connected),
            Err(e) => (format!("{e:#}"), was_connected),
        };
        if !retry {
            inner.update(|s| s.conn = ConnState::Failed { addr: addr.clone(), error: reason.clone() });
            inner.emit(names::CONNECTION_CHANGED, json!({"state": "failed", "addr": addr, "error": reason}));
            inner.notice(NoticeLevel::Error, format!("disconnected: {reason}"));
            return;
        }
        attempt += 1;
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
        inner.notice(NoticeLevel::Warn, format!("connection lost ({reason}); retrying in {}s", delay.as_secs()));
        inner.emit(names::CONNECTION_CHANGED, json!({"state": "connecting", "addr": addr, "attempt": attempt}));
        tokio::time::sleep(delay).await;
    }
}

async fn run_once(inner: &Arc<Inner>, epoch: u64, addr: &str, was_connected: &mut bool) -> anyhow::Result<End> {
    let port = inner.config.read().port;
    let sock = resolve(addr, port).await?;
    let stream = tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(sock))
        .await
        .with_context(|| format!("timed out connecting to {sock}"))?
        .with_context(|| format!("connecting to {sock}"))?;
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let nick = inner.state.read().nick.clone();
    write_frame(
        &mut wr,
        &ClientMsg::Hello { version: PROTOCOL_VERSION, nick, client: inner.client_name.clone(), codecs: codecs(), features: Vec::new() },
    )
    .await?;
    let first: ServerMsg = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut rd))
        .await
        .context("handshake timed out")??
        .context("server closed during handshake")?;
    match first {
        ServerMsg::Welcome { .. } => {}
        ServerMsg::Error { message, .. } => return Ok(End::Final(message)),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
    if !inner.on_welcome(epoch, addr, sock.ip(), first, tx.clone()) {
        return Ok(End::Final("superseded".into()));
    }
    *was_connected = true;
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write_frame(&mut wr, &m).await.is_err() {
                break;
            }
        }
    });
    let ping_inner = inner.clone();
    let pinger = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let nonce: u64 = rand::random();
            *ping_inner.ping.lock() = Some((nonce, Instant::now()));
            if tx.send(ClientMsg::Ping { nonce }).is_err() {
                break;
            }
        }
    });
    let result = loop {
        match tokio::time::timeout(Duration::from_secs(45), read_frame::<_, ServerMsg>(&mut rd)).await {
            Err(_) => break Ok(End::Retry("server stopped responding".into())),
            Ok(Err(e)) => break Err(e),
            Ok(Ok(None)) => break Ok(End::Retry("server closed the connection".into())),
            Ok(Ok(Some(ServerMsg::Bye { reason }))) => {
                break Ok(if reason.starts_with("kicked") { End::Final(reason) } else { End::Retry(reason) });
            }
            Ok(Ok(Some(msg))) => inner.apply(epoch, msg),
        }
    };
    writer.abort();
    pinger.abort();
    result
}

fn codecs() -> Vec<Codec> {
    #[cfg(feature = "voice")]
    {
        tailchat_audio::supported_codecs()
    }
    #[cfg(not(feature = "voice"))]
    {
        Vec::new()
    }
}

fn local_line(channel: &str, text: String) -> ChatMessage {
    ChatMessage {
        id: 0,
        channel: channel.to_owned(),
        ts: crate::now_ms(),
        author_id: None,
        author: "tailchat".into(),
        author_login: None,
        text,
        kind: MessageKind::System,
    }
}

fn push_capped(list: &mut Vec<ChatMessage>, m: ChatMessage) {
    list.push(m);
    if list.len() > MAX_LOCAL_MESSAGES {
        let excess = list.len() - MAX_LOCAL_MESSAGES;
        list.drain(..excess);
    }
}

impl Inner {
    fn stale(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) != epoch
    }

    fn changed(&self) {
        let listeners: Vec<Listener> = self.listeners.read().clone();
        for l in listeners {
            l();
        }
    }

    fn update<R>(&self, f: impl FnOnce(&mut SessionState) -> R) -> R {
        let r = {
            let mut s = self.state.write();
            let r = f(&mut s);
            s.revision += 1;
            r
        };
        self.changed();
        r
    }

    fn emit(&self, name: &str, data: Value) {
        let _ = self.events.send(Event { event: name.to_owned(), data });
    }

    fn send(&self, msg: ClientMsg) {
        let tx = self.conn.lock().outbox.clone();
        match tx {
            Some(tx) => {
                let _ = tx.send(msg);
            }
            None => self.notice(NoticeLevel::Warn, "not connected".into()),
        }
    }

    fn target_channel(&self, channel: Option<&str>) -> Option<ChannelId> {
        let s = self.state.read();
        match channel {
            Some(c) => Some(tailchat_proto::clean_channel_id(c)),
            None => s.focus.clone().or_else(|| s.first_text()),
        }
    }

    fn notice(&self, level: NoticeLevel, text: String) {
        let connected = self.update(|s| {
            if let Some(ch) = s.focus.clone().or_else(|| s.first_text()) {
                push_capped(s.messages.entry(ch.clone()).or_default(), local_line(&ch, text.clone()));
                true
            } else {
                false
            }
        });
        if !connected || level != NoticeLevel::Info {
            self.ui.toast(level, text.clone());
            self.changed();
        }
        self.emit(names::NOTICE, json!({"level": level, "text": text}));
    }

    fn stop_connection(&self) {
        let mut slot = self.conn.lock();
        if let Some(t) = slot.task.take() {
            t.abort();
        }
        slot.outbox = None;
        slot.server_ip = None;
        slot.voice = None;
        drop(slot);
        self.stop_engine();
    }

    /// After a connection ends: forget live state but keep messages on screen.
    fn teardown(&self, epoch: u64) {
        if self.stale(epoch) {
            return;
        }
        {
            let mut slot = self.conn.lock();
            slot.outbox = None;
            slot.voice = None;
        }
        self.stop_engine();
        self.update(|s| {
            s.members.clear();
            s.me = None;
            s.voice.channel = None;
            s.voice.joined_at = None;
            s.latency_ms = None;
        });
    }

    fn on_welcome(
        self: &Arc<Self>,
        epoch: u64,
        addr: &str,
        server_ip: IpAddr,
        msg: ServerMsg,
        tx: mpsc::UnboundedSender<ClientMsg>,
    ) -> bool {
        let ServerMsg::Welcome { server, you, voice, channels, members, .. } = msg else {
            return false;
        };
        {
            let mut slot = self.conn.lock();
            if self.stale(epoch) {
                return false;
            }
            slot.outbox = Some(tx);
            slot.server_ip = Some(server_ip);
            slot.voice = Some(voice);
        }
        let motd = server.motd.clone();
        let room = server.name.clone();
        self.update(|s| {
            s.conn = ConnState::Connected { addr: addr.to_owned() };
            s.server = Some(server);
            s.me = Some(you);
            s.members = members.into_iter().map(|m| (m.id, m)).collect();
            if let Some(me) = s.members.get(&you) {
                s.nick = me.nick.clone();
            }
            s.channels = channels;
            s.messages.clear();
            s.clear_unread();
            if s.focus.as_ref().is_none_or(|f| s.channel(f).is_none()) {
                s.focus = s.first_text();
            }
        });
        let hosting = self.state.read().hosting.is_some();
        if !hosting {
            let addr = addr.to_owned();
            let snapshot = {
                let mut c = self.config.write();
                c.recent.retain(|r| r != &addr);
                c.recent.insert(0, addr);
                c.recent.truncate(8);
                c.clone()
            };
            let _ = config::save(&self.paths.config_file(), &snapshot);
        }
        self.emit(names::CONNECTION_CHANGED, json!({"state": "connected", "addr": addr, "room": room}));
        if let Some(m) = motd {
            self.notice(NoticeLevel::Info, m);
        }
        if let Some(ch) = self.voice_wanted.lock().clone() {
            self.send(ClientMsg::VoiceJoin { channel: ch });
            self.ensure_engine();
        }
        true
    }

    fn apply(&self, epoch: u64, msg: ServerMsg) {
        if self.stale(epoch) {
            return;
        }
        match msg {
            ServerMsg::Message { message } => {
                let data = json!({"message": message});
                self.update(|s| {
                    let counts = message.kind != MessageKind::System && s.focus.as_deref() != Some(message.channel.as_str());
                    if counts {
                        s.new_since.entry(message.channel.clone()).or_insert(message.ts);
                        *s.unread.entry(message.channel.clone()).or_default() += 1;
                        if s.mentions_me(&message) {
                            *s.mentions.entry(message.channel.clone()).or_default() += 1;
                        }
                    }
                    push_capped(s.messages.entry(message.channel.clone()).or_default(), message);
                });
                self.emit(names::MESSAGE_CREATED, data);
            }
            ServerMsg::History { channel, messages, .. } => self.update(|s| {
                let list = s.messages.entry(channel).or_default();
                let known: std::collections::HashSet<u64> = list.iter().map(|m| m.id).filter(|id| *id != 0).collect();
                let mut merged: Vec<ChatMessage> = messages.into_iter().filter(|m| !known.contains(&m.id)).collect();
                merged.append(list);
                merged.sort_by_key(|m| m.ts);
                *list = merged;
            }),
            ServerMsg::MemberJoined { member } => {
                let line = format!("{} joined", member.nick);
                let data = json!({"member": member});
                self.update(|s| {
                    if let Some(ch) = s.first_text() {
                        push_capped(s.messages.entry(ch.clone()).or_default(), local_line(&ch, line));
                    }
                    s.members.insert(member.id, member);
                });
                self.emit(names::MEMBER_JOINED, data);
            }
            ServerMsg::MemberLeft { id } => {
                let gone = self.update(|s| {
                    let gone = s.members.remove(&id);
                    if let (Some(m), Some(ch)) = (&gone, s.first_text()) {
                        push_capped(s.messages.entry(ch.clone()).or_default(), local_line(&ch, format!("{} left", m.nick)));
                    }
                    gone
                });
                #[cfg(feature = "voice")]
                if let Some(l) = self.voice.lock().as_ref() {
                    l.engine().remove_speaker(id);
                }
                self.renegotiate();
                if let Some(m) = gone {
                    self.emit(names::MEMBER_LEFT, json!({"member": m}));
                }
            }
            ServerMsg::MemberUpdated { member } => {
                let data = json!({"member": member});
                let (mine, voice_before) = self.update(|s| {
                    let mine = s.me == Some(member.id);
                    let before = s.voice.channel.clone();
                    if mine {
                        s.nick = member.nick.clone();
                        if s.voice.channel != member.voice {
                            s.voice.joined_at = member.voice.as_ref().map(|_| crate::now_ms());
                        }
                        s.voice.channel = member.voice.clone();
                        s.voice.muted = member.muted;
                        s.voice.deafened = member.deafened;
                    }
                    s.members.insert(member.id, member.clone());
                    (mine, before)
                });
                if mine && voice_before != member.voice {
                    match &member.voice {
                        Some(ch) => self.emit(names::VOICE_JOINED, json!({"channel": ch})),
                        None => {
                            self.stop_engine();
                            self.emit(names::VOICE_LEFT, json!({"channel": voice_before}));
                        }
                    }
                }
                self.renegotiate();
                self.emit(names::MEMBER_UPDATED, data);
            }
            ServerMsg::ChannelsUpdated { channels } => self.update(|s| s.channels = channels),
            ServerMsg::App { from, ns, channel, data } => {
                self.emit(names::APP_MESSAGE, json!({"from": from, "ns": ns, "channel": channel, "data": data}));
            }
            ServerMsg::Notice { level, text } => self.notice(level, text),
            ServerMsg::Error { code, message } => self.notice(NoticeLevel::Error, format!("{message} ({code})")),
            ServerMsg::Pong { nonce } => {
                let sent = self.ping.lock().take();
                if let Some((n, at)) = sent
                    && n == nonce
                {
                    let ms = at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    self.update(|s| s.latency_ms = Some(ms));
                }
            }
            ServerMsg::Welcome { .. } | ServerMsg::ProbeInfo { .. } | ServerMsg::Bye { .. } => {}
        }
    }

    async fn start_hosting(&self, opts: HostOptions) -> anyhow::Result<String> {
        let bind = server::resolve_bind(&opts.bind, opts.port, self.tailscale.as_ref()).await?;
        // Room settings (channels, motd, limits) come from server.toml when present.
        let base: ServerConfig = config::load_or_default(&self.paths.server_config_file()).unwrap_or_default();
        let config = ServerConfig { name: opts.name.clone(), bind: opts.bind.clone(), port: opts.port, persist: opts.persist, ..base };
        let handle = server::start(ServerOptions {
            config,
            mode: ServerMode::Host,
            bind,
            history_dir: opts.persist.then(|| self.paths.history_dir().join(&opts.name)),
            tailscale: self.tailscale.clone(),
        })
        .await
        .map_err(|e| {
            if format!("{e:#}").contains("in use") {
                anyhow::anyhow!("port {} is already in use — is another room running here? ({e:#})", opts.port)
            } else {
                e
            }
        })?;
        let listen = handle.local_addr();
        let mut share = Vec::new();
        if let Some(ts) = &self.tailscale
            && let Ok(st) = ts.status().await
            && let Some(me) = &st.me
        {
            let suffix = |h: String| if listen.port() == tailchat_proto::DEFAULT_PORT { h } else { format!("{h}:{}", listen.port()) };
            share.push(suffix(me.short_name()));
            if let Some(d) = &me.dns_name {
                share.push(suffix(d.clone()));
            }
            if let Some(ip) = me.ipv4() {
                share.push(format!("{ip}:{}", listen.port()));
            }
        }
        if share.is_empty() || !listen.ip().is_unspecified() && !crate::tailscale::is_tailnet_ip(listen.ip()) {
            share.insert(0, listen.to_string());
        }
        *self.hosted.lock() = Some(handle);
        let info = HostInfo { name: opts.name, listen, share };
        self.emit(names::HOSTING_CHANGED, json!({"hosting": info}));
        self.update(|s| s.hosting = Some(info));
        let connect = if listen.ip().is_unspecified() { SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen.port()) } else { listen };
        Ok(connect.to_string())
    }

    fn set_voice_flags(&self, muted: bool, deafened: bool) {
        #[cfg(feature = "voice")]
        if let Some(l) = self.voice.lock().as_ref() {
            l.engine().set_muted(muted);
            l.engine().set_deafened(deafened);
        }
        self.update(|s| {
            s.voice.muted = muted;
            s.voice.deafened = deafened;
        });
        if self.state.read().conn.is_connected() {
            self.send(ClientMsg::VoiceState { muted, deafened });
        }
    }

    #[cfg(feature = "voice")]
    fn ensure_engine(self: &Arc<Self>) {
        if self.voice.lock().is_some() {
            return;
        }
        let (ip, setup) = {
            let slot = self.conn.lock();
            match (slot.server_ip, slot.voice.clone()) {
                (Some(ip), Some(v)) => (ip, v),
                _ => return,
            }
        };
        let settings = crate::voice::settings(&self.config.read().voice);
        let (muted, deafened) = {
            let s = self.state.read();
            (s.voice.muted, s.voice.deafened)
        };
        let me = self.clone();
        self.rt.spawn_blocking(move || {
            let server = SocketAddr::new(ip, setup.udp_port);
            match crate::voice::VoiceLink::start(server, setup.token, settings) {
                Ok(link) => {
                    link.engine().set_muted(muted);
                    link.engine().set_deafened(deafened);
                    let err = link.engine().stats().last_error;
                    *me.voice.lock() = Some(link);
                    me.update(|s| {
                        s.voice.engine = true;
                        s.voice.error = err;
                    });
                    me.renegotiate();
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    me.update(|s| {
                        s.voice.engine = false;
                        s.voice.error = Some(error.clone());
                    });
                    me.notice(NoticeLevel::Error, format!("voice unavailable: {error}"));
                    *me.voice_wanted.lock() = None;
                    me.send(ClientMsg::VoiceLeave);
                }
            }
        });
    }

    #[cfg(not(feature = "voice"))]
    fn ensure_engine(self: &Arc<Self>) {}

    fn stop_engine(&self) {
        #[cfg(feature = "voice")]
        {
            let link = self.voice.lock().take();
            if link.is_some() {
                // Dropping joins audio threads; keep it off async workers.
                std::thread::spawn(move || drop(link));
            }
        }
        let running = self.state.read().voice.engine;
        if running {
            self.update(|s| s.voice.engine = false);
        }
    }

    /// Pick the best codec everyone in our voice channel can decode.
    fn renegotiate(&self) {
        #[cfg(feature = "voice")]
        {
            let guard = self.voice.lock();
            let Some(link) = guard.as_ref() else { return };
            let s = self.state.read();
            let Some(ch) = s.voice.channel.clone() else { return };
            let listeners: Vec<&[Codec]> = s.members_in_voice(&ch).filter(|m| Some(m.id) != s.me).map(|m| m.codecs.as_slice()).collect();
            let mine = tailchat_audio::supported_codecs();
            link.engine().set_tx_codec(tailchat_proto::voice::negotiate(&mine, listeners));
        }
    }

    fn voice_stats(&self) -> Option<VoiceStatsView> {
        #[cfg(feature = "voice")]
        {
            let guard = self.voice.lock();
            let st = guard.as_ref()?.engine().stats();
            Some(VoiceStatsView {
                input_level: st.input_level,
                transmitting: st.transmitting,
                tx_codec: st.tx_codec,
                input_device: st.input_device,
                output_device: st.output_device,
                speakers: st.speakers.into_iter().map(|sp| SpeakerView { id: sp.id, level: sp.level, speaking: sp.speaking }).collect(),
                error: st.last_error,
            })
        }
        #[cfg(not(feature = "voice"))]
        {
            None
        }
    }
}

/// [`ModHost`] for a client session (weak, so mods never keep a session alive).
struct SessionHost {
    inner: Weak<Inner>,
    ui: Arc<UiStore>,
}

impl SessionHost {
    fn session(&self) -> Option<Session> {
        self.inner.upgrade().map(|inner| Session { inner })
    }
}

impl ModHost for SessionHost {
    fn node_kind(&self) -> &'static str {
        "client"
    }

    fn send_text(&self, channel: Option<String>, text: String) {
        if let Some(s) = self.session() {
            s.send_message(channel.as_deref(), &text);
        }
    }

    fn notice(&self, level: NoticeLevel, text: String) {
        if let Some(s) = self.session() {
            s.notice(level, text);
        }
    }

    fn env(&self) -> Vec<(String, String)> {
        let Some(s) = self.session() else { return Vec::new() };
        let mut env = s.api_env();
        let st = s.state();
        env.push(("TAILCHAT_NICK".into(), st.nick.clone()));
        if let Some(server) = &st.server {
            env.push(("TAILCHAT_ROOM".into(), server.name.clone()));
        }
        if let Some(f) = &st.focus {
            env.push(("TAILCHAT_FOCUS_CHANNEL".into(), f.clone()));
        }
        env
    }

    fn self_id(&self) -> Option<MemberId> {
        self.session().and_then(|s| s.state().me)
    }

    fn ui(&self) -> &UiStore {
        &self.ui
    }
}

/// [`ApiHandler`] for a client session.
struct SessionApi(Weak<Inner>);

const SESSION_METHODS: &[MethodDoc] = &[
    doc("state.get", "{}", "Connection, room, members, channels, voice, focus"),
    doc("messages.list", "{channel?, limit?}", "Recent messages in a channel (default: focused)"),
    doc("message.send", "{text, channel?}", "Send plain text"),
    doc("input.submit", "{text, channel?}", "Send as if typed (slash commands work)"),
    doc("channel.focus", "{channel}", "Switch the focused text channel"),
    doc("nick.set", "{nick}", "Change nick"),
    doc("voice.join", "{channel?}", "Join a voice channel"),
    doc("voice.leave", "{}", "Leave voice"),
    doc("voice.mute", "{muted}", "Mute/unmute mic"),
    doc("voice.deafen", "{deafened}", "Deafen/undeafen"),
    doc("voice.stats", "{}", "Mic level, speakers, devices"),
    doc("app.send", "{ns, data, channel?, to?}", "Relay a mod-defined payload to the room"),
    doc("ui.panel.set", "{panel, widgets, title?}", "Set a panel's widgets (any layout can place it)"),
    doc("ui.panel.clear", "{panel}", "Clear a panel"),
    doc("ui.panels.list", "{}", "Known panels"),
    doc("ui.notify", "{text, level?}", "Show a toast"),
    doc("ui.action", "{panel, action, value?}", "Emit a ui.action event"),
    doc("ui.theme.set", "{name}", "Switch theme (desktop app follows within a second)"),
    doc("ui.layout.set", "{name}", "Switch layout (desktop app follows within a second)"),
    doc("ui.screenshot", "{path?}", "Save a PNG of the app window; returns the path it will be written to"),
    doc("mods.list", "{}", "Installed mods and their status"),
    doc("mods.run", "{command, args?}", "Run a mod slash command"),
    doc("mods.reload", "{}", "Rescan mods and restart their processes"),
    doc("session.host", "{name?, bind?, port?, persist?}", "Host a room from this app"),
    doc("session.join", "{addr}", "Join a room"),
    doc("session.leave", "{}", "Leave the room / stop hosting"),
    doc("tailnet.status", "{}", "Tailnet peers"),
    doc("tailnet.discover", "{}", "Probe peers for rooms (async; see state.get discovery)"),
    doc("config.get", "{}", "Current config.toml values"),
];

fn ok<T: Serialize>(v: T) -> api::ApiResult {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

impl ApiHandler for SessionApi {
    fn kind(&self) -> &'static str {
        "client"
    }

    fn label(&self) -> String {
        self.0.upgrade().map(|i| i.state.read().nick.clone()).unwrap_or_default()
    }

    fn events(&self) -> broadcast::Receiver<Event> {
        match self.0.upgrade() {
            Some(i) => i.events.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    fn methods(&self) -> Vec<MethodDoc> {
        SESSION_METHODS.to_vec()
    }

    fn call(&self, method: &str, p: Value) -> ApiFuture {
        let Some(inner) = self.0.upgrade() else {
            return ready(Err("session closed".into()));
        };
        let s = Session { inner };
        let ch = param_str(&p, "channel");
        let res: api::ApiResult = match method {
            "state.get" => {
                let st = s.state();
                let mut v = serde_json::to_value(&*st).unwrap_or_default();
                v["members"] = serde_json::to_value(st.members.values().collect::<Vec<_>>()).unwrap_or_default();
                v["mods"] = json!(s.mods().active().map(|m| m.manifest.id.clone()).collect::<Vec<_>>());
                v["panels"] = json!(s.ui().panel_ids().into_iter().map(|(id, _)| id).collect::<Vec<_>>());
                Ok(v)
            }
            "messages.list" => {
                let st = s.state();
                let channel = ch.or_else(|| st.focus.clone()).unwrap_or_default();
                let limit = p["limit"].as_u64().unwrap_or(50) as usize;
                let all = st.messages(&channel);
                ok(&all[all.len().saturating_sub(limit)..])
            }
            "message.send" => require_str(&p, "text").map(|t| {
                s.send_message(ch.as_deref(), &t);
                json!({"sent": true})
            }),
            "input.submit" => require_str(&p, "text").map(|t| {
                s.submit(ch.as_deref(), &t);
                json!({"submitted": true})
            }),
            "channel.focus" => require_str(&p, "channel").map(|c| {
                s.focus_channel(&c);
                json!({"focus": s.state().focus})
            }),
            "nick.set" => require_str(&p, "nick").map(|n| {
                s.set_nick(&n);
                json!({"nick": s.state().nick})
            }),
            "voice.join" => {
                s.voice_join(ch);
                Ok(json!({"requested": true}))
            }
            "voice.leave" => {
                s.voice_leave();
                Ok(json!({"left": true}))
            }
            "voice.mute" => {
                s.set_muted(p["muted"].as_bool().unwrap_or(true));
                ok(&s.state().voice)
            }
            "voice.deafen" => {
                s.set_deafened(p["deafened"].as_bool().unwrap_or(true));
                ok(&s.state().voice)
            }
            "voice.stats" => ok(s.voice_stats()),
            "app.send" => require_str(&p, "ns").map(|ns| {
                let to = p["to"].as_u64().map(|v| v as MemberId);
                s.send_app(&ns, ch, to, p.get("data").cloned().unwrap_or(Value::Null));
                json!({"sent": true})
            }),
            "ui.panel.set" => require_str(&p, "panel").and_then(|panel| {
                let widgets: Vec<Widget> =
                    serde_json::from_value(p.get("widgets").cloned().unwrap_or(json!([]))).map_err(|e| format!("bad widgets: {e}"))?;
                s.ui().set_panel(&panel, param_str(&p, "title").as_deref(), widgets);
                s.inner.changed();
                s.inner.emit(names::UI_CHANGED, json!({"panel": panel}));
                Ok(json!({"panel": panel}))
            }),
            "ui.panel.clear" => require_str(&p, "panel").map(|panel| {
                let found = s.ui().clear_panel(&panel);
                s.inner.changed();
                json!({"cleared": found})
            }),
            "ui.panels.list" => ok(s.ui().panels()),
            "ui.notify" => require_str(&p, "text").map(|t| {
                let level = match p["level"].as_str() {
                    Some("warn") => NoticeLevel::Warn,
                    Some("error") => NoticeLevel::Error,
                    _ => NoticeLevel::Info,
                };
                let id = s.ui().toast(level, t);
                s.inner.changed();
                json!({"toast": id})
            }),
            "ui.theme.set" => require_str(&p, "name").map(|n| {
                s.update_config(|c| c.ui.theme = n.clone());
                s.inner.changed();
                json!({"theme": n})
            }),
            "ui.layout.set" => require_str(&p, "name").map(|n| {
                s.update_config(|c| c.ui.layout = n.clone());
                s.inner.changed();
                json!({"layout": n})
            }),
            "ui.screenshot" => {
                let path = match p["path"].as_str() {
                    Some(x) => std::path::PathBuf::from(x),
                    None => s.paths().state.join("screenshots").join(format!("{}.png", crate::now_ms())),
                };
                s.ui().request_screenshot(path.clone());
                s.inner.changed();
                ok(json!({"path": path, "queued": true, "note": "written by the desktop app on its next frame; poll for the file"}))
            }
            "ui.action" => require_str(&p, "panel").and_then(|panel| {
                let action = require_str(&p, "action")?;
                s.ui_action(&panel, &action, p.get("value").cloned().unwrap_or(Value::Null));
                Ok(json!({"emitted": true}))
            }),
            "mods.list" => {
                let m = s.mods();
                Ok(json!({"mods": m.mods(), "problems": m.problems()}))
            }
            "mods.run" => require_str(&p, "command").map(|c| {
                let args: Vec<String> = serde_json::from_value(p["args"].clone()).unwrap_or_default();
                s.run_mod_command(None, &c, &args);
                json!({"started": true})
            }),
            "mods.reload" => {
                s.reload_mods();
                Ok(json!({"mods": s.mods().active().count()}))
            }
            "session.host" => {
                let cfg = s.config();
                s.host(HostOptions {
                    name: param_str(&p, "name").unwrap_or_else(|| format!("{}'s room", s.state().nick)),
                    bind: param_str(&p, "bind").unwrap_or_else(|| "tailnet".into()),
                    port: p["port"].as_u64().map(|v| v as u16).unwrap_or(cfg.port),
                    persist: p["persist"].as_bool().unwrap_or(false),
                });
                Ok(json!({"starting": true}))
            }
            "session.join" => require_str(&p, "addr").map(|a| {
                s.join(a);
                json!({"connecting": true})
            }),
            "session.leave" => {
                s.leave();
                Ok(json!({"left": true}))
            }
            "tailnet.status" => ok(&s.state().tailnet),
            "tailnet.discover" => {
                s.scan();
                Ok(json!({"scanning": true}))
            }
            "config.get" => ok(s.config()),
            other => Err(format!("unknown method {other} (see api.methods)")),
        };
        ready(res)
    }
}

#[cfg(test)]
mod tests {
    use super::split_args;

    #[test]
    fn args_split_with_quotes() {
        assert_eq!(split_args(r#"a "b c" d"#), vec!["a", "b c", "d"]);
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(split_args(r#"'' x"#), vec!["", "x"]);
    }
}
