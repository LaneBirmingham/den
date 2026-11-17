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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use denchat_proto::voice::Codec;
use denchat_proto::{
    Access, Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, FEATURE_HOST, MAX_BATCH, MAX_MESSAGE_LEN, Member, MemberId,
    MessageKind, NoticeLevel, PROTOCOL_VERSION, Reach, ServerInfo, ServerMsg, VoiceSetup,
};
use iroh::endpoint::Connection;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;

use crate::api::{self, ApiFuture, ApiHandler, ApiServer, MethodDoc, doc, param_str, ready, require_str};
use crate::config::{self, AppConfig, ServerConfig};
use crate::events::{Event, names};
use crate::mods::{ModHost, ModRuntime};
use crate::net::{self, Link, Net, read_frame, write_frame};
use crate::paths::Paths;
use crate::rooms::{self, RoomSave};
use crate::server::{self, HandoffState, ServerHandle, ServerMode, ServerOptions};
use crate::ui::{UiStore, Widget};

const MAX_LOCAL_MESSAGES: usize = 2000;

/// Why the host would drop or refuse `text` as a message, if it would: blank,
/// or over [`MAX_MESSAGE_LEN`] once trailing whitespace is gone.
pub fn message_problem(text: &str) -> Option<String> {
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Some("message is empty".into());
    }
    if text.len() > MAX_MESSAGE_LEN {
        return Some(format!("messages are limited to {MAX_MESSAGE_LEN} bytes (this one is {})", text.len()));
    }
    None
}

/// Built-in slash commands: `(usage, description)`. Frontends use this for help and completion.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("/help", "list commands"),
    ("/nick <name>", "change your nick"),
    ("/me <action>", "emote"),
    ("/ch <channel>", "switch text channel"),
    ("/newchannel [voice] <name>", "add a text (or voice) channel"),
    ("/clear", "clear this channel locally"),
    ("/voice [channel]", "join voice"),
    ("/hangup", "leave voice"),
    ("/mute", "mute your mic"),
    ("/unmute", "unmute your mic"),
    ("/deafen", "mute everything"),
    ("/undeafen", "hear again"),
    ("/connect <invite>", "join another room"),
    ("/invite", "show this room's invite link"),
    ("/whoami", "show your key for this room"),
    ("/disconnect", "leave the room (hands it off if you host it)"),
    ("/handoff [nick]", "hand hosting to another member"),
    ("/takeover", "host this room from your copy (when its host is gone)"),
    ("/mods", "list mods"),
];

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Idle,
    Connecting {
        addr: String,
        attempt: u32,
    },
    Connected {
        addr: String,
    },
    /// `refused`: the host is up but turned us away (join rules, full, version),
    /// so taking the room over can't help.
    Failed {
        addr: String,
        error: String,
        refused: bool,
    },
}

impl ConnState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnState::Connected { .. })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct HostInfo {
    pub name: String,
    pub room_id: String,
    pub term: u64,
    /// Invite links friends can paste into *Join* (they carry the room's
    /// invite key, so share them only with people you want in).
    pub share: Vec<String>,
    /// Where this room is (no invite key): what members use to get back in.
    pub addr: String,
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
    /// A mic test (settings) has the audio devices open outside a call.
    pub testing: bool,
    /// The mic test plays your voice back to you.
    pub loopback: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SpeakerView {
    pub id: MemberId,
    /// Smoothed RMS (0..1) of what we receive from them.
    pub level: f32,
    /// How loud they are while speaking, 0..1 ([`crate::loudness::intensity`]).
    pub intensity: f32,
    pub speaking: bool,
}

/// Serializable voice telemetry (frontends poll this ~15×/s while in voice
/// or while a mic test runs).
#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceStatsView {
    /// Smoothed mic RMS (0..1) after input gain, metered even while muted.
    pub input_level: f32,
    /// How loud you are, 0..1 ([`crate::loudness::intensity`] of `input_level`).
    pub input_intensity: f32,
    /// The gate is open: you're being sent (or would be, in a mic test).
    pub transmitting: bool,
    /// These stats come from a mic test, not a call.
    pub test: bool,
    pub tx_codec: Option<Codec>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub speakers: Vec<SpeakerView>,
    pub error: Option<String>,
    /// Voice packets sent / received since voice started (to check the path
    /// works even when nobody speaks: `open` mode sends silence too).
    pub tx_packets: u64,
    pub rx_packets: u64,
}

/// Audio devices by name, for device pickers.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DevicesView {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

/// One of your saved rooms that someone is hosting right now.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DiscoveredRoom {
    /// Where to connect (the host that answered, as a link).
    pub addr: String,
    /// Short form of the host's endpoint id, for showing people.
    pub host: String,
    pub server: ServerInfo,
    pub members: u32,
}

/// This app's network endpoint.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct NetView {
    /// Endpoint id (z-base-32): what peers dial.
    pub id: String,
    /// Short form, for showing people.
    pub short: String,
    /// The relay this endpoint is reachable through, if any.
    pub relay: Option<String>,
    /// Direct addresses peers can try.
    pub direct: Vec<String>,
    /// Relays are configured (otherwise direct connections only).
    pub relays_on: bool,
    /// Direct (IP) paths are allowed; false with `[net] direct = false` / `DEN_NO_DIRECT=1`.
    pub direct_enabled: bool,
    /// How easily others could connect to this app, as far as it can tell
    /// (a host's check can say more: see the member list).
    pub reach: Reach,
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
    /// Whether our connection to the host is direct or relayed (updated with `latency_ms`).
    pub via: Option<net::Via>,
    /// Our network endpoint, once it's up.
    pub net: Option<NetView>,
    pub discovery: DiscoveryView,
    /// Rooms saved on this machine (every room you've been in), newest first.
    pub rooms: Vec<RoomSave>,
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
            via: None,
            net: None,
            discovery: DiscoveryView::default(),
            rooms: Vec::new(),
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

    /// Id of the room we're in (or were in, while reconnecting).
    pub fn room_id(&self) -> Option<&str> {
        self.server.as_ref().map(|s| s.room_id.as_str()).filter(|id| !id.is_empty())
    }

    /// The connection dropped and isn't back: offer "Host this room". Not
    /// when the host refused us: it's running, so a takeover would find it and
    /// be refused again.
    pub fn host_lost(&self) -> bool {
        self.room_id().is_some()
            && self.hosting.is_none()
            && matches!(self.conn, ConnState::Connecting { attempt: 1.., .. } | ConnState::Failed { refused: false, .. })
    }

    /// We host the room we're in (our own server is the one we're connected to).
    pub fn is_host(&self) -> bool {
        self.serving_this_room()
    }

    /// Members other than us whose app can take over hosting, best placed
    /// first (see [`SessionState::best_host`]).
    pub fn can_take_over(&self) -> impl Iterator<Item = &Member> {
        let mut v: Vec<&Member> = self.members.values().filter(|m| m.can_host && Some(m.id) != self.me).collect();
        v.sort_by_key(|m| (std::cmp::Reverse(m.reach), m.id));
        v.into_iter()
    }

    /// Who should host this room, us included: the member others can reach
    /// most easily (whoever has been here longest, on a tie), among those
    /// whose app can host, leaving out the host we're (or were) connected to.
    pub fn best_host(&self) -> Option<&Member> {
        let host = self.host_endpoint();
        self.members
            .values()
            .filter(|m| m.can_host && (host.is_none() || m.addr.as_deref().and_then(|a| Link::parse(a).ok()).map(|l| l.addr.id) != host))
            .max_by_key(|m| (m.reach, std::cmp::Reverse(m.id)))
    }

    /// Someone else is better placed to host than we are (see
    /// [`SessionState::best_host`]): worth saying before we take over.
    pub fn better_host(&self) -> Option<&Member> {
        let mine = self.me.and_then(|id| self.members.get(&id)).map(|m| m.reach).unwrap_or_default();
        self.best_host().filter(|m| Some(m.id) != self.me && m.reach > mine)
    }

    /// The endpoint of the host we're connected (or connecting) to.
    fn host_endpoint(&self) -> Option<net::EndpointId> {
        let addr = match &self.conn {
            ConnState::Idle => return None,
            ConnState::Connecting { addr, .. } | ConnState::Connected { addr } | ConnState::Failed { addr, .. } => addr,
        };
        Link::parse(addr).ok().map(|l| l.addr.id)
    }

    /// Our own server is the one we're connected to.
    fn serving_this_room(&self) -> bool {
        match (&self.hosting, &self.server) {
            (Some(h), Some(srv)) => h.room_id == srv.room_id && h.term == srv.term && self.conn.is_connected(),
            _ => false,
        }
    }
}

pub struct SessionOptions {
    pub paths: Paths,
    pub config: AppConfig,
    /// e.g. `den-gui/0.1.0`.
    pub client_name: String,
}

/// A new room to host. Channels, motd and limits come from `server.toml`.
#[derive(Clone, Debug)]
pub struct HostOptions {
    pub name: String,
    /// Anyone who can reach you may join (no invite key). Off by default.
    pub open: bool,
}

struct ConnSlot {
    task: Option<AbortHandle>,
    outbox: Option<mpsc::UnboundedSender<ClientMsg>>,
    /// The connection to the host (voice datagrams use it too).
    link: Option<Connection>,
    voice: Option<VoiceSetup>,
}

type Listener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    /// This profile's endpoint key is ours until the session closes.
    key_lock: Mutex<Option<net::KeyLock>>,
    rt: Handle,
    paths: Paths,
    client_name: String,
    /// Our endpoint: started on first use (joining or hosting).
    net: tokio::sync::OnceCell<Net>,
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
    /// A text channel we asked to create: focus it when it appears.
    creating: Mutex<Option<(String, Instant)>>,
    ping: Mutex<Option<(u64, Instant)>>,
    /// Our copy of the room we're in.
    copy: Mutex<Option<rooms::Copy>>,
    /// Our member id on the previous host, to keep "my messages" across reconnects.
    prev_me: Mutex<Option<MemberId>>,
    /// Set once [`Session::close`] has run (it may be called from a signal and the UI at once).
    closed: tokio::sync::OnceCell<()>,
    #[cfg(feature = "voice")]
    voice: Mutex<Option<crate::voice::VoiceLink>>,
    /// Mic test (settings screen): devices open without a call.
    #[cfg(feature = "voice")]
    mic_test: Mutex<Option<denchat_audio::MicTest>>,
    /// Bumped on every mic test start/stop, so a slow start that was
    /// cancelled meanwhile closes its devices instead of installing them.
    #[cfg(feature = "voice")]
    test_epoch: AtomicU64,
}

/// Cloneable handle; all clones share one session.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

enum End {
    /// Don't reconnect (kicked).
    Final(String),
    /// The host answered the handshake with an error (join rules, full room,
    /// version): don't reconnect, and don't offer a takeover.
    Refused(String),
    /// Reconnect if we had been connected.
    Retry(String),
    /// The room moved to another host.
    Moved { to: Vec<String>, term: u64 },
}

impl Session {
    /// Create a session on `rt`, start the control API and mods per config.
    pub async fn start(opts: SessionOptions) -> anyhow::Result<Session> {
        let nick = opts.config.nick.clone().unwrap_or_else(config::os_username);
        let mut state = SessionState::new(denchat_proto::clean_nick(&nick));
        state.rooms = rooms::list(&opts.paths);
        state.voice.mode = opts.config.voice.mode.clone();
        let (events, _) = broadcast::channel(1024);
        let modules = opts.config.modules.clone();
        let key_lock = net::lock_key(&opts.paths.data.join("device.key"))?;
        let inner = Arc::new(Inner {
            key_lock: Mutex::new(Some(key_lock)),
            rt: Handle::current(),
            paths: opts.paths.clone(),
            client_name: opts.client_name,
            net: tokio::sync::OnceCell::new(),
            config: RwLock::new(opts.config),
            state: RwLock::new(state),
            events,
            listeners: RwLock::new(Vec::new()),
            epoch: AtomicU64::new(0),
            conn: Mutex::new(ConnSlot { task: None, outbox: None, link: None, voice: None }),
            hosted: Mutex::new(None),
            ui: Arc::new(UiStore::default()),
            mods: RwLock::new(Arc::new(ModRuntime::empty(&opts.paths))),
            api: Mutex::new(None),
            api_env: RwLock::new(Vec::new()),
            voice_wanted: Mutex::new(None),
            creating: Mutex::new(None),
            ping: Mutex::new(None),
            copy: Mutex::new(None),
            prev_me: Mutex::new(None),
            closed: tokio::sync::OnceCell::new(),
            #[cfg(feature = "voice")]
            voice: Mutex::new(None),
            #[cfg(feature = "voice")]
            mic_test: Mutex::new(None),
            #[cfg(feature = "voice")]
            test_epoch: AtomicU64::new(0),
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
        // Bring the endpoint up now, so joining and hosting are quick, then
        // keep its view current (addresses and NAT facts settle over a few
        // seconds, and change when the network does).
        let weak = Arc::downgrade(&session.inner);
        session.inner.rt.spawn(async move {
            let Some(inner) = weak.upgrade() else { return };
            let net = match inner.net().await {
                Ok(n) => n,
                Err(e) => return tracing::warn!("network: {e:#}"),
            };
            drop(inner);
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                if net.endpoint().is_closed() {
                    break;
                }
                let view = net_view(&net.addr(), net.relays_on(), net.direct_enabled(), net.reach());
                if inner.state.read().net.as_ref() != Some(&view) {
                    inner.update(|s| s.net = Some(view));
                }
            }
        });
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

    /// Our network endpoint, if it's up.
    pub fn net(&self) -> Option<Net> {
        self.inner.net.get().cloned()
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

    /// Connect to a room by link: an invite (`den://…?room=…&invite=…`)
    /// or a host address.
    pub fn join(&self, addr: impl Into<String>) {
        let addr = addr.into().trim().to_owned();
        self.inner.connect(vec![addr]);
    }

    /// Start a new room in this process and join it. Everyone who joins
    /// keeps a copy, so any of them can host it later.
    pub fn host(&self, opts: HostOptions) {
        self.leave();
        let me = self.clone();
        self.inner.update(|s| s.conn = ConnState::Connecting { addr: "starting room…".into(), attempt: 0 });
        self.inner.rt.spawn(async move {
            let base: ServerConfig = config::load_or_default(&me.paths().server_config_file()).unwrap_or_default();
            let mut save = RoomSave::new(&ServerConfig { name: opts.name.clone(), ..base });
            if opts.open {
                save.invite = None;
            }
            match me.inner.start_hosting(save, 1).await {
                Ok(info) => me.join(info.addr),
                Err(e) => me.inner.host_failed(e),
            }
        });
    }

    /// Open a saved room (by id or name): join its live host if someone is
    /// hosting it, otherwise host it from our copy. This is "Host this room"
    /// when the host has gone.
    pub fn open_room(&self, room: &str) {
        let Some(save) = rooms::find(&self.inner.paths, room) else {
            self.notice(NoticeLevel::Warn, format!("no saved room {room:?}"));
            return;
        };
        let hosting = self.state().hosting.clone();
        if let Some(h) = hosting.filter(|h| h.room_id == save.id) {
            self.join(h.addr);
            return;
        }
        // Stay on the room screen if this is the room we're already looking at.
        if self.state().room_id() == Some(save.id.as_str()) {
            self.inner.epoch.fetch_add(1, Ordering::SeqCst);
            self.inner.stop_connection();
        } else {
            self.leave();
        }
        let epoch = self.inner.epoch.load(Ordering::SeqCst);
        let name = save.name.clone();
        self.inner.update(|s| s.conn = ConnState::Connecting { addr: name, attempt: 0 });
        let me = self.clone();
        self.inner.rt.spawn(async move {
            let live = match me.inner.net().await {
                Ok(net) => rooms::find_live(&net, &save.id, &save.places()).await,
                Err(e) => return me.inner.host_failed(e),
            };
            if me.inner.stale(epoch) {
                return;
            }
            if let Some(live) = live {
                // Where it answered first, then everywhere else it was seen.
                let mut addrs = vec![live.addr];
                addrs.extend(save.addrs.iter().filter(|a| !addrs.contains(a)).cloned().collect::<Vec<_>>());
                me.inner.connect(addrs);
                return;
            }
            let term = save.term + 1;
            match me.inner.start_hosting(save, term).await {
                Ok(info) if !me.inner.stale(epoch) => {
                    me.notice(NoticeLevel::Info, format!("you're hosting {} now", info.name));
                    me.join(info.addr);
                }
                Ok(_) => {}
                Err(e) => me.inner.host_failed(e),
            }
        });
    }

    /// Delete a saved room (not the one you're in).
    pub fn forget_room(&self, room: &str) -> anyhow::Result<()> {
        let save = rooms::find(&self.inner.paths, room).with_context(|| format!("no saved room {room:?}"))?;
        if self.state().room_id() == Some(save.id.as_str()) {
            bail!("leave the room before forgetting it");
        }
        rooms::forget(&self.inner.paths, &save.id)?;
        self.inner.refresh_rooms();
        Ok(())
    }

    /// While hosting: ask a member (`None`: the first who can) to take over.
    /// Everyone, us included, moves to them once they're up.
    pub fn handoff(&self, to: Option<MemberId>) {
        let Some(h) = self.inner.hosted.lock().clone() else {
            self.notice(NoticeLevel::Warn, "you're not hosting this room".into());
            return;
        };
        let me = self.state().me;
        if to.is_some() && to == me {
            self.notice(NoticeLevel::Warn, "you're already the host".into());
            return;
        }
        match h.hand_off(to, me) {
            Ok(m) => self.notice(NoticeLevel::Info, format!("asking {} to take over hosting…", m.nick)),
            Err(e) => self.notice(NoticeLevel::Warn, format!("{e:#}")),
        }
    }

    /// Leave the room. If we host it and someone else can, they take it
    /// over (in the background); otherwise the room stops.
    pub fn leave(&self) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        let me = self.state().me;
        self.inner.stop_connection();
        let hosted = self.inner.hosted.lock().take();
        if let Some(h) = hosted {
            match h.hand_off(None, me) {
                Ok(_) => {
                    self.inner.rt.spawn(async move {
                        h.wait_handoff(Duration::from_secs(10)).await;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        h.shutdown();
                    });
                }
                Err(_) => h.shutdown(),
            }
        }
        *self.inner.copy.lock() = None;
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
            s.via = None;
        });
        self.inner.emit(names::CONNECTION_CHANGED, json!({"state": "idle"}));
        self.inner.emit(names::HOSTING_CHANGED, json!({"hosting": null}));
    }

    /// Look for live hosts of every saved room; results land in `state().discovery`.
    pub fn scan(&self) {
        if !self.config().modules.discovery {
            return;
        }
        let inner = self.inner.clone();
        inner.update(|s| {
            s.discovery.scanning = true;
            s.discovery.error = None;
        });
        self.inner.rt.spawn(async move {
            let res = match inner.net().await {
                Ok(net) => Ok(discover_rooms(&net, rooms::list(&inner.paths)).await),
                Err(e) => Err(format!("{e:#}")),
            };
            inner.update(|s| {
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

    /// Switch to text channel `channel`; false if there's no such channel.
    pub fn focus_channel(&self, channel: &str) -> bool {
        let channel = denchat_proto::clean_channel_id(channel);
        self.inner.update(|s| {
            if !s.channel(&channel).is_some_and(|c| c.kind == ChannelKind::Text) {
                return false;
            }
            if let Some(old) = s.focus.take().filter(|f| *f != channel) {
                s.new_since.remove(&old);
            }
            s.unread.remove(&channel);
            s.mentions.remove(&channel);
            s.focus = Some(channel);
            true
        })
    }

    /// Add a text or voice channel to the room (anyone in it may). The host
    /// tells everyone; a new text channel you made gets the focus.
    pub fn create_channel(&self, name: &str, kind: ChannelKind) {
        if !self.state().conn.is_connected() {
            self.notice(NoticeLevel::Warn, "not in a room".into());
            return;
        }
        let name = name.trim().to_owned();
        if denchat_proto::channel_slug(&name).is_empty() {
            self.notice(NoticeLevel::Warn, "channel names need a letter or a digit".into());
            return;
        }
        if kind == ChannelKind::Text {
            *self.inner.creating.lock() = Some((denchat_proto::channel_slug(&name), Instant::now()));
        }
        self.inner.send(ClientMsg::CreateChannel { name, kind });
    }

    /// Send plain text (no slash-command handling).
    pub fn send_message(&self, channel: Option<&str>, text: &str) {
        if let Err(e) = self.try_send_message(channel, text) {
            self.notice(NoticeLevel::Warn, e);
        }
    }

    /// [`Session::send_message`], but says why the text can't go (empty, too
    /// long, no room) instead of posting a notice.
    pub fn try_send_message(&self, channel: Option<&str>, text: &str) -> Result<(), String> {
        if let Some(problem) = message_problem(text) {
            return Err(problem);
        }
        let channel = self.inner.target_channel(channel).ok_or("not in a room")?;
        self.inner.send(ClientMsg::Send { channel, text: text.to_owned() });
        Ok(())
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
            "ch" | "channel" if !arg.is_empty() => {
                if !self.focus_channel(arg) {
                    self.notice(NoticeLevel::Warn, format!("no text channel #{}", denchat_proto::clean_channel_id(arg)));
                }
            }
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
            "newchannel" => match arg.split_once(' ').map(|(k, n)| (k, n.trim())) {
                Some(("voice", name)) => self.create_channel(name, ChannelKind::Voice),
                Some(("text", name)) => self.create_channel(name, ChannelKind::Text),
                _ if arg.is_empty() => self.notice(NoticeLevel::Warn, "usage: /newchannel [voice] <name>".into()),
                _ => self.create_channel(arg, ChannelKind::Text),
            },
            "mute" => self.set_muted(true),
            "unmute" => self.set_muted(false),
            "deafen" => self.set_deafened(true),
            "undeafen" => self.set_deafened(false),
            "connect" if !arg.is_empty() => self.join(arg),
            "disconnect" => self.leave(),
            "invite" => match self.invite_link() {
                Some(link) => self.notice(NoticeLevel::Info, format!("invite link (anyone with it can join):\n{link}")),
                None => self.notice(NoticeLevel::Warn, "not in a room yet".into()),
            },
            "whoami" => {
                let st = self.state();
                let line = match (st.me(), st.room_id()) {
                    (Some(m), Some(_)) => match &m.identity.key {
                        Some(k) => format!("you're {} in this room, key {k}", m.nick),
                        None => format!("you're {} (no key)", m.nick),
                    },
                    _ => "not in a room".into(),
                };
                drop(st);
                self.notice(NoticeLevel::Info, line);
            }
            "handoff" => {
                let found = self.state().members.values().find(|m| m.nick.eq_ignore_ascii_case(arg)).map(|m| m.id);
                match (arg.is_empty(), found) {
                    (true, _) => self.handoff(None),
                    (false, Some(id)) => self.handoff(Some(id)),
                    (false, None) => self.notice(NoticeLevel::Warn, format!("nobody called {arg} here")),
                }
            }
            "takeover" => {
                let (id, better) = {
                    let s = self.state();
                    (
                        s.room_id().map(str::to_owned),
                        s.better_host().map(|m| format!("{} is better placed to host ({})", m.nick, m.reach.short())),
                    )
                };
                if let Some(b) = better {
                    self.notice(NoticeLevel::Info, format!("{b}; hosting from here anyway"));
                }
                match id {
                    Some(id) => self.open_room(&id),
                    None => self.notice(NoticeLevel::Warn, "not in a room".into()),
                }
            }
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

    /// An invite link to the room we're in: where it's hosted now, plus our
    /// saved invite key (if the room has one).
    pub fn invite_link(&self) -> Option<String> {
        let st = self.state();
        if let Some(h) = st.hosting.as_ref().filter(|h| Some(h.room_id.as_str()) == st.room_id()) {
            return h.share.first().cloned();
        }
        let id = st.room_id()?.to_owned();
        let addr = match &st.conn {
            ConnState::Connected { addr } | ConnState::Connecting { addr, .. } | ConnState::Failed { addr, .. } => addr.clone(),
            ConnState::Idle => return None,
        };
        drop(st);
        let save = rooms::load(&self.inner.paths, &id).ok();
        let link = Link::parse(&addr).ok().or_else(|| save.as_ref().and_then(|s| s.addrs.first()).and_then(|a| Link::parse(a).ok()))?;
        Some(link.room(id).invite(save.and_then(|s| s.invite)).to_string())
    }

    pub fn set_nick(&self, nick: &str) {
        let nick = denchat_proto::clean_nick(nick);
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
            self.notice(NoticeLevel::Warn, "this build of Den has no voice support".into());
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
                Some(c) => Some(denchat_proto::clean_channel_id(&c)),
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
        self.inner.each_engine(|e| e.set_ptt(pressed));
        self.inner.update(|s| s.voice.ptt = pressed);
    }

    /// `voice_activity`, `push_to_talk` or `open`. Anything else is refused.
    pub fn set_voice_mode(&self, mode: &str) {
        let mode = match mode {
            "voice_activity" | "vad" => "voice_activity",
            "push_to_talk" | "ptt" => "push_to_talk",
            "open" | "always" => "open",
            other => {
                self.notice(NoticeLevel::Warn, format!("unknown voice mode \"{other}\" (voice_activity, push_to_talk or open)"));
                return;
            }
        };
        self.update_config(|c| c.voice.mode = mode.to_owned());
        #[cfg(feature = "voice")]
        self.inner.each_engine(|e| e.set_mode(crate::voice::tx_mode(mode)));
        self.inner.update(|s| s.voice.mode = mode.to_owned());
    }

    /// Voice-activity gate: mic RMS (0..1) above which you're sent.
    pub fn set_vad_threshold(&self, t: f32) {
        let t = t.clamp(0.0, 1.0);
        self.update_config(|c| c.voice.vad_threshold = t);
        #[cfg(feature = "voice")]
        self.inner.each_engine(|e| e.set_vad_threshold(t));
    }

    /// Playback gain (1.0 = as received, up to 4.0).
    pub fn set_output_volume(&self, v: f32) {
        let v = v.clamp(0.0, 4.0);
        self.update_config(|c| c.voice.output_volume = v);
        #[cfg(feature = "voice")]
        self.inner.each_engine(|e| e.set_output_volume(v));
    }

    /// Mic gain (1.0 = as captured, up to 16.0).
    pub fn set_input_gain(&self, g: f32) {
        let g = g.clamp(0.0, 16.0);
        self.update_config(|c| c.voice.input_gain = g);
        #[cfg(feature = "voice")]
        self.inner.each_engine(|e| e.set_input_gain(g));
    }

    /// The push-to-talk key (`F8`, `Space`, `Mouse4`, …); `None` falls back
    /// to the layout's `[keys] ptt`. Frontends read it from the config.
    pub fn set_ptt_key(&self, key: Option<String>) {
        let key = key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty());
        self.update_config(|c| c.voice.ptt_key = key);
    }

    /// Choose audio devices by name (`None` = system default). Restarts the
    /// audio engine (or the mic test) so the change applies immediately.
    pub fn set_voice_devices(&self, input: Option<String>, output: Option<String>) {
        self.update_config(|c| {
            c.voice.input_device = input;
            c.voice.output_device = output;
        });
        if self.state().voice.engine {
            self.inner.stop_engine();
            self.inner.ensure_engine();
        }
        if self.state().voice.testing {
            self.voice_test(false);
            self.voice_test(true);
        }
    }

    /// Audio devices on this machine (empty without voice support). Can take
    /// a moment: device enumeration talks to the OS audio stack.
    pub fn voice_devices(&self) -> anyhow::Result<DevicesView> {
        #[cfg(feature = "voice")]
        {
            let d = denchat_audio::list_devices()?;
            Ok(DevicesView { inputs: d.inputs, outputs: d.outputs, default_input: d.default_input, default_output: d.default_output })
        }
        #[cfg(not(feature = "voice"))]
        {
            Ok(DevicesView::default())
        }
    }

    /// Open the audio devices without a call, so a settings screen can show
    /// the mic level (and whether the gate would open). Does nothing while in
    /// voice: the call's own stats serve. Joining voice stops the test.
    pub fn voice_test(&self, on: bool) {
        self.inner.voice_test(on);
    }

    /// While a mic test runs: play your mic back to you (as others would hear it).
    pub fn set_voice_loopback(&self, on: bool) {
        #[cfg(feature = "voice")]
        if let Some(t) = self.inner.mic_test.lock().as_ref() {
            t.set_loopback(on);
        }
        self.inner.update(|s| s.voice.loopback = on);
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

    /// Like [`shutdown`](Self::shutdown), but if we host the room and someone
    /// else can, wait (a few seconds at most) for them to take it over first.
    /// Call this when the app is closing.
    /// Safe to call more than once and from several places at once (window
    /// close and a signal): later calls wait for the first.
    pub async fn close(&self) {
        let me = self.clone();
        self.inner
            .closed
            .get_or_init(|| async move {
                let hosted = me.inner.hosted.lock().clone();
                let my_id = me.state().me;
                // We're going: don't follow the room to its next host (and
                // start voice up again there) while the handoff runs.
                me.inner.epoch.fetch_add(1, Ordering::SeqCst);
                me.inner.stop_connection();
                *me.inner.voice_wanted.lock() = None;
                match hosted.map(|h| (h.hand_off(None, my_id), h)) {
                    Some((Ok(m), h)) => {
                        tracing::info!("closing: handing the room to {}", m.nick);
                        match h.wait_handoff(Duration::from_secs(6)).await {
                            HandoffState::Moved { to, .. } => {
                                tracing::info!("handed the room to {}", to.first().map_or("?", |a| a.as_str()))
                            }
                            other => tracing::warn!("handoff didn't finish ({other:?}); closing anyway"),
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    Some((Err(e), _)) => tracing::info!("closing: the room stops ({e:#})"),
                    None => tracing::info!("closing"),
                }
                me.shutdown();
                // Close the endpoint properly so peers see us go at once.
                if let Some(net) = me.inner.net.get() {
                    net.close().await;
                }
                me.inner.key_lock.lock().take();
            })
            .await;
    }

    /// Stop everything: room, hosting, voice, mods, API.
    pub fn shutdown(&self) {
        tracing::debug!("session shutting down");
        self.leave();
        self.voice_test(false);
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

/// Which of `saves` someone is hosting right now (looked for at each one's
/// last hosts and members' apps).
pub async fn discover_rooms(net: &Net, saves: Vec<RoomSave>) -> Vec<DiscoveredRoom> {
    let mut set = tokio::task::JoinSet::new();
    for save in saves {
        let net = net.clone();
        set.spawn(async move {
            let live = rooms::find_live(&net, &save.id, &save.places()).await?;
            let host = Link::parse(&live.addr).map(|l| l.short()).unwrap_or_default();
            Some(DiscoveredRoom { addr: live.addr, host, server: live.server, members: live.members })
        });
    }
    let mut found = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(room)) = r {
            found.push(room);
        }
    }
    found.sort_by(|a, b| a.server.name.cmp(&b.server.name));
    found
}

/// Connect to the first of `addrs` that answers; reconnect with backoff;
/// follow the room when it moves to another host.
async fn conn_task(inner: Arc<Inner>, epoch: u64, mut addrs: Vec<String>) {
    let mut attempt = 0u32;
    let mut was_connected = false;
    let mut last_move: Option<(Vec<String>, u64)> = None;
    loop {
        if inner.stale(epoch) {
            return;
        }
        let addr = addrs.first().cloned().unwrap_or_default();
        inner.update(|s| s.conn = ConnState::Connecting { addr: addr.clone(), attempt });
        let res = run_addrs(&inner, epoch, &addrs, &mut was_connected).await;
        if inner.stale(epoch) {
            return;
        }
        inner.teardown(epoch);
        let (reason, retry, refused) = match res {
            Ok(End::Moved { to, term }) => {
                if last_move.as_ref().is_some_and(|(t, n)| *t == to && *n == term) {
                    // The old host answered for the new one again (they can
                    // share a port for a moment): give it time to let go.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                } else {
                    tracing::info!("the room moved to {to:?} (term {term})");
                    addrs = inner.moved(to.clone(), term);
                    // Redirected before we ever got in (an invite from an
                    // earlier host): we have no saved copy to take the room
                    // and invite from, so bring them along from the link.
                    if let Ok(from) = Link::parse(&addr)
                        && from.room.is_some()
                    {
                        addrs = addrs.into_iter().map(|a| carry_room(&a, &from)).collect();
                    }
                    last_move = Some((to, term));
                }
                attempt = 0;
                continue;
            }
            Ok(End::Final(r)) => (r, false, false),
            Ok(End::Refused(r)) => (r, false, true),
            Ok(End::Retry(r)) => (r, was_connected, false),
            Err(e) => (format!("{e:#}"), was_connected, false),
        };
        if !retry {
            tracing::info!("not reconnecting to {addr}: {reason}");
            inner.update(|s| s.conn = ConnState::Failed { addr: addr.clone(), error: reason.clone(), refused });
            inner.emit(names::CONNECTION_CHANGED, json!({"state": "failed", "addr": addr, "error": reason, "refused": refused}));
            inner.notice(NoticeLevel::Error, format!("disconnected: {reason}"));
            return;
        }
        attempt += 1;
        tracing::info!("connection to {addr} lost ({reason}); reconnect attempt {attempt}");
        // The host may have handed the room on while we were away: look for it.
        if attempt >= 2
            && let Some(found) = inner.locate().await
            && !addrs.contains(&found)
        {
            if inner.stale(epoch) {
                return;
            }
            inner.notice(NoticeLevel::Info, format!("found the room at {found}"));
            addrs = vec![found];
            continue;
        }
        let delay = Duration::from_secs((1u64 << attempt.min(5)).min(30));
        // One line that updates, not a new one every 30 s.
        inner.status_notice(
            NoticeLevel::Warn,
            format!("connection lost ({reason}); retrying in {}s", delay.as_secs()),
            "connection lost (",
        );
        inner.update(|s| s.conn = ConnState::Connecting { addr: addr.clone(), attempt });
        inner.emit(names::CONNECTION_CHANGED, json!({"state": "connecting", "addr": addr, "attempt": attempt}));
        tokio::time::sleep(delay).await;
    }
}

/// `to` with `from`'s room and invite where it has none of its own (and
/// names the same room, or none).
fn carry_room(to: &str, from: &Link) -> String {
    match Link::parse(to) {
        Ok(l) if l.room.is_none() || l.room == from.room => {
            let invite = l.invite.clone().or_else(|| from.invite.clone());
            let room = l.room.clone().or_else(|| from.room.clone());
            let mut l = l.invite(invite);
            l.room = room;
            l.to_string()
        }
        _ => to.to_owned(),
    }
}

/// Try each address in turn until one connects.
async fn run_addrs(inner: &Arc<Inner>, epoch: u64, addrs: &[String], was_connected: &mut bool) -> anyhow::Result<End> {
    let mut last = None;
    for addr in addrs {
        match run_once(inner, epoch, addr, was_connected).await {
            Err(e) => {
                tracing::info!("couldn't connect to {addr}: {e:#}");
                last = Some(e);
            }
            done => return done,
        }
        if inner.stale(epoch) {
            break;
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no address to connect to")))
}

/// One connection. `Err` means we never got in (try the next address).
async fn run_once(inner: &Arc<Inner>, epoch: u64, addr: &str, was_connected: &mut bool) -> anyhow::Result<End> {
    let link = Link::parse(addr)?;
    let net = inner.net().await?;
    // Which room: the link's, else the one we're in (a handoff), else ask.
    let room = match link.room.clone().or_else(|| inner.state.read().room_id().map(str::to_owned)) {
        Some(r) => r,
        None => match net::probe(&net, &link, None, Duration::from_secs(10)).await? {
            ServerMsg::ProbeInfo { server, .. } if rooms::valid_id(&server.room_id) => server.room_id,
            ServerMsg::Error { message, .. } => return Ok(End::Refused(message)),
            other => bail!("{} didn't answer like a Den room: {other:?}", link.short()),
        },
    };
    // Our key for this room, and the invite: from the link, else our copy.
    let member = rooms::member_key(&inner.paths, &room)?;
    let invite = match link.invite.clone().or_else(|| rooms::load(&inner.paths, &room).ok().and_then(|s| s.invite)) {
        Some(i) => Some((i.clone(), net::invite_key(&i)?)),
        None => None,
    };
    let me_addr = net.link().await.to_string();
    let me_reach = net.reach();
    let conn = net.connect(link.addr.clone(), Duration::from_secs(10)).await?;
    let (mut wr, mut rd) = conn.open_bi().await.context("opening a stream")?;
    let nick = inner.state.read().nick.clone();
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nick,
        client: inner.client_name.clone(),
        codecs: codecs(),
        features: vec![FEATURE_HOST.into()],
        room: Some(room.clone()),
        proof: net::prove(&conn, &room, Some(&member), invite.as_ref().map(|(_, k)| k))?,
        addr: Some(me_addr.clone()),
        reach: me_reach,
    };
    write_frame(&mut wr, &hello).await?;
    let first: ServerMsg = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut rd))
        .await
        .context("handshake timed out")??
        .context("server closed during handshake")?;
    match first {
        ServerMsg::Welcome { .. } => {}
        ServerMsg::Error { code, message } => {
            tracing::info!("{} refused us ({code}): {message}", link.short());
            // Nobody hosts the room there (any more): keep looking.
            if code == "no_room" {
                return Ok(End::Retry(message));
            }
            return Ok(End::Refused(message));
        }
        ServerMsg::Handoff { to, term } => return Ok(End::Moved { to, term }),
        other => bail!("unexpected handshake reply: {other:?}"),
    }
    // What to remember for getting back in: the room's address, and its invite.
    let shown = link.without_invite().room(room).to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
    if !inner.on_welcome(epoch, &shown, conn.clone(), invite.map(|(i, _)| i), first, tx.clone()) {
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
    // This task is aborted when we leave; the helpers must go with it, or
    // the socket stays open and the host keeps us as a ghost member.
    let _writer = AbortOnDrop(writer.abort_handle());
    // Tell the host when our network changes (new address, NAT measured), so
    // it can check how reachable we are again.
    let (reach_tx, reach_net) = (tx.clone(), net.clone());
    let reporter = tokio::spawn(async move {
        let mut last = (me_reach, me_addr);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = (reach_net.reach(), reach_net.link().await.to_string());
            if now != last {
                if reach_tx.send(ClientMsg::Reach { reach: now.0, addr: Some(now.1.clone()) }).is_err() {
                    break;
                }
                last = now;
            }
        }
    });
    let _reporter = AbortOnDrop(reporter.abort_handle());
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
    let _pinger = AbortOnDrop(pinger.abort_handle());
    loop {
        match tokio::time::timeout(Duration::from_secs(45), read_frame::<_, ServerMsg>(&mut rd)).await {
            Err(_) => break Ok(End::Retry("server stopped responding".into())),
            Ok(Err(e)) => break Ok(End::Retry(format!("{e:#}"))),
            Ok(Ok(None)) => break Ok(End::Retry("server closed the connection".into())),
            Ok(Ok(Some(ServerMsg::Bye { reason }))) => {
                break Ok(if reason.starts_with("kicked") { End::Final(reason) } else { End::Retry(reason) });
            }
            Ok(Ok(Some(ServerMsg::Handoff { to, term }))) => break Ok(End::Moved { to, term }),
            Ok(Ok(Some(msg))) => inner.apply(epoch, msg),
        }
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Report failed handoffs, and once the room has moved off our server, stop it
/// (after a moment, so everyone gets the handoff) and stop showing "hosting".
async fn watch_hosted(inner: Weak<Inner>, handle: ServerHandle) {
    let mut rx = handle.handoff_state();
    let watch_failures = async {
        while rx.changed().await.is_ok() {
            let state = rx.borrow_and_update().clone();
            if let (HandoffState::Failed { error }, Some(i)) = (state, inner.upgrade()) {
                i.notice(NoticeLevel::Warn, format!("handoff failed: {error}"));
            }
        }
    };
    let moved = tokio::select! {
        m = handle.moved() => m,
        _ = watch_failures => None,
    };
    if moved.is_none() {
        return;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    handle.shutdown();
    let Some(inner) = inner.upgrade() else { return };
    let ours = {
        let mut hosted = inner.hosted.lock();
        let ours = hosted.as_ref().is_some_and(|h| h.same(&handle));
        if ours {
            hosted.take();
        }
        ours
    };
    if ours {
        inner.update(|s| s.hosting = None);
        inner.emit(names::HOSTING_CHANGED, json!({"hosting": null}));
    }
}

fn codecs() -> Vec<Codec> {
    #[cfg(feature = "voice")]
    {
        denchat_audio::supported_codecs()
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
        author: "Den".into(),
        author_key: None,
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

    /// Start a fresh connection task for `addrs`.
    fn connect(self: &Arc<Self>, addrs: Vec<String>) {
        self.stop_connection();
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let task = self.rt.spawn(conn_task(self.clone(), epoch, addrs)).abort_handle();
        self.conn.lock().task = Some(task);
    }

    fn host_failed(&self, e: anyhow::Error) {
        let error = format!("{e:#}");
        tracing::warn!("could not host: {error}");
        self.update(|s| s.conn = ConnState::Failed { addr: "host".into(), error: error.clone(), refused: false });
        self.notice(NoticeLevel::Error, format!("could not host: {error}"));
    }

    /// Our endpoint, started on first use with this machine's device key.
    async fn net(&self) -> anyhow::Result<Net> {
        let net = self
            .net
            .get_or_try_init(|| async {
                let key = net::load_or_create_key(&self.paths.data.join("device.key"))?;
                let cfg = self.config.read().clone();
                Net::bind(key, &cfg.bind, cfg.port, false, &cfg.net).await
            })
            .await?
            .clone();
        if self.state.read().net.is_none() {
            let view = net_view(&net.ready_addr().await, net.relays_on(), net.direct_enabled(), net.reach());
            self.update(|s| s.net = Some(view));
        }
        Ok(net)
    }

    fn refresh_rooms(&self) {
        let list = rooms::list(&self.paths);
        self.update(|s| s.rooms = list);
    }

    /// The room moved to `to` at `term`: remember it, and return where to connect.
    fn moved(&self, to: Vec<String>, term: u64) -> Vec<String> {
        let (room_id, local) = {
            let s = self.state.read();
            let id = s.room_id().map(str::to_owned);
            let local = s.hosting.as_ref().filter(|h| h.term == term && Some(h.room_id.as_str()) == id.as_deref()).map(|h| h.addr.clone());
            (id, local)
        };
        if let Some(id) = &room_id
            && local.is_none()
            && let Ok(mut save) = rooms::load(&self.paths, id)
        {
            save.term = save.term.max(term);
            save.addrs = to.clone();
            save.updated = crate::now_ms();
            let _ = rooms::save(&self.paths, &save);
        }
        let first = to.first().cloned().unwrap_or_default();
        self.emit(names::ROOM_MOVED, json!({"to": to, "term": term}));
        match local {
            Some(addr) => vec![addr],
            None => {
                self.notice(NoticeLevel::Info, format!("the room moved to {first}"));
                to
            }
        }
    }

    /// Look for the live host of the room we were in (by room id): where the
    /// save says it was last, and at every member's app we know of.
    async fn locate(&self) -> Option<String> {
        let id = self.state.read().room_id()?.to_owned();
        let places = rooms::load(&self.paths, &id).map(|s| s.places()).unwrap_or_default();
        let net = self.net().await.ok()?;
        tracing::info!("looking for room {id} in {} places", places.len());
        let found = rooms::find_live(&net, &id, &places).await;
        match &found {
            Some(h) => tracing::info!("room {id} is live at {} (term {})", h.addr, h.server.term),
            None => tracing::info!("room {id} isn't live anywhere we looked"),
        }
        found.map(|h| h.addr)
    }

    /// Save what the host told us about the room and open our copy of it.
    fn remember_room(&self, welcome: Welcome<'_>, addr: &str, invite: Option<String>) {
        let Welcome { server, channels, access, members, profiles } = welcome;
        if !rooms::valid_id(&server.room_id) {
            *self.copy.lock() = None;
            return;
        }
        let mut save = rooms::load(&self.paths, &server.room_id).unwrap_or_else(|_| RoomSave::empty(&server.room_id));
        save.update_from(server, channels, Some(access));
        if invite.is_some() {
            save.invite = invite;
        }
        save.merge_profiles(profiles);
        for m in members {
            save.note_member(m);
        }
        // Our own server already recorded its share addresses.
        let ours = self.state.read().hosting.as_ref().is_some_and(|h| h.room_id == server.room_id && h.term == server.term);
        if !ours {
            save.addrs = vec![addr.to_owned()];
        }
        if let Err(e) = rooms::save(&self.paths, &save) {
            tracing::warn!("saving room: {e:#}");
        }
        let mut copy = self.copy.lock();
        if copy.as_ref().is_none_or(|c| c.id != save.id) {
            *copy = rooms::Copy::open(&self.paths, &save).map_err(|e| tracing::warn!("opening room copy: {e:#}")).ok();
        }
        drop(copy);
        self.refresh_rooms();
    }

    /// Keep messages in our copy. When our own server is the one we're in,
    /// it writes the files, so only remember them.
    fn keep(&self, msgs: &[ChatMessage]) {
        let write = !self.state.read().serving_this_room();
        if let Some(c) = self.copy.lock().as_mut() {
            for m in msgs {
                c.add(m, write);
            }
        }
    }

    /// The host asked us to take the room over.
    fn on_host_request(self: &Arc<Self>, term: u64) {
        let me = self.clone();
        self.rt.spawn(async move {
            let res: anyhow::Result<HostInfo> = async {
                let (id, current) = {
                    let s = me.state.read();
                    (s.room_id().map(str::to_owned).context("not in a room")?, s.server.as_ref().map(|x| x.term).unwrap_or(0))
                };
                let old = me.hosted.lock().clone();
                if let Some(old) = old {
                    if old.room_id() == id && old.term() == current {
                        bail!("already hosting");
                    }
                    // A server we handed on earlier and haven't stopped yet.
                    me.hosted.lock().take();
                    old.shutdown();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let save = rooms::load(&me.paths, &id).context("no saved copy of this room")?;
                me.start_hosting(save, term).await
            }
            .await;
            match res {
                Ok(info) => {
                    me.notice(NoticeLevel::Info, format!("you're hosting {} now", info.name));
                    me.send(ClientMsg::HostReady { term, addrs: vec![info.addr] });
                }
                Err(e) => {
                    me.notice(NoticeLevel::Warn, format!("couldn't take over hosting: {e:#}"));
                    me.send(ClientMsg::HostFailed { term, error: format!("{e:#}") });
                }
            }
        });
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
            Some(c) => Some(denchat_proto::clean_channel_id(c)),
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

    /// A status line that repeats (reconnect attempts): if the last line in
    /// the channel is the previous one (starts with `prefix`), update it in
    /// place instead of adding another, and toast only the first time.
    fn status_notice(&self, level: NoticeLevel, text: String, prefix: &str) {
        let replaced = self.update(|s| {
            let ch = s.focus.clone().or_else(|| s.first_text())?;
            let list = s.messages.entry(ch.clone()).or_default();
            match list.last_mut() {
                Some(m) if m.id == 0 && m.kind == MessageKind::System && m.text.starts_with(prefix) => {
                    m.text = text.clone();
                    m.ts = crate::now_ms();
                    Some(true)
                }
                _ => {
                    push_capped(list, local_line(&ch, text.clone()));
                    Some(false)
                }
            }
        });
        if replaced != Some(true) && (replaced.is_none() || level != NoticeLevel::Info) {
            self.ui.toast(level, text.clone());
            self.changed();
        }
        self.emit(names::NOTICE, json!({"level": level, "text": text}));
    }

    /// Merge messages from the host (or our own copy) into what's on screen.
    fn show_history(&self, channel: ChannelId, messages: Vec<ChatMessage>) {
        let count = messages.len();
        self.update(|s| {
            let list = s.messages.entry(channel.clone()).or_default();
            let known: std::collections::HashSet<u64> = list.iter().map(|m| m.id).filter(|id| *id != 0).collect();
            let mut merged: Vec<ChatMessage> = messages.into_iter().filter(|m| !known.contains(&m.id)).collect();
            merged.append(list);
            merged.sort_by_key(|m| m.ts);
            let excess = merged.len().saturating_sub(MAX_LOCAL_MESSAGES);
            merged.drain(..excess);
            *list = merged;
        });
        self.emit(names::HISTORY_LOADED, json!({"channel": channel, "count": count}));
    }

    fn stop_connection(&self) {
        let mut slot = self.conn.lock();
        if let Some(t) = slot.task.take() {
            t.abort();
        }
        slot.outbox = None;
        slot.link = None;
        slot.voice = None;
        drop(slot);
        self.stop_engine();
        self.stop_forwarding();
    }

    /// Stop sending joins for our room on to its host (we left, or lost it).
    fn stop_forwarding(&self) {
        if let (Some(net), Some(id)) = (self.net.get(), self.state.read().room_id()) {
            net.forward(id, None);
        }
    }

    /// After a connection ends: forget live state but keep messages on screen.
    fn teardown(&self, epoch: u64) {
        if self.stale(epoch) {
            return;
        }
        {
            let mut slot = self.conn.lock();
            slot.outbox = None;
            slot.link = None;
            slot.voice = None;
        }
        self.stop_engine();
        self.stop_forwarding();
        self.update(|s| {
            s.members.clear();
            s.me = None;
            s.voice.channel = None;
            s.voice.joined_at = None;
            s.latency_ms = None;
            s.via = None;
        });
    }

    fn on_welcome(
        self: &Arc<Self>,
        epoch: u64,
        addr: &str,
        link: Connection,
        invite: Option<String>,
        msg: ServerMsg,
        tx: mpsc::UnboundedSender<ClientMsg>,
    ) -> bool {
        let ServerMsg::Welcome { server, you, voice, channels, members, access, profiles, .. } = msg else {
            return false;
        };
        {
            let mut slot = self.conn.lock();
            if self.stale(epoch) {
                return false;
            }
            slot.outbox = Some(tx);
            slot.link = Some(link);
            slot.voice = Some(voice);
        }
        // Joins that reach us for this room (an invite from when we hosted
        // it) go on to where it's hosted now, unless that's us.
        if let Some(net) = self.net.get()
            && rooms::valid_id(&server.room_id)
            && Link::parse(addr).is_ok_and(|l| l.id() != net.id())
        {
            net.forward(&server.room_id, Some((addr.to_owned(), server.term)));
        }
        let motd = server.motd.clone();
        let room = server.name.clone();
        self.remember_room(
            Welcome { server: &server, channels: &channels, access: &access, members: &members, profiles: &profiles },
            addr,
            invite,
        );
        let prev_me = self.prev_me.lock().replace(you);
        self.update(|s| {
            // Same room (reconnect, or the room moved host): keep what's on
            // screen. Member ids are per host, so only ours carries over.
            let same_room = !server.room_id.is_empty() && s.room_id() == Some(server.room_id.as_str());
            if same_room {
                for m in s.messages.values_mut().flatten() {
                    m.author_id = match m.author_id {
                        Some(a) if Some(a) == prev_me => Some(you),
                        _ => None,
                    };
                }
            } else {
                s.messages.clear();
                s.clear_unread();
            }
            s.conn = ConnState::Connected { addr: addr.to_owned() };
            s.server = Some(server);
            s.me = Some(you);
            s.members = members.into_iter().map(|m| (m.id, m)).collect();
            if let Some(me) = s.members.get(&you) {
                s.nick = me.nick.clone();
            }
            s.channels = channels;
            if s.focus.as_ref().is_none_or(|f| s.channel(f).is_none()) {
                s.focus = s.first_text();
            }
        });
        // Swap message ids with the host: it sends what we lack and asks for what it lacks.
        let syncs: Vec<ClientMsg> = {
            let copy = self.copy.lock();
            let s = self.state.read();
            s.text_channels()
                .map(|c| ClientMsg::Sync { channel: c.id.clone(), have: copy.as_ref().map(|k| k.ids(&c.id)).unwrap_or_default() })
                .collect()
        };
        for m in syncs {
            self.send(m);
        }
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

    fn apply(self: &Arc<Self>, epoch: u64, msg: ServerMsg) {
        if self.stale(epoch) {
            return;
        }
        match msg {
            ServerMsg::Message { message } => {
                self.keep(std::slice::from_ref(&message));
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
            ServerMsg::History { channel, messages, .. } | ServerMsg::Backfill { channel, messages } => {
                self.keep(&messages);
                self.show_history(channel, messages);
            }
            ServerMsg::Want { channel, ids } => {
                let found = self.copy.lock().as_ref().map(|c| c.get(&channel, &ids)).unwrap_or_default();
                for chunk in found.chunks(MAX_BATCH) {
                    self.send(ClientMsg::Upload { channel: channel.clone(), messages: chunk.to_vec() });
                }
                // The host lacked these, so it didn't send them to us either;
                // it backfills everyone else. Show them here too.
                if !found.is_empty() {
                    self.show_history(channel, found);
                }
            }
            ServerMsg::HostRequest { term } => self.on_host_request(term),
            ServerMsg::MemberJoined { member } => {
                self.note_member(&member);
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
                self.note_member(&member);
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
            ServerMsg::ChannelsUpdated { channels } => {
                let created: Vec<Channel> = self.update(|s| {
                    let new = channels.iter().filter(|c| s.channel(&c.id).is_none()).cloned().collect();
                    s.channels = channels.clone();
                    new
                });
                // Every member keeps the channel list, so whoever hosts next serves it.
                if let Some(id) = self.state.read().room_id().map(str::to_owned)
                    && let Ok(mut save) = rooms::load(&self.paths, &id)
                {
                    save.channels = channels.clone();
                    save.updated = crate::now_ms();
                    if let Err(e) = rooms::save(&self.paths, &save) {
                        tracing::warn!("saving room: {e:#}");
                    }
                }
                // Ours (same slug, maybe with a `-2` the host added): go there.
                let wanted = self.creating.lock().take_if(|(_, at)| at.elapsed() < Duration::from_secs(10)).map(|(slug, _)| slug);
                if let Some(slug) = wanted {
                    let mine =
                        created.iter().find(|c| c.kind == ChannelKind::Text && (c.id == slug || c.id.starts_with(&format!("{slug}-"))));
                    match mine {
                        Some(c) => {
                            Session { inner: self.clone() }.focus_channel(&c.id);
                        }
                        None => *self.creating.lock() = Some((slug, Instant::now())),
                    }
                }
                self.emit(names::CHANNELS_UPDATED, json!({"channels": channels, "created": created}));
            }
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
                    let via = self.conn.lock().link.as_ref().and_then(net::Via::of);
                    self.update(|s| {
                        s.latency_ms = Some(ms);
                        s.via = via;
                    });
                }
            }
            ServerMsg::Welcome { .. } | ServerMsg::ProbeInfo { .. } | ServerMsg::Bye { .. } | ServerMsg::Handoff { .. } => {}
        }
    }

    /// Remember a member in our copy of the room (profile, where their app is).
    fn note_member(&self, m: &Member) {
        let Some(id) = self.state.read().room_id().map(str::to_owned) else { return };
        if let Ok(mut save) = rooms::load(&self.paths, &id) {
            let before = (save.profiles.clone(), save.peers.clone());
            save.note_member(m);
            if (save.profiles.clone(), save.peers.clone()) != before {
                let _ = rooms::save(&self.paths, &save);
            }
        }
    }

    /// Run `save` in this process at `term`, and keep the save up to date.
    async fn start_hosting(self: &Arc<Self>, mut save: RoomSave, term: u64) -> anyhow::Result<HostInfo> {
        let net = self.net().await?;
        // Limits come from server.toml; name, channels and rules from the room.
        let base: ServerConfig = config::load_or_default(&self.paths.server_config_file()).unwrap_or_default();
        let config = save.server_config(base);
        let handle = server::start(ServerOptions {
            config,
            mode: ServerMode::Host,
            net,
            history_dir: Some(rooms::history_dir(&self.paths, &save.id)),
            room_id: save.id.clone(),
            term,
            invite: save.invite.clone(),
            profiles: save.profiles.clone(),
        })
        .await?;
        let link = handle.link().await;
        let share = vec![link.to_string()];
        let addr = link.without_invite().to_string();
        save.term = term;
        save.addrs = vec![addr.clone()];
        save.updated = crate::now_ms();
        if let Err(e) = rooms::save(&self.paths, &save) {
            tracing::warn!("saving room: {e:#}");
        }
        if let Some(old) = self.hosted.lock().replace(handle.clone()) {
            old.shutdown();
        }
        let info = HostInfo { name: save.name.clone(), room_id: save.id.clone(), term, share, addr };
        self.emit(names::HOSTING_CHANGED, json!({"hosting": info}));
        let saved = rooms::list(&self.paths);
        self.update(|s| {
            s.hosting = Some(info.clone());
            s.rooms = saved;
        });
        self.rt.spawn(watch_hosted(Arc::downgrade(self), handle));
        Ok(info)
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
        let (link, setup) = {
            let slot = self.conn.lock();
            match (slot.link.clone(), slot.voice.clone()) {
                (Some(l), Some(v)) => (l, v),
                _ => return,
            }
        };
        let settings = crate::voice::settings(&self.config.read().voice);
        let (muted, deafened) = {
            let s = self.state.read();
            (s.voice.muted, s.voice.deafened)
        };
        let me = self.clone();
        let rt = self.rt.clone();
        let test = self.end_test();
        self.rt.spawn_blocking(move || {
            // Release the devices a mic test held before the call opens them.
            drop(test);
            match crate::voice::VoiceLink::start(link, setup.token, settings, &rt) {
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

    /// Run `f` on every open engine: the call's and the mic test's.
    #[cfg(feature = "voice")]
    fn each_engine(&self, f: impl Fn(&denchat_audio::VoiceEngine)) {
        if let Some(l) = self.voice.lock().as_ref() {
            f(l.engine());
        }
        if let Some(t) = self.mic_test.lock().as_ref() {
            f(t.engine());
        }
    }

    /// Take the mic test out (the caller drops it off async workers: dropping
    /// joins audio threads) and cancel one that is still starting.
    #[cfg(feature = "voice")]
    fn end_test(&self) -> Option<denchat_audio::MicTest> {
        self.test_epoch.fetch_add(1, Ordering::SeqCst);
        let test = self.mic_test.lock().take();
        if self.state.read().voice.testing {
            self.update(|s| s.voice.testing = false);
        }
        test
    }

    fn voice_test(self: &Arc<Self>, on: bool) {
        #[cfg(feature = "voice")]
        {
            if !on {
                if let Some(t) = self.end_test() {
                    std::thread::spawn(move || drop(t));
                }
                return;
            }
            let in_call = self.voice.lock().is_some() || self.voice_wanted.lock().is_some();
            if in_call || self.mic_test.lock().is_some() || !self.config.read().modules.voice {
                return;
            }
            let epoch = self.test_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let settings = crate::voice::settings(&self.config.read().voice);
            let (ptt, loopback) = {
                let s = self.state.read();
                (s.voice.ptt, s.voice.loopback)
            };
            self.update(|s| s.voice.testing = true);
            let me = self.clone();
            self.rt.spawn_blocking(move || match denchat_audio::MicTest::start(settings) {
                Ok(test) => {
                    test.engine().set_ptt(ptt);
                    test.set_loopback(loopback);
                    let err = test.engine().stats().last_error;
                    let mut slot = me.mic_test.lock();
                    if me.test_epoch.load(Ordering::SeqCst) != epoch {
                        // Stopped (or a call started) while the devices opened.
                        drop(slot);
                        drop(test);
                        return;
                    }
                    *slot = Some(test);
                    drop(slot);
                    me.update(|s| s.voice.error = err);
                }
                Err(e) => {
                    if me.test_epoch.load(Ordering::SeqCst) == epoch {
                        let error = format!("{e:#}");
                        me.update(|s| {
                            s.voice.testing = false;
                            s.voice.error = Some(error);
                        });
                    }
                }
            });
        }
        #[cfg(not(feature = "voice"))]
        let _ = on;
    }

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
            let mine = denchat_audio::supported_codecs();
            link.engine().set_tx_codec(denchat_proto::voice::negotiate(&mine, listeners));
        }
    }

    fn voice_stats(&self) -> Option<VoiceStatsView> {
        #[cfg(feature = "voice")]
        {
            use crate::loudness::intensity;
            let (st, test) = match self.voice.lock().as_ref() {
                Some(l) => (l.engine().stats(), false),
                None => (self.mic_test.lock().as_ref()?.engine().stats(), true),
            };
            Some(VoiceStatsView {
                input_level: st.input_level,
                input_intensity: intensity(st.input_level),
                transmitting: st.transmitting,
                test,
                tx_codec: st.tx_codec,
                input_device: st.input_device,
                output_device: st.output_device,
                // A mic test's only "speaker" is your own echo.
                speakers: st
                    .speakers
                    .into_iter()
                    .filter(|sp| !test && sp.id != denchat_audio::LOOPBACK_SPEAKER)
                    .map(|sp| SpeakerView { id: sp.id, level: sp.level, intensity: intensity(sp.level), speaking: sp.speaking })
                    .collect(),
                error: st.last_error,
                tx_packets: st.tx_packets,
                rx_packets: st.rx_packets,
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
        env.push(("DEN_NICK".into(), st.nick.clone()));
        if let Some(server) = &st.server {
            env.push(("DEN_ROOM".into(), server.name.clone()));
        }
        if let Some(f) = &st.focus {
            env.push(("DEN_FOCUS_CHANNEL".into(), f.clone()));
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
    doc("channel.create", "{name, kind?}", "Add a channel to the room (kind: text (default) or voice)"),
    doc("nick.set", "{nick}", "Change nick"),
    doc("voice.join", "{channel?}", "Join a voice channel"),
    doc("voice.leave", "{}", "Leave voice"),
    doc("voice.mute", "{muted}", "Mute/unmute mic"),
    doc("voice.deafen", "{deafened}", "Deafen/undeafen"),
    doc("voice.stats", "{}", "Mic level, speakers, devices"),
    doc("voice.devices", "{}", "Audio input/output devices and the system defaults"),
    doc(
        "voice.settings",
        "{input_device?, output_device?, mode?, vad_threshold?, input_gain?, output_volume?, ptt_key?}",
        "Change voice settings (null or \"\" device/key = default); returns them all",
    ),
    doc("voice.test", "{on?, loopback?}", "Mic test outside a call (level in voice.stats); loopback plays you back"),
    doc("app.send", "{ns, data, channel?, to?}", "Relay a mod-defined payload to the room"),
    doc("ui.panel.set", "{panel, widgets, title?}", "Set a panel's widgets (any layout can place it)"),
    doc("ui.panel.clear", "{panel}", "Clear a panel"),
    doc("ui.panels.list", "{}", "Known panels"),
    doc("ui.notify", "{text, level?}", "Show a toast"),
    doc("ui.action", "{panel, action, value?}", "Emit a ui.action event"),
    doc("ui.theme.set", "{name}", "Switch theme (desktop app follows within a second)"),
    doc("ui.layout.set", "{name}", "Switch layout (desktop app follows within a second)"),
    doc(
        "ui.screenshot",
        "{path?, panel?, target?, scale?, renderer?}",
        "Save a PNG of the window, a panel or one widget; returns once written",
    ),
    doc("ui.raise", "{}", "Bring the app window to the front"),
    doc("ui.tree", "{panel?, all?, rects?, format?}", "Widgets on screen: @ref, role, text, panel (format=text: one line each)"),
    doc(
        "ui.click",
        "{target | x,y, button?, double?, pointer?}",
        "Click a widget (@ref, text, or role:text) or a point, like a real click",
    ),
    doc("ui.type", "{text, target?, enter?}", "Type into the focused text box (or `target`); enter=true presses Enter after"),
    doc("ui.key", "{key}", "Press a key or chord: Enter, Escape, Tab, ctrl+a, cmd+k, F2"),
    doc("ui.scroll", "{dy?, dx?, target?}", "Scroll by points (dy>0 scrolls down) over a widget or the pointer"),
    doc("ui.hover", "{target | x,y}", "Move the pointer over a widget or point (tooltips, hover states)"),
    doc("ui.wait", "{target, gone?, timeout_ms?}", "Wait until a widget matching `target` is on screen (or gone)"),
    doc("ui.window", "{size?, pos?, minimized?, maximized?, fullscreen?, focus?, visible?}", "Window state; with params, change it first"),
    doc("ui.scale", "{scale}", "UI zoom, like display scaling (1.0 = 100%)"),
    doc("ui.clipboard", "{}", "Text the app last copied (copies go to $DEN_CLIPBOARD, not the system clipboard, when set)"),
    doc("app.quit", "{}", "Close the app like its close button (hands the room on first)"),
    doc("ui.settings", "{open?}", "Open (default) or close the settings page in the desktop app"),
    doc("mods.list", "{}", "Installed mods and their status"),
    doc("mods.run", "{command, args?}", "Run a mod slash command"),
    doc("mods.reload", "{}", "Rescan mods and restart their processes"),
    doc("session.host", "{name?, open?}", "Host a new room from this app (invite-only unless open)"),
    doc("session.join", "{addr}", "Join a room by invite link or address"),
    doc("session.invite", "{}", "The invite link for the room you're in"),
    doc("session.leave", "{}", "Leave the room (hands it to another member if you host it)"),
    doc("session.handoff", "{to?}", "While hosting: hand the room to member `to` (default: the first who can host)"),
    doc("rooms.list", "{}", "Rooms saved on this machine"),
    doc("rooms.open", "{room}", "Join a saved room's live host, or host it from your copy (\"Host this room\")"),
    doc("rooms.forget", "{room}", "Delete a saved room and its history"),
    doc("net.status", "{}", "This app's network endpoint (id, relay, direct addresses)"),
    doc("rooms.discover", "{}", "Look for live hosts of saved rooms (async; see state.get discovery)"),
    doc("config.get", "{}", "Current config.toml values"),
];

/// `ui.*` calls answered by the graphical frontend (windowed or headless).
const FRONTEND_METHODS: &[&str] =
    &["ui.tree", "ui.click", "ui.type", "ui.key", "ui.scroll", "ui.hover", "ui.wait", "ui.window", "ui.screenshot", "ui.scale", "app.quit"];

/// Forward a call to the frontend and wait for its answer.
fn frontend_call(s: &Session, method: &str, mut p: Value) -> ApiFuture {
    if !p.is_object() {
        p = json!({});
    }
    if method == "ui.screenshot" && p["path"].as_str().is_none() {
        p["path"] = json!(s.paths().state.join("screenshots").join(format!("{}.png", crate::now_ms())));
    }
    if method == "app.quit" && s.ui().frontend_kind().is_none() {
        // Terminal client: close like Ctrl-C would, after answering.
        let s = s.clone();
        s.runtime().clone().spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            s.close().await;
            std::process::exit(0);
        });
        return ready(Ok(json!({"quitting": true})));
    }
    let wait_ms = match method {
        // An hour at most, as the frontend allows.
        "ui.wait" => p["timeout_ms"].as_u64().unwrap_or(10_000).min(3_600_000) + 3_000,
        _ => 20_000,
    };
    let method = method.to_owned();
    match s.ui().call_frontend(&method, p) {
        Err(e) => ready(Err(e)),
        Ok(rx) => Box::pin(async move {
            match tokio::time::timeout(Duration::from_millis(wait_ms), rx).await {
                Ok(Ok(answer)) => answer,
                Ok(Err(_)) => Err("the window closed".into()),
                Err(_) => Err(format!("{method}: the app didn't answer within {} s (is it frozen?)", wait_ms / 1000)),
            }
        }),
    }
}

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
        if (FRONTEND_METHODS.contains(&method) || method == "ui.clipboard") && !s.config().modules.agent_control {
            return ready(Err(format!("{method}: agent control is off in this app ([modules] agent_control = false in config.toml)")));
        }
        if FRONTEND_METHODS.contains(&method) {
            return frontend_call(&s, method, p);
        }
        let ch = param_str(&p, "channel");
        let res: api::ApiResult = match method {
            "state.get" => {
                let st = s.state();
                let mut v = serde_json::to_value(&*st).unwrap_or_default();
                v["members"] = serde_json::to_value(st.members.values().collect::<Vec<_>>()).unwrap_or_default();
                v["mods"] = json!(s.mods().active().map(|m| m.manifest.id.clone()).collect::<Vec<_>>());
                v["panels"] = json!(s.ui().panel_ids().into_iter().map(|(id, _)| id).collect::<Vec<_>>());
                v["is_host"] = json!(st.is_host());
                v["frontend"] = json!(s.ui().frontend_kind());
                v["agent_control"] = json!(s.config().modules.agent_control);
                Ok(v)
            }
            "messages.list" => {
                let st = s.state();
                let channel = ch.or_else(|| st.focus.clone()).unwrap_or_default();
                let limit = p["limit"].as_u64().unwrap_or(50) as usize;
                let all = st.messages(&channel);
                ok(&all[all.len().saturating_sub(limit)..])
            }
            "message.send" => require_str(&p, "text").and_then(|t| s.try_send_message(ch.as_deref(), &t)).map(|()| json!({"sent": true})),
            "input.submit" => require_str(&p, "text").map(|t| {
                s.submit(ch.as_deref(), &t);
                json!({"submitted": true})
            }),
            "channel.create" => require_str(&p, "name").and_then(|n| {
                let kind = match p["kind"].as_str().unwrap_or("text") {
                    "text" => ChannelKind::Text,
                    "voice" => ChannelKind::Voice,
                    other => return Err(format!("unknown kind {other} (text or voice)")),
                };
                if denchat_proto::channel_slug(&n).is_empty() {
                    return Err("channel names need a letter or a digit".into());
                }
                s.create_channel(&n, kind);
                Ok(json!({"requested": true, "id": denchat_proto::channel_slug(&n)}))
            }),
            "channel.focus" => require_str(&p, "channel").and_then(|c| {
                if s.focus_channel(&c) { Ok(json!({"focus": s.state().focus})) } else { Err(format!("no text channel #{c}")) }
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
            "voice.devices" => s.voice_devices().map_err(|e| format!("{e:#}")).and_then(ok),
            "voice.settings" => voice_settings(&s, &p),
            "voice.test" => {
                if let Some(l) = p["loopback"].as_bool() {
                    s.set_voice_loopback(l);
                }
                s.voice_test(p["on"].as_bool().unwrap_or(true));
                ok(&s.state().voice)
            }
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
            "ui.clipboard" => ok(json!({"copied": s.ui().copied()})),
            "ui.raise" => {
                s.ui().request_raise();
                s.inner.changed();
                ok(json!({"queued": true}))
            }
            "ui.settings" => {
                let open = p["open"].as_bool().unwrap_or(true);
                s.ui().request_settings(open);
                s.inner.changed();
                Ok(json!({"open": open}))
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
                let nick = s.state().nick.clone();
                s.host(HostOptions {
                    name: param_str(&p, "name").unwrap_or_else(|| format!("{nick}'s room")),
                    open: p["open"].as_bool().unwrap_or(false),
                });
                Ok(json!({"starting": true}))
            }
            "session.invite" => match s.invite_link() {
                Some(link) => Ok(json!({"invite": link})),
                None => Err("not in a room".into()),
            },
            "session.handoff" => {
                s.handoff(p["to"].as_u64().map(|v| v as MemberId));
                Ok(json!({"requested": true}))
            }
            "rooms.list" => ok(rooms::list(s.paths())),
            "rooms.open" => require_str(&p, "room").map(|r| {
                s.open_room(&r);
                json!({"opening": true})
            }),
            "rooms.forget" => {
                require_str(&p, "room").and_then(|r| s.forget_room(&r).map(|_| json!({"forgotten": true})).map_err(|e| format!("{e:#}")))
            }
            "session.join" => require_str(&p, "addr").map(|a| {
                s.join(a);
                json!({"connecting": true})
            }),
            "session.leave" => {
                s.leave();
                Ok(json!({"left": true}))
            }
            "net.status" => ok(&s.state().net),
            "rooms.discover" => {
                s.scan();
                Ok(json!({"scanning": true}))
            }
            "config.get" => ok(s.config()),
            other => Err(format!("unknown method {other} (see api.methods)")),
        };
        ready(res)
    }
}

/// `voice.settings`: apply the fields present, return the voice config.
fn voice_settings(s: &Session, p: &Value) -> api::ApiResult {
    let name = |k: &str| p[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
    let num = |k: &str| -> Result<Option<f32>, String> {
        match p.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_f64().map(|f| Some(f as f32)).ok_or_else(|| format!("{k} must be a number")),
        }
    };
    let (threshold, gain, volume) = (num("vad_threshold")?, num("input_gain")?, num("output_volume")?);
    if p.get("input_device").is_some() || p.get("output_device").is_some() {
        let cur = s.config().voice;
        let input = if p.get("input_device").is_some() { name("input_device") } else { cur.input_device };
        let output = if p.get("output_device").is_some() { name("output_device") } else { cur.output_device };
        s.set_voice_devices(input, output);
    }
    if let Some(m) = p["mode"].as_str() {
        if !matches!(m, "voice_activity" | "vad" | "push_to_talk" | "ptt" | "open" | "always") {
            return Err(format!("unknown mode {m} (voice_activity, push_to_talk or open)"));
        }
        s.set_voice_mode(m);
    }
    if let Some(t) = threshold {
        s.set_vad_threshold(t);
    }
    if let Some(g) = gain {
        s.set_input_gain(g);
    }
    if let Some(v) = volume {
        s.set_output_volume(v);
    }
    if p.get("ptt_key").is_some() {
        s.set_ptt_key(name("ptt_key"));
    }
    ok(s.config().voice)
}

/// What a `welcome` says about the room, for saving.
struct Welcome<'a> {
    server: &'a ServerInfo,
    channels: &'a [Channel],
    access: &'a Access,
    members: &'a [Member],
    profiles: &'a [denchat_proto::Profile],
}

fn net_view(addr: &net::EndpointAddr, relays_on: bool, direct_enabled: bool, reach: Reach) -> NetView {
    NetView {
        id: addr.id.to_z32(),
        short: addr.id.fmt_short().to_string(),
        relay: addr.relay_urls().next().map(crate::relay::without_token),
        direct: addr.ip_addrs().map(|a| a.to_string()).collect(),
        relays_on,
        direct_enabled,
        reach,
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
