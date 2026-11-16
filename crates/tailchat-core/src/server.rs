//! The room server.
//!
//! One implementation serves both modes:
//!
//! * **Host (peer-to-peer)** — someone clicks *Host* in the app; the server
//!   runs inside their process and they join it like everyone else.
//! * **Headless** — `tailchat serve` on an always-on box.
//!
//! The server owns membership, channels and history, relays chat and mod
//! `app` messages, and forwards voice datagrams between members of the same
//! voice channel without decoding them (an SFU, not a mixer).
//!
//! It listens through the process's [`Net`] endpoint (which routes
//! connections to it by room id), checks each member's key proof and the
//! room's invite, and keeps the room's profiles (key → nick).
//!
//! A room isn't tied to one server: it has an id and a term, members sync
//! history with it on join, and it can hand everyone over to another host
//! ([`ServerHandle::hand_off`], or a `claim` from a host with a higher term).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use bytes::Bytes;
use iroh::endpoint::Connection;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tailchat_proto::voice::{self, Packet};
use tailchat_proto::{
    Access, Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, FEATURE_HOST, Identity, MAX_BATCH, MAX_MESSAGE_LEN, Member, MemberId,
    MessageKind, PROTOCOL_VERSION, Profile, Proof, Reach, ServerInfo, ServerMsg, VoiceSetup, clean_nick, encode_frame,
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::config::ServerConfig;
use crate::net::{self, Incoming, Link, Net, PublicKey, Registration, read_frame, write_frame};
use crate::rooms;

/// History sent to a member on join, per text channel.
const JOIN_HISTORY: usize = 100;
const OUTBOX_FRAMES: usize = 1024;
/// Most profiles a room keeps (oldest unused ones go first).
const MAX_PROFILES: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    Host,
    Headless,
}

impl ServerMode {
    fn as_str(self) -> &'static str {
        match self {
            ServerMode::Host => "host",
            ServerMode::Headless => "server",
        }
    }
}

pub struct ServerOptions {
    pub config: ServerConfig,
    pub mode: ServerMode,
    /// The endpoint to serve on (shared with everything else this process does).
    pub net: Net,
    /// Load and persist history as JSON lines here (a room save's `history/`).
    pub history_dir: Option<PathBuf>,
    /// Stable room id (see [`rooms`]).
    pub room_id: String,
    /// This host's term: one more than the room's previous host.
    pub term: u64,
    /// The room's invite key (see [`net::new_invite`]). With one, only people
    /// holding it can join; without, anyone who can reach the host.
    pub invite: Option<String>,
    /// Members the room already knows (from the save).
    pub profiles: Vec<Profile>,
}

/// Progress of handing the room to another host.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HandoffState {
    None,
    /// Waiting for `member` to start hosting at `term`.
    Asked {
        member: MemberId,
        term: u64,
    },
    /// Everyone was sent to `to`; this server is done.
    Moved {
        to: Vec<String>,
        term: u64,
    },
    Failed {
        error: String,
    },
}

/// What happened on the server, for consoles, mods and the control API.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerEvent {
    MemberJoined {
        member: Member,
    },
    MemberLeft {
        member: Member,
        reason: String,
    },
    MemberUpdated {
        member: Member,
    },
    Message {
        message: ChatMessage,
    },
    App {
        from: MemberId,
        ns: String,
        channel: Option<ChannelId>,
        data: Value,
    },
    /// The room moved to another host.
    Moved {
        to: Vec<String>,
        term: u64,
    },
}

struct Conn {
    member: Member,
    token: u64,
    /// The member's connection: voice datagrams go out on it.
    link: Connection,
    tx: mpsc::Sender<Arc<[u8]>>,
    kick: Option<oneshot::Sender<String>>,
    sent: VecDeque<Instant>,
    /// Reach checks so far (the newest one wins), and when the latest one
    /// starts (one every 2 s at most).
    reach_seq: u64,
    reach_at: Instant,
}

struct Room {
    members: HashMap<MemberId, Conn>,
    tokens: HashMap<u64, MemberId>,
    channels: Vec<Channel>,
    /// Sorted by id.
    history: HashMap<ChannelId, VecDeque<ChatMessage>>,
    next_member: MemberId,
    last_id: u64,
    /// Member asked to take over, and at which term.
    pending: Option<(MemberId, u64)>,
    /// Everyone the room knows, by key.
    profiles: HashMap<String, Profile>,
}

impl Room {
    fn channel(&self, id: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    fn members(&self) -> Vec<Member> {
        let mut v: Vec<Member> = self.members.values().map(|c| c.member.clone()).collect();
        v.sort_by_key(|m| m.id);
        v
    }

    /// `wanted`, or `wanted (2)`… if someone here uses it or another key owns it.
    fn unique_nick(&self, wanted: &str, except: Option<MemberId>, key: Option<&str>) -> String {
        let taken = |n: &str| {
            self.members.values().any(|c| Some(c.member.id) != except && c.member.nick.eq_ignore_ascii_case(n))
                || self.profiles.values().any(|p| Some(p.key.as_str()) != key && p.nick.eq_ignore_ascii_case(n))
        };
        if !taken(wanted) {
            return wanted.to_owned();
        }
        (2..).map(|i| format!("{wanted} ({i})")).find(|n| !taken(n)).unwrap_or_else(|| wanted.to_owned())
    }

    /// Queue a frame to members. Members whose outbox is full get disconnected.
    fn broadcast(&mut self, frame: &Arc<[u8]>, except: Option<MemberId>) {
        for conn in self.members.values_mut() {
            if Some(conn.member.id) == except {
                continue;
            }
            send_to(conn, frame);
        }
    }

    /// Time-ordered ids that stay unique when the room changes host:
    /// `(unix ms << 10) | 10 random bits`, never below the last id. Stays
    /// under 2^53, so JSON tools that use doubles keep it exact.
    fn next_id(&mut self) -> u64 {
        let stamp = ((crate::now_ms().max(0) as u64) << 10) | (rand::random::<u64>() & 0x3ff);
        self.last_id = stamp.max(self.last_id + 1);
        self.last_id
    }

    /// Merge messages from a member's copy. Returns the ones that were new
    /// and survived trimming to `limit`.
    fn merge_history(&mut self, channel: &str, msgs: Vec<ChatMessage>, limit: usize) -> Vec<ChatMessage> {
        let h = self.history.entry(channel.to_owned()).or_default();
        let mut known: HashSet<u64> = h.iter().map(|m| m.id).collect();
        let mut added: Vec<ChatMessage> = msgs.into_iter().filter(|m| known.insert(m.id)).collect();
        if added.is_empty() {
            return added;
        }
        h.extend(added.iter().cloned());
        h.make_contiguous().sort_by_key(|m| m.id);
        while h.len() > limit.max(1) {
            h.pop_front();
        }
        let oldest = h.front().map(|m| m.id).unwrap_or(0);
        added.retain(|m| m.id >= oldest);
        added.sort_by_key(|m| m.id);
        self.last_id = self.last_id.max(added.last().map(|m| m.id).unwrap_or(0));
        added
    }

    /// Remember that `key` goes by `nick`.
    fn remember(&mut self, key: Option<&str>, nick: &str) {
        let Some(key) = key else { return };
        let since = self.profiles.get(key).map(|p| p.since).unwrap_or_else(crate::now_ms);
        self.profiles.insert(key.to_owned(), Profile { key: key.to_owned(), nick: nick.to_owned(), since });
        if self.profiles.len() > MAX_PROFILES {
            let online: HashSet<String> = self.members.values().filter_map(|c| c.member.identity.key.clone()).collect();
            if let Some(oldest) =
                self.profiles.values().filter(|p| !online.contains(&p.key)).min_by_key(|p| p.since).map(|p| p.key.clone())
            {
                self.profiles.remove(&oldest);
            }
        }
    }

    fn profile_list(&self) -> Vec<Profile> {
        let mut v: Vec<Profile> = self.profiles.values().cloned().collect();
        v.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.key.cmp(&b.key)));
        v
    }

    fn push_history(&mut self, msg: ChatMessage, limit: usize) {
        let h = self.history.entry(msg.channel.clone()).or_default();
        h.push_back(msg);
        while h.len() > limit.max(1) {
            h.pop_front();
        }
    }

    fn history_page(&self, channel: &str, before: Option<u64>, limit: usize) -> (Vec<ChatMessage>, bool) {
        let Some(h) = self.history.get(channel) else {
            return (Vec::new(), true);
        };
        let older: Vec<&ChatMessage> = h.iter().filter(|m| before.is_none_or(|b| m.id < b)).collect();
        let start = older.len().saturating_sub(limit);
        let page = older[start..].iter().map(|m| (*m).clone()).collect();
        (page, start == 0)
    }
}

fn send_to(conn: &mut Conn, frame: &Arc<[u8]>) {
    if let Err(mpsc::error::TrySendError::Full(_)) = conn.tx.try_send(frame.clone())
        && let Some(k) = conn.kick.take()
    {
        let _ = k.send("too slow to keep up".into());
    }
}

fn frame(msg: &ServerMsg) -> Arc<[u8]> {
    encode_frame(msg).expect("server messages always encode").into()
}

struct Shared {
    room: Mutex<Room>,
    info: Mutex<ServerInfo>,
    cfg: ServerConfig,
    events: broadcast::Sender<ServerEvent>,
    net: Net,
    /// The room's invite key, and its public half (what joiners must sign with).
    invite: Option<(String, PublicKey)>,
    /// Routes this room's connections here; dropped on shutdown.
    route: Mutex<Option<Registration>>,
    persist: Option<mpsc::UnboundedSender<ChatMessage>>,
    shutdown: watch::Sender<bool>,
    handoff: watch::Sender<HandoffState>,
}

/// Cheap, cloneable handle to a running server.
#[derive(Clone)]
pub struct ServerHandle {
    shared: Arc<Shared>,
}

pub async fn start(opts: ServerOptions) -> anyhow::Result<ServerHandle> {
    let invite = match &opts.invite {
        Some(s) => Some((s.clone(), net::invite_key(s).context("the room's invite key")?.public())),
        None => None,
    };

    let channels = opts.config.channel_list();
    if channels.is_empty() {
        bail!("server needs at least one channel");
    }

    let (history, last_id, persist) = match &opts.history_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating history dir {}", dir.display()))?;
            let (h, max_id) = rooms::load_history(dir, &channels, opts.config.history_limit)?;
            (h, max_id, Some(rooms::spawn_persister(dir.clone())))
        }
        None => (HashMap::new(), 0, None),
    };

    let info = ServerInfo {
        name: opts.config.name.clone(),
        motd: opts.config.motd.clone(),
        version: crate::VERSION.into(),
        mode: opts.mode.as_str().into(),
        room_id: opts.room_id,
        term: opts.term,
    };
    let (events, _) = broadcast::channel(1024);
    let (shutdown, _) = watch::channel(false);
    let (handoff, _) = watch::channel(HandoffState::None);
    let profiles = opts.profiles.into_iter().filter(|p| !p.key.is_empty()).map(|p| (p.key.clone(), p)).collect();
    let room =
        Room { members: HashMap::new(), tokens: HashMap::new(), channels, history, next_member: 1, last_id, pending: None, profiles };
    let room_id = info.room_id.clone();
    let shared = Arc::new(Shared {
        room: Mutex::new(room),
        info: Mutex::new(info),
        cfg: opts.config,
        events,
        net: opts.net.clone(),
        invite,
        route: Mutex::new(None),
        persist,
        shutdown,
        handoff,
    });

    let (tx, rx) = mpsc::channel(64);
    *shared.route.lock() = Some(opts.net.register(&room_id, tx));
    tokio::spawn(accept_loop(shared.clone(), rx));
    tracing::info!(
        "room {:?} ({room_id}) hosted on endpoint {}{}",
        shared.cfg.name,
        opts.net.id().fmt_short(),
        if shared.invite.is_some() { ", invite only" } else { "" }
    );
    Ok(ServerHandle { shared })
}

async fn accept_loop(shared: Arc<Shared>, mut rx: mpsc::Receiver<Incoming>) {
    let mut stop = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            inc = rx.recv() => match inc {
                Some(inc) => {
                    tokio::spawn(handle_conn(shared.clone(), inc));
                }
                None => break,
            },
            _ = stop.changed() => break,
        }
    }
}

fn reject(code: &str, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error { code: code.into(), message: message.into() }
}

/// Answer a connection's first frame and close it.
async fn answer(inc: Incoming, reply: &ServerMsg) {
    let Incoming { conn, mut send, .. } = inc;
    let _ = write_frame(&mut send, reply).await;
    net::finish(&conn, send).await;
}

async fn handle_conn(shared: Arc<Shared>, inc: Incoming) {
    let peer = inc.conn.remote_id().fmt_short().to_string();
    let first = inc.first.clone();
    let (nick, client, codecs, features, proof, addr, claimed) = match first {
        ClientMsg::Probe { .. } => {
            let (members, channels) = {
                let room = shared.room.lock();
                (room.members.len() as u32, room.channels.clone())
            };
            let info = shared.info.lock().clone();
            answer(inc, &ServerMsg::ProbeInfo { server: info, members, channels }).await;
            return;
        }
        ClientMsg::Claim { room, term, addrs, proof } => {
            let reply = shared.claim(&inc.conn, &peer, &room, term, addrs, &proof);
            answer(inc, &reply).await;
            return;
        }
        ClientMsg::Hello { version, nick, client, codecs, features, proof, addr, reach, .. } => {
            if version != PROTOCOL_VERSION {
                let msg = format!("protocol {version} not supported (server speaks {PROTOCOL_VERSION})");
                answer(inc, &reject("version", msg)).await;
                return;
            }
            // Late arrivals after a handoff are sent on to the new host.
            let moved = shared.handoff.borrow().clone();
            if let HandoffState::Moved { to, term } = moved {
                answer(inc, &ServerMsg::Handoff { to, term }).await;
                return;
            }
            (nick, client, codecs, features, proof, addr, reach)
        }
        _ => {
            answer(inc, &reject("handshake", "expected hello")).await;
            return;
        }
    };

    let (identity, invited) = shared.identify(&inc.conn, &proof);
    if let Some(refusal) = shared.refusal(&identity, invited) {
        if let ServerMsg::Error { code, .. } = &refusal {
            tracing::info!("refused {nick:?} from {peer} ({}): {code}", describe_identity(&identity));
        }
        answer(inc, &refusal).await;
        return;
    }
    let Incoming { conn: link, send: mut wr, recv: mut rd, .. } = inc;
    let addr = addr.filter(|a| a.len() <= 1024 && Link::parse(a).is_ok());

    let (tx, mut rx) = mpsc::channel::<Arc<[u8]>>(OUTBOX_FRAMES);
    let (kick_tx, mut kick_rx) = oneshot::channel::<String>();

    let registered = {
        let mut room = shared.room.lock();
        if room.members.len() >= shared.cfg.max_members {
            None
        } else {
            let id = room.next_member;
            room.next_member += 1;
            let token = loop {
                let t: u64 = rand::random();
                if t != 0 && !room.tokens.contains_key(&t) {
                    break t;
                }
            };
            let wanted = clean_nick(&nick);
            let nick = room.unique_nick(&wanted, None, identity.key.as_deref());
            room.remember(identity.key.as_deref(), &nick);
            let member = Member {
                id,
                nick,
                identity,
                voice: None,
                muted: false,
                deafened: false,
                client: client.chars().take(64).collect(),
                codecs,
                can_host: features.iter().any(|f| f == FEATURE_HOST),
                addr,
                // Only the host's own check can make someone `open`.
                reach: claimed.min(Reach::Public),
            };
            let mut conn = Conn {
                member: member.clone(),
                token,
                link: link.clone(),
                tx,
                kick: Some(kick_tx),
                sent: VecDeque::new(),
                reach_seq: 0,
                reach_at: Instant::now(),
            };
            // Welcome and history go into the outbox before anything else can.
            let welcome = ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                server: shared.info.lock().clone(),
                you: id,
                voice: VoiceSetup { udp_port: 0, token, frame_ms: voice::FRAME_MS },
                channels: room.channels.clone(),
                members: {
                    let mut m = room.members();
                    m.push(member.clone());
                    m
                },
                access: shared.access(),
                profiles: room.profile_list(),
            };
            send_to(&mut conn, &frame(&welcome));
            let text_channels: Vec<ChannelId> =
                room.channels.iter().filter(|c| c.kind == ChannelKind::Text).map(|c| c.id.clone()).collect();
            for ch in text_channels {
                let (messages, complete) = room.history_page(&ch, None, JOIN_HISTORY);
                send_to(&mut conn, &frame(&ServerMsg::History { channel: ch, messages, complete }));
            }
            room.broadcast(&frame(&ServerMsg::MemberJoined { member: member.clone() }), None);
            room.tokens.insert(token, id);
            room.members.insert(id, conn);
            Some(member)
        }
    };
    let Some(member) = registered else {
        tracing::info!("refused {nick:?} from {peer}: the room is full");
        let _ = write_frame(&mut wr, &reject("full", "room is full")).await;
        net::finish(&link, wr).await;
        return;
    };
    let id = member.id;
    tracing::info!("{} joined from {peer} ({})", member.nick, describe_identity(&member.identity));
    if shared.checks_reach(member.addr.as_deref()) {
        tokio::spawn(check_reach(shared.clone(), id, 0, Duration::ZERO, member.addr.clone(), member.reach));
    }
    let _ = shared.events.send(ServerEvent::MemberJoined { member });

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if wr.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = wr.finish();
    });
    let voice = tokio::spawn(voice_loop(shared.clone(), id, link.clone()));

    let mut stop = shared.shutdown.subscribe();
    let reason: String = loop {
        tokio::select! {
            res = read_frame::<_, ClientMsg>(&mut rd) => match res {
                Ok(Some(msg)) => shared.handle(id, msg),
                Ok(None) => break "left".into(),
                // The app closed the connection on purpose (quit, leave): a normal goodbye.
                Err(_) if matches!(link.close_reason(), Some(iroh::endpoint::ConnectionError::ApplicationClosed(_))) => break "left".into(),
                Err(e) => break format!("connection error: {e}"),
            },
            r = &mut kick_rx => break r.map(|r| format!("kicked: {r}")).unwrap_or_else(|_| "kicked".into()),
            _ = stop.changed() => break "server shutting down".into(),
        }
    };

    let removed = {
        let mut room = shared.room.lock();
        let conn = room.members.remove(&id);
        if room.pending.is_some_and(|(asked, _)| asked == id) {
            room.pending = None;
            shared.handoff.send_replace(HandoffState::Failed { error: "the member asked to host left".into() });
        }
        if let Some(c) = &conn {
            room.tokens.remove(&c.token);
            if reason != "left" && !reason.starts_with("connection error") {
                let _ = c.tx.try_send(frame(&ServerMsg::Bye { reason: reason.clone() }));
            }
            room.broadcast(&frame(&ServerMsg::MemberLeft { id }), None);
        }
        conn.map(|c| c.member)
    };
    // Dropping the Conn closed the outbox; let the writer flush (Bye) and
    // finish, give the member a moment to read it, then close.
    voice.abort();
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), link.closed()).await;
    link.close(0u32.into(), b"bye");
    if let Some(member) = removed {
        tracing::info!("{} left ({reason})", member.nick);
        let _ = shared.events.send(ServerEvent::MemberLeft { member, reason });
    }
}

/// Connect to a member's app directly from a fresh endpoint, and put the
/// answer in the member list: `open` if it worked, else what the member
/// claimed, at most `nat`. A newer report (`seq`) makes this one moot.
async fn check_reach(shared: Arc<Shared>, id: MemberId, seq: u64, delay: Duration, addr: Option<String>, claimed: Reach) {
    let Some(link) = addr.as_deref().and_then(|a| Link::parse(a).ok()) else { return };
    let current = || shared.room.lock().members.get(&id).is_some_and(|c| c.reach_seq == seq);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if !current() {
        return;
    }
    // Public addresses only (a LAN address would only prove we share a LAN),
    // except for a host that only listens on loopback (tests).
    let loopback = shared.net.bound().iter().all(|a| a.ip().is_loopback());
    let targets: Vec<std::net::SocketAddr> =
        link.addr.ip_addrs().filter(|a| crate::relay::is_public(a.ip()) || (loopback && a.ip().is_loopback())).copied().collect();
    let reach = if net::reachable(link.addr.id, targets, Duration::from_secs(4)).await { Reach::Open } else { claimed.min(Reach::Nat) };
    let mut room = shared.room.lock();
    if room.members.get(&id).is_none_or(|c| c.reach_seq != seq) {
        return;
    }
    tracing::debug!("reach check of member {id}: {}", reach.as_str());
    shared.update_member(&mut room, id, |m| m.reach = reach);
}

fn describe_identity(id: &Identity) -> String {
    match id.short() {
        Some(k) if id.verified => format!("key {k}"),
        _ => "no key".into(),
    }
}

impl Shared {
    fn access(&self) -> Access {
        Access { allow: self.cfg.allow.clone() }
    }

    /// Check a key proof made on `conn`: who it is, and whether they hold the invite.
    fn identify(&self, conn: &Connection, proof: &Proof) -> (Identity, bool) {
        let room = self.info.lock().room_id.clone();
        let v = net::verify(conn, &room, proof, self.invite.as_ref().map(|(_, pk)| pk));
        (Identity { verified: v.key.is_some(), key: v.key }, v.invited)
    }

    /// The room's join rules: why `identity` may not come in, if it may not.
    /// Every way in (joining, claiming) goes through here.
    fn refusal(&self, identity: &Identity, invited: bool) -> Option<ServerMsg> {
        if self.invite.is_some() && !invited {
            return Some(reject("not_invited", "this room needs an invite: ask a member for a link"));
        }
        if !self.access().allows(identity.key.as_deref()) {
            return Some(reject("not_allowed", "you are not on this room's allow list"));
        }
        None
    }

    /// Another peer hosts this room at a higher term and wants everyone.
    fn claim(&self, conn: &Connection, peer: &str, room: &str, term: u64, addrs: Vec<String>, proof: &Proof) -> ServerMsg {
        let (identity, invited) = self.identify(conn, proof);
        if let Some(refusal) = self.refusal(&identity, invited) {
            tracing::info!("refused a claim from {peer} ({})", describe_identity(&identity));
            return refusal;
        }
        let (id, current) = {
            let info = self.info.lock();
            (info.room_id.clone(), info.term)
        };
        if room != id {
            return reject("wrong_room", "this server hosts a different room");
        }
        if term <= current {
            tracing::info!("refused a claim from {peer} at term {term}: this room is at term {current}");
            return reject("stale_term", format!("this room is already at term {current}"));
        }
        if addrs.is_empty() || addrs.len() > 8 {
            return reject("bad_claim", "a claim needs 1-8 addresses");
        }
        tracing::info!("room claimed at term {term} by {peer} ({})", describe_identity(&identity));
        self.finish_handoff(addrs.clone(), term);
        ServerMsg::Handoff { to: addrs, term }
    }

    /// Send everyone to the new host. This server keeps running until its
    /// owner shuts it down, answering late joiners with the same handoff.
    fn finish_handoff(&self, to: Vec<String>, term: u64) {
        {
            let mut room = self.room.lock();
            room.pending = None;
            room.broadcast(&frame(&ServerMsg::Handoff { to: to.clone(), term }), None);
        }
        self.handoff.send_replace(HandoffState::Moved { to: to.clone(), term });
        let _ = self.events.send(ServerEvent::Moved { to, term });
    }

    fn reply(&self, room: &mut Room, id: MemberId, msg: &ServerMsg) {
        if let Some(c) = room.members.get_mut(&id) {
            send_to(c, &frame(msg));
        }
    }

    fn handle(self: &Arc<Self>, id: MemberId, msg: ClientMsg) {
        let mut room = self.room.lock();
        match msg {
            ClientMsg::Hello { .. } | ClientMsg::Probe { .. } => {
                self.reply(&mut room, id, &reject("handshake", "already joined"));
            }
            ClientMsg::Ping { nonce } => self.reply(&mut room, id, &ServerMsg::Pong { nonce }),
            ClientMsg::Send { channel, text } => {
                let text = text.trim_end().to_owned();
                if text.trim().is_empty() {
                    return;
                }
                if text.len() > MAX_MESSAGE_LEN {
                    self.reply(&mut room, id, &reject("too_long", format!("messages are limited to {MAX_MESSAGE_LEN} bytes")));
                    return;
                }
                match room.channel(&channel) {
                    Some(c) if c.kind == ChannelKind::Text => {}
                    _ => {
                        self.reply(&mut room, id, &reject("no_channel", format!("no text channel #{channel}")));
                        return;
                    }
                }
                let limit = self.cfg.rate_limit.max(1) as usize;
                let Some(conn) = room.members.get_mut(&id) else { return };
                let now = Instant::now();
                while conn.sent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(10)) {
                    conn.sent.pop_front();
                }
                if conn.sent.len() >= limit {
                    let msg = ServerMsg::Notice { level: tailchat_proto::NoticeLevel::Warn, text: "slow down — message dropped".into() };
                    send_to(conn, &frame(&msg));
                    return;
                }
                conn.sent.push_back(now);
                let author = conn.member.nick.clone();
                let author_key = conn.member.identity.key.clone();
                let message = ChatMessage {
                    id: room.next_id(),
                    channel,
                    ts: crate::now_ms(),
                    author_id: Some(id),
                    author,
                    author_key,
                    text,
                    kind: MessageKind::User,
                };
                self.publish(&mut room, message);
            }
            ClientMsg::History { channel, before, limit } => {
                let (messages, complete) = room.history_page(&channel, before, limit.clamp(1, 200) as usize);
                self.reply(&mut room, id, &ServerMsg::History { channel, messages, complete });
            }
            ClientMsg::SetNick { nick } => {
                let wanted = clean_nick(&nick);
                let key = room.members.get(&id).and_then(|c| c.member.identity.key.clone());
                let nick = room.unique_nick(&wanted, Some(id), key.as_deref());
                room.remember(key.as_deref(), &nick);
                self.update_member(&mut room, id, |m| m.nick = nick);
            }
            ClientMsg::VoiceJoin { channel } => match room.channel(&channel) {
                Some(c) if c.kind == ChannelKind::Voice => {
                    self.update_member(&mut room, id, |m| m.voice = Some(channel));
                }
                _ => self.reply(&mut room, id, &reject("no_channel", format!("no voice channel #{channel}"))),
            },
            ClientMsg::VoiceLeave => self.update_member(&mut room, id, |m| m.voice = None),
            ClientMsg::VoiceState { muted, deafened } => self.update_member(&mut room, id, |m| {
                m.muted = muted;
                m.deafened = deafened;
            }),
            ClientMsg::App { ns, channel, to, data } => {
                if ns.is_empty() || ns.len() > 64 {
                    self.reply(&mut room, id, &reject("bad_ns", "app namespace must be 1-64 chars"));
                    return;
                }
                let out = frame(&ServerMsg::App { from: id, ns: ns.clone(), channel: channel.clone(), data: data.clone() });
                match to {
                    Some(target) => {
                        if let Some(c) = room.members.get_mut(&target) {
                            send_to(c, &out);
                        }
                    }
                    None => room.broadcast(&out, Some(id)),
                }
                drop(room);
                let _ = self.events.send(ServerEvent::App { from: id, ns, channel, data });
            }
            ClientMsg::Sync { channel, have } => {
                if !room.channel(&channel).is_some_and(|c| c.kind == ChannelKind::Text) {
                    return;
                }
                let limit = self.cfg.history_limit.max(1);
                let have: HashSet<u64> = have.into_iter().take(limit * 4).collect();
                let (missing, want) = {
                    let empty = VecDeque::new();
                    let h = room.history.get(&channel).unwrap_or(&empty);
                    let missing: Vec<ChatMessage> = h.iter().filter(|m| !have.contains(&m.id)).cloned().collect();
                    let mine: HashSet<u64> = h.iter().map(|m| m.id).collect();
                    // When full, only ask for what would survive trimming.
                    let floor = if h.len() >= limit { h.front().map(|m| m.id).unwrap_or(0) } else { 0 };
                    let mut want: Vec<u64> = have.iter().copied().filter(|i| *i > floor && !mine.contains(i)).collect();
                    want.sort_unstable();
                    let skip = want.len().saturating_sub(limit);
                    (missing, want.split_off(skip))
                };
                for chunk in missing.chunks(MAX_BATCH) {
                    self.reply(&mut room, id, &ServerMsg::Backfill { channel: channel.clone(), messages: chunk.to_vec() });
                }
                if !want.is_empty() {
                    self.reply(&mut room, id, &ServerMsg::Want { channel, ids: want });
                }
            }
            ClientMsg::Upload { channel, messages } => {
                if !room.channel(&channel).is_some_and(|c| c.kind == ChannelKind::Text) {
                    return;
                }
                let msgs: Vec<ChatMessage> = messages
                    .into_iter()
                    .take(MAX_BATCH)
                    .filter(|m| m.id != 0 && m.channel == channel && m.text.len() <= MAX_MESSAGE_LEN && m.author.len() <= 64)
                    .map(|mut m| {
                        // Member ids belong to the host that assigned them.
                        m.author_id = None;
                        m
                    })
                    .collect();
                let added = room.merge_history(&channel, msgs, self.cfg.history_limit);
                if added.is_empty() {
                    return;
                }
                if let Some(p) = &self.persist {
                    for m in &added {
                        let _ = p.send(m.clone());
                    }
                }
                room.broadcast(&frame(&ServerMsg::Backfill { channel, messages: added }), Some(id));
            }
            ClientMsg::HostReady { term, addrs } => {
                let ours = room.pending == Some((id, term));
                drop(room);
                if ours && !addrs.is_empty() {
                    self.finish_handoff(addrs.into_iter().take(8).collect(), term);
                }
            }
            ClientMsg::HostFailed { term, error } => {
                if room.pending == Some((id, term)) {
                    room.pending = None;
                    let nick = room.members.get(&id).map(|c| c.member.nick.clone()).unwrap_or_default();
                    drop(room);
                    self.handoff.send_replace(HandoffState::Failed { error: format!("{nick} couldn't host: {error}") });
                }
            }
            ClientMsg::Reach { reach, addr } => {
                let Some(conn) = room.members.get_mut(&id) else { return };
                let addr = addr.filter(|a| a.len() <= 1024 && Link::parse(a).is_ok()).or_else(|| conn.member.addr.clone());
                let claimed = reach.min(Reach::Public);
                let checkable = self.checks_reach(addr.as_deref());
                // One check every 2 s at most: a report that comes sooner is
                // checked when its turn comes (unless a newer one beats it).
                let now = Instant::now();
                let start = (conn.reach_at + Duration::from_secs(2)).max(now);
                if checkable {
                    conn.reach_seq += 1;
                    conn.reach_at = start;
                }
                let seq = conn.reach_seq;
                // Our own app can't be checked: its claim stands. Others keep
                // what they have until the check answers.
                let shown = if checkable { conn.member.reach } else { claimed };
                self.update_member(&mut room, id, |m| {
                    m.addr = addr.clone();
                    m.reach = shown;
                });
                drop(room);
                if checkable {
                    tokio::spawn(check_reach(self.clone(), id, seq, start - now, addr, claimed));
                }
            }
            ClientMsg::Claim { .. } => self.reply(&mut room, id, &reject("handshake", "claim must be the first frame")),
        }
    }

    /// Whether we can check how reachable the app at `addr` is: it has an
    /// address, and it isn't our own app (hosting from it).
    fn checks_reach(&self, addr: Option<&str>) -> bool {
        addr.and_then(|a| Link::parse(a).ok()).is_some_and(|l| l.addr.id != self.net.id())
    }

    fn update_member(&self, room: &mut Room, id: MemberId, f: impl FnOnce(&mut Member)) {
        let Some(conn) = room.members.get_mut(&id) else { return };
        let before = conn.member.clone();
        f(&mut conn.member);
        if conn.member == before {
            return;
        }
        let member = conn.member.clone();
        room.broadcast(&frame(&ServerMsg::MemberUpdated { member: member.clone() }), None);
        let _ = self.events.send(ServerEvent::MemberUpdated { member });
    }

    /// Store, persist and fan out a message.
    fn publish(&self, room: &mut Room, message: ChatMessage) {
        room.push_history(message.clone(), self.cfg.history_limit);
        room.broadcast(&frame(&ServerMsg::Message { message: message.clone() }), None);
        if let Some(p) = &self.persist {
            let _ = p.send(message.clone());
        }
        let _ = self.events.send(ServerEvent::Message { message });
    }
}

/// One member's voice: datagrams on their connection. The connection says
/// who's speaking, so the packet's token isn't needed to tell.
async fn voice_loop(shared: Arc<Shared>, speaker: MemberId, link: Connection) {
    let mut out = Vec::with_capacity(1500);
    let ack = Bytes::from(Packet::Ack.to_vec());
    while let Ok(d) = link.read_datagram().await {
        match Packet::parse(&d) {
            Some(Packet::Register { .. }) => {
                let _ = link.send_datagram(ack.clone());
            }
            Some(Packet::Audio { token, seq, codec, payload }) => {
                if payload.len() > voice::MAX_PAYLOAD {
                    continue;
                }
                let targets: Vec<Connection> = {
                    let room = shared.room.lock();
                    // The connection says who speaks; the token must agree.
                    let Some(conn) = room.members.get(&speaker).filter(|c| c.token == token) else { continue };
                    let (Some(vc), false) = (conn.member.voice.clone(), conn.member.muted) else { continue };
                    voice::audio_to_relay(speaker, seq, codec, payload, &mut out);
                    room.members
                        .values()
                        .filter(|c| c.member.id != speaker && !c.member.deafened)
                        .filter(|c| c.member.voice.as_deref() == Some(vc.as_str()))
                        .map(|c| c.link.clone())
                        .collect()
                };
                let pkt = Bytes::copy_from_slice(&out);
                for t in targets {
                    let _ = t.send_datagram(pkt.clone());
                }
            }
            _ => {}
        }
    }
}

impl ServerHandle {
    pub fn net(&self) -> &Net {
        &self.shared.net
    }

    /// The room's invite key, if it has one.
    pub fn invite(&self) -> Option<String> {
        self.shared.invite.as_ref().map(|(s, _)| s.clone())
    }

    /// An invite link to this room (carries the invite key).
    pub async fn link(&self) -> Link {
        self.shared.net.link().await.room(self.room_id()).invite(self.invite())
    }

    /// Where members can reach this room (no invite key): for handoffs and saves.
    pub async fn addrs(&self) -> Vec<String> {
        vec![self.link().await.without_invite().to_string()]
    }

    /// Everyone the room knows (key → nick).
    pub fn profiles(&self) -> Vec<Profile> {
        self.shared.room.lock().profile_list()
    }

    pub fn info(&self) -> ServerInfo {
        self.shared.info.lock().clone()
    }

    pub fn room_id(&self) -> String {
        self.shared.info.lock().room_id.clone()
    }

    pub fn term(&self) -> u64 {
        self.shared.info.lock().term
    }

    /// Whether two handles are the same server.
    pub fn same(&self, other: &ServerHandle) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Ask a member to take over hosting. `to: None` picks the member whose
    /// app can host and that others reach most easily (see [`Reach`]; ties go
    /// to whoever has been here longest), other than `except` (usually the
    /// host's own session). When they're ready, everyone is
    /// sent to them; watch [`handoff_state`](Self::handoff_state).
    pub fn hand_off(&self, to: Option<MemberId>, except: Option<MemberId>) -> anyhow::Result<Member> {
        if matches!(*self.shared.handoff.borrow(), HandoffState::Moved { .. }) {
            bail!("the room has already moved");
        }
        let term = self.term() + 1;
        let mut room = self.shared.room.lock();
        let target = match to {
            Some(id) => room.members.get(&id).map(|c| c.member.clone()).with_context(|| format!("no member {id}"))?,
            None => {
                room.members()
                    .into_iter()
                    .filter(|m| Some(m.id) != except && m.can_host)
                    .max_by_key(|m| (m.reach, std::cmp::Reverse(m.id)))
                    .context("nobody else here can host the room")?
            }
        };
        if !target.can_host {
            bail!("{}'s app can't host rooms", target.nick);
        }
        room.pending = Some((target.id, term));
        self.shared.reply(&mut room, target.id, &ServerMsg::HostRequest { term });
        drop(room);
        self.shared.handoff.send_replace(HandoffState::Asked { member: target.id, term });
        tracing::info!("asked {} to host at term {term}", target.nick);
        Ok(target)
    }

    pub fn handoff_state(&self) -> watch::Receiver<HandoffState> {
        self.shared.handoff.subscribe()
    }

    /// Wait (up to `timeout`) for a handoff in progress to finish.
    pub async fn wait_handoff(&self, timeout: Duration) -> HandoffState {
        let mut rx = self.handoff_state();
        let wait = rx.wait_for(|s| matches!(s, HandoffState::Moved { .. } | HandoffState::Failed { .. } | HandoffState::None));
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(s)) => s.clone(),
            _ => HandoffState::Failed { error: "the new host didn't answer in time".into() },
        }
    }

    /// Resolves when the room moves to another host (`Some((to, term))`) or
    /// this server shuts down (`None`).
    pub async fn moved(&self) -> Option<(Vec<String>, u64)> {
        let mut handoff = self.handoff_state();
        let mut stop = self.shared.shutdown.subscribe();
        loop {
            if let HandoffState::Moved { to, term } = &*handoff.borrow_and_update() {
                return Some((to.clone(), *term));
            }
            if *stop.borrow_and_update() {
                return None;
            }
            tokio::select! {
                r = handoff.changed() => if r.is_err() { return None },
                r = stop.changed() => if r.is_err() { return None },
            }
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.shared.cfg
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.shared.events.subscribe()
    }

    pub fn members(&self) -> Vec<Member> {
        self.shared.room.lock().members()
    }

    pub fn channels(&self) -> Vec<Channel> {
        self.shared.room.lock().channels.clone()
    }

    pub fn default_text_channel(&self) -> Option<ChannelId> {
        self.shared.room.lock().channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id.clone())
    }

    pub fn history(&self, channel: &str, limit: usize) -> Vec<ChatMessage> {
        self.shared.room.lock().history_page(channel, None, limit).0
    }

    /// Post a message as the server (admin console, server-side mods).
    pub fn say(&self, channel: Option<&str>, text: &str) -> anyhow::Result<ChatMessage> {
        let text = text.trim();
        if text.is_empty() {
            bail!("empty message");
        }
        if text.len() > MAX_MESSAGE_LEN {
            bail!("message too long");
        }
        let mut room = self.shared.room.lock();
        let channel = match channel {
            Some(c) => {
                let c = tailchat_proto::clean_channel_id(c);
                match room.channel(&c) {
                    Some(ch) if ch.kind == ChannelKind::Text => c,
                    _ => bail!("no text channel #{c}"),
                }
            }
            None => {
                room.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id.clone()).context("server has no text channel")?
            }
        };
        let message = ChatMessage {
            id: room.next_id(),
            channel,
            ts: crate::now_ms(),
            author_id: None,
            author: self.shared.info.lock().name.clone(),
            author_key: None,
            text: text.to_owned(),
            kind: MessageKind::Server,
        };
        self.shared.publish(&mut room, message.clone());
        Ok(message)
    }

    /// Broadcast a mod `app` payload from the server (member id 0).
    pub fn send_app(&self, ns: &str, channel: Option<String>, data: Value) {
        let mut room = self.shared.room.lock();
        let out = frame(&ServerMsg::App { from: 0, ns: ns.into(), channel, data });
        room.broadcast(&out, None);
    }

    pub fn notice_all(&self, text: &str) {
        let mut room = self.shared.room.lock();
        let msg = ServerMsg::Notice { level: tailchat_proto::NoticeLevel::Info, text: text.into() };
        room.broadcast(&frame(&msg), None);
    }

    pub fn kick(&self, id: MemberId, reason: &str) -> bool {
        let mut room = self.shared.room.lock();
        match room.members.get_mut(&id).and_then(|c| c.kick.take()) {
            Some(k) => k.send(reason.into()).is_ok(),
            None => false,
        }
    }

    pub fn set_motd(&self, motd: Option<String>) {
        self.shared.info.lock().motd = motd;
    }

    /// Stop serving: everyone is told (`bye`) and the room is no longer
    /// routed here. The endpoint itself belongs to the caller.
    pub fn shutdown(&self) {
        self.shared.route.lock().take();
        let _ = self.shared.shutdown.send(true);
    }
}
