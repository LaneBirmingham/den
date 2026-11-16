//! The room server.
//!
//! One implementation serves both modes:
//!
//! * **Host (peer-to-peer)** — someone clicks *Host* in the app; the server
//!   runs inside their process and they join it like everyone else.
//! * **Headless** — `tailchat serve` on an always-on box; history persists.
//!
//! The server owns membership, channels and history, relays chat and mod
//! `app` messages, and forwards voice datagrams between members of the same
//! voice channel without decoding them (an SFU, not a mixer).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tailchat_proto::voice::{self, Packet};
use tailchat_proto::{
    Channel, ChannelId, ChannelKind, ChatMessage, ClientMsg, Identity, MAX_MESSAGE_LEN, Member, MemberId, MessageKind, PROTOCOL_VERSION,
    ServerInfo, ServerMsg, VoiceSetup, clean_nick, encode_frame,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::config::ServerConfig;
use crate::net::{read_frame, write_frame};
use crate::tailscale::{Tailscale, WhoIs};

/// History sent to a member on join, per text channel.
const JOIN_HISTORY: usize = 100;
const OUTBOX_FRAMES: usize = 1024;
const WHOIS_TTL: Duration = Duration::from_secs(300);

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
    pub bind: SocketAddr,
    /// Persist history as JSON lines here.
    pub history_dir: Option<PathBuf>,
    pub tailscale: Option<Tailscale>,
}

/// What happened on the server, for consoles, mods and the control API.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerEvent {
    MemberJoined { member: Member },
    MemberLeft { member: Member, reason: String },
    MemberUpdated { member: Member },
    Message { message: ChatMessage },
    App { from: MemberId, ns: String, channel: Option<ChannelId>, data: Value },
}

/// Resolve a bind spec (`tailnet`, `localhost`, `all`, or an IP) to a socket address.
pub async fn resolve_bind(spec: &str, port: u16, tailscale: Option<&Tailscale>) -> anyhow::Result<SocketAddr> {
    let ip: IpAddr = match spec.trim() {
        "" | "tailnet" | "tailscale" => {
            let ts = tailscale.context("the tailscale CLI was not found; install Tailscale or bind with `localhost`/`all`")?;
            ts.self_ipv4()
                .await
                .context("asking tailscale for this machine's address")?
                .context("Tailscale is not connected; run `tailscale up` or bind with `localhost`/`all`")?
        }
        "localhost" | "loopback" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        "all" | "any" | "0.0.0.0" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        other => other.parse().with_context(|| format!("bind must be tailnet, localhost, all or an IP, got {other:?}"))?,
    };
    Ok(SocketAddr::new(ip, port))
}

struct Conn {
    member: Member,
    ip: IpAddr,
    token: u64,
    udp: Option<SocketAddr>,
    tx: mpsc::Sender<Arc<[u8]>>,
    kick: Option<oneshot::Sender<String>>,
    sent: VecDeque<Instant>,
}

struct Room {
    members: HashMap<MemberId, Conn>,
    tokens: HashMap<u64, MemberId>,
    channels: Vec<Channel>,
    history: HashMap<ChannelId, VecDeque<ChatMessage>>,
    next_member: MemberId,
    next_msg: u64,
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

    fn unique_nick(&self, wanted: &str, except: Option<MemberId>) -> String {
        let taken = |n: &str| self.members.values().any(|c| Some(c.member.id) != except && c.member.nick.eq_ignore_ascii_case(n));
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
    udp: Arc<UdpSocket>,
    local_addr: SocketAddr,
    tailscale: Option<Tailscale>,
    persist: Option<mpsc::UnboundedSender<ChatMessage>>,
    shutdown: watch::Sender<bool>,
    whois: Mutex<HashMap<IpAddr, (Instant, Option<WhoIs>)>>,
}

/// Cheap, cloneable handle to a running server.
#[derive(Clone)]
pub struct ServerHandle {
    shared: Arc<Shared>,
}

/// TCP and UDP on the same port. With port 0 the OS picks the TCP port, and
/// that number can already be taken for UDP, so try a few.
async fn bind_tcp_udp(bind: SocketAddr) -> anyhow::Result<(TcpListener, UdpSocket)> {
    let mut tries = if bind.port() == 0 { 8 } else { 1 };
    loop {
        let listener = TcpListener::bind(bind).await.with_context(|| format!("binding TCP {bind}"))?;
        let local = listener.local_addr()?;
        match UdpSocket::bind(local).await {
            Ok(udp) => return Ok((listener, udp)),
            Err(e) if tries > 1 && e.kind() == std::io::ErrorKind::AddrInUse => tries -= 1,
            Err(e) => return Err(e).with_context(|| format!("binding UDP {local} for voice")),
        }
    }
}

pub async fn start(opts: ServerOptions) -> anyhow::Result<ServerHandle> {
    let (listener, udp) = bind_tcp_udp(opts.bind).await?;
    let local_addr = listener.local_addr()?;

    let channels: Vec<Channel> = {
        let mut seen = std::collections::HashSet::new();
        opts.config.channels.iter().map(|c| c.to_channel()).filter(|c| !c.id.is_empty() && seen.insert(c.id.clone())).collect()
    };
    if channels.is_empty() {
        bail!("server needs at least one channel");
    }

    let (history, next_msg, persist) = match &opts.history_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating history dir {}", dir.display()))?;
            let (h, max_id) = load_history(dir, &channels, opts.config.history_limit)?;
            (h, max_id + 1, Some(spawn_persister(dir.clone())))
        }
        None => (HashMap::new(), 1, None),
    };

    let info = ServerInfo {
        name: opts.config.name.clone(),
        motd: opts.config.motd.clone(),
        version: crate::VERSION.into(),
        mode: opts.mode.as_str().into(),
    };
    let (events, _) = broadcast::channel(1024);
    let (shutdown, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        room: Mutex::new(Room { members: HashMap::new(), tokens: HashMap::new(), channels, history, next_member: 1, next_msg }),
        info: Mutex::new(info),
        cfg: opts.config,
        events,
        udp: Arc::new(udp),
        local_addr,
        tailscale: opts.tailscale,
        persist,
        shutdown,
        whois: Mutex::new(HashMap::new()),
    });

    tokio::spawn(accept_loop(shared.clone(), listener));
    tokio::spawn(udp_loop(shared.clone()));
    tracing::info!("room {:?} listening on {local_addr} (tcp+udp)", shared.cfg.name);
    Ok(ServerHandle { shared })
}

async fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    let mut stop = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, peer)) => {
                    tokio::spawn(handle_conn(shared.clone(), stream, peer));
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            },
            _ = stop.changed() => break,
        }
    }
}

fn reject(code: &str, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error { code: code.into(), message: message.into() }
}

async fn handle_conn(shared: Arc<Shared>, stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();

    let first = tokio::time::timeout(Duration::from_secs(10), read_frame::<_, ClientMsg>(&mut rd)).await;
    let (nick, client, codecs) = match first {
        Ok(Ok(Some(ClientMsg::Probe))) => {
            let (members, channels) = {
                let room = shared.room.lock();
                (room.members.len() as u32, room.channels.clone())
            };
            let info = shared.info.lock().clone();
            let _ = write_frame(&mut wr, &ServerMsg::ProbeInfo { server: info, members, channels }).await;
            return;
        }
        Ok(Ok(Some(ClientMsg::Hello { version, nick, client, codecs, .. }))) => {
            if version != PROTOCOL_VERSION {
                let msg = format!("protocol {version} not supported (server speaks {PROTOCOL_VERSION})");
                let _ = write_frame(&mut wr, &reject("version", msg)).await;
                return;
            }
            (nick, client, codecs)
        }
        _ => {
            let _ = write_frame(&mut wr, &reject("handshake", "expected hello")).await;
            return;
        }
    };

    let identity = shared.identify(peer.ip()).await;
    if shared.cfg.require_tailnet && !identity.verified {
        let _ = write_frame(&mut wr, &reject("not_tailnet", "this room only admits tailnet members")).await;
        return;
    }
    if !shared.cfg.allow.is_empty() {
        let ok = identity.login.as_ref().is_some_and(|l| shared.cfg.allow.iter().any(|a| a.eq_ignore_ascii_case(l)));
        if !ok {
            let _ = write_frame(&mut wr, &reject("not_allowed", "you are not on this room's allow list")).await;
            return;
        }
    }

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
            let nick = room.unique_nick(&wanted, None);
            let member = Member {
                id,
                nick,
                identity,
                voice: None,
                muted: false,
                deafened: false,
                client: client.chars().take(64).collect(),
                codecs,
            };
            let mut conn = Conn { member: member.clone(), ip: peer.ip(), token, udp: None, tx, kick: Some(kick_tx), sent: VecDeque::new() };
            // Welcome and history go into the outbox before anything else can.
            let welcome = ServerMsg::Welcome {
                version: PROTOCOL_VERSION,
                server: shared.info.lock().clone(),
                you: id,
                voice: VoiceSetup { udp_port: shared.local_addr.port(), token, frame_ms: voice::FRAME_MS },
                channels: room.channels.clone(),
                members: {
                    let mut m = room.members();
                    m.push(member.clone());
                    m
                },
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
        let _ = write_frame(&mut wr, &reject("full", "room is full")).await;
        return;
    };
    let id = member.id;
    tracing::info!("{} joined from {peer} ({})", member.nick, describe_identity(&member.identity));
    let _ = shared.events.send(ServerEvent::MemberJoined { member });

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if wr.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let mut stop = shared.shutdown.subscribe();
    let reason: String = loop {
        tokio::select! {
            res = read_frame::<_, ClientMsg>(&mut rd) => match res {
                Ok(Some(msg)) => shared.handle(id, msg),
                Ok(None) => break "left".into(),
                Err(e) => break format!("connection error: {e}"),
            },
            r = &mut kick_rx => break r.map(|r| format!("kicked: {r}")).unwrap_or_else(|_| "kicked".into()),
            _ = stop.changed() => break "server shutting down".into(),
        }
    };

    let removed = {
        let mut room = shared.room.lock();
        let conn = room.members.remove(&id);
        if let Some(c) = &conn {
            room.tokens.remove(&c.token);
            if reason != "left" && !reason.starts_with("connection error") {
                let _ = c.tx.try_send(frame(&ServerMsg::Bye { reason: reason.clone() }));
            }
            room.broadcast(&frame(&ServerMsg::MemberLeft { id }), None);
        }
        conn.map(|c| c.member)
    };
    // Dropping the Conn closed the outbox; let the writer flush (Bye) and finish.
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    if let Some(member) = removed {
        tracing::info!("{} left ({reason})", member.nick);
        let _ = shared.events.send(ServerEvent::MemberLeft { member, reason });
    }
}

fn describe_identity(id: &Identity) -> String {
    match (&id.login, &id.node) {
        (Some(l), Some(n)) => format!("{l} on {n}"),
        (Some(l), None) => l.clone(),
        (None, Some(n)) => format!("unverified, {n}"),
        _ => "unverified".into(),
    }
}

impl Shared {
    async fn identify(&self, ip: IpAddr) -> Identity {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return Identity { node: Some("localhost".into()), ..Identity::default() };
        }
        let Some(ts) = &self.tailscale else {
            return Identity::default();
        };
        if let Some((at, cached)) = self.whois.lock().get(&ip).cloned()
            && at.elapsed() < WHOIS_TTL
        {
            return whois_identity(cached);
        }
        let who = ts.whois(ip).await.unwrap_or(None);
        self.whois.lock().insert(ip, (Instant::now(), who.clone()));
        whois_identity(who)
    }

    fn reply(&self, room: &mut Room, id: MemberId, msg: &ServerMsg) {
        if let Some(c) = room.members.get_mut(&id) {
            send_to(c, &frame(msg));
        }
    }

    fn handle(&self, id: MemberId, msg: ClientMsg) {
        let mut room = self.room.lock();
        match msg {
            ClientMsg::Hello { .. } | ClientMsg::Probe => {
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
                let author_login = conn.member.identity.login.clone();
                let message = ChatMessage {
                    id: room.next_msg,
                    channel,
                    ts: crate::now_ms(),
                    author_id: Some(id),
                    author,
                    author_login,
                    text,
                    kind: MessageKind::User,
                };
                room.next_msg += 1;
                self.publish(&mut room, message);
            }
            ClientMsg::History { channel, before, limit } => {
                let (messages, complete) = room.history_page(&channel, before, limit.clamp(1, 200) as usize);
                self.reply(&mut room, id, &ServerMsg::History { channel, messages, complete });
            }
            ClientMsg::SetNick { nick } => {
                let wanted = clean_nick(&nick);
                let nick = room.unique_nick(&wanted, Some(id));
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
        }
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

fn whois_identity(who: Option<WhoIs>) -> Identity {
    match who {
        Some(w) => Identity { verified: w.login.is_some(), login: w.login, display_name: w.display_name, node: w.node },
        None => Identity::default(),
    }
}

async fn udp_loop(shared: Arc<Shared>) {
    let mut stop = shared.shutdown.subscribe();
    let mut buf = vec![0u8; 2048];
    let mut out = Vec::with_capacity(1500);
    let ack = Packet::Ack.to_vec();
    loop {
        let (n, src) = tokio::select! {
            r = shared.udp.recv_from(&mut buf) => match r {
                Ok(v) => v,
                // Windows reports ICMP port-unreachable as a recv error; ignore and carry on.
                Err(_) => continue,
            },
            _ = stop.changed() => break,
        };
        match Packet::parse(&buf[..n]) {
            Some(Packet::Register { token }) => {
                let ok = {
                    let mut room = shared.room.lock();
                    authenticate(&mut room, token, src).is_some()
                };
                if ok {
                    let _ = shared.udp.send_to(&ack, src).await;
                }
            }
            Some(Packet::Audio { token, seq, codec, payload }) => {
                if payload.len() > voice::MAX_PAYLOAD {
                    continue;
                }
                let targets: Vec<SocketAddr> = {
                    let mut room = shared.room.lock();
                    let Some(speaker) = authenticate(&mut room, token, src) else { continue };
                    let Some(conn) = room.members.get(&speaker) else { continue };
                    let (Some(vc), false) = (conn.member.voice.clone(), conn.member.muted) else { continue };
                    voice::audio_to_relay(speaker, seq, codec, payload, &mut out);
                    room.members
                        .values()
                        .filter(|c| c.member.id != speaker && !c.member.deafened)
                        .filter(|c| c.member.voice.as_deref() == Some(vc.as_str()))
                        .filter_map(|c| c.udp)
                        .collect()
                };
                for t in targets {
                    let _ = shared.udp.send_to(&out, t).await;
                }
            }
            _ => {}
        }
    }
}

/// Match a token to a member, require the datagram to come from the member's
/// control-connection IP, and remember its UDP address.
fn authenticate(room: &mut Room, token: u64, src: SocketAddr) -> Option<MemberId> {
    let id = *room.tokens.get(&token)?;
    let conn = room.members.get_mut(&id)?;
    if conn.ip.to_canonical() != src.ip().to_canonical() {
        return None;
    }
    conn.udp = Some(src);
    Some(id)
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.shared.local_addr
    }

    pub fn info(&self) -> ServerInfo {
        self.shared.info.lock().clone()
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
            id: room.next_msg,
            channel,
            ts: crate::now_ms(),
            author_id: None,
            author: self.shared.info.lock().name.clone(),
            author_login: None,
            text: text.to_owned(),
            kind: MessageKind::Server,
        };
        room.next_msg += 1;
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

    pub fn shutdown(&self) {
        let _ = self.shared.shutdown.send(true);
    }
}

fn history_file(dir: &Path, channel: &str) -> PathBuf {
    dir.join(format!("{channel}.jsonl"))
}

fn load_history(dir: &Path, channels: &[Channel], limit: usize) -> anyhow::Result<(HashMap<ChannelId, VecDeque<ChatMessage>>, u64)> {
    let mut out = HashMap::new();
    let mut max_id = 0;
    for ch in channels.iter().filter(|c| c.kind == ChannelKind::Text) {
        let path = history_file(dir, &ch.id);
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let mut all: Vec<ChatMessage> =
            std::io::BufReader::new(file).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect();
        max_id = all.iter().map(|m| m.id).fold(max_id, u64::max);
        let total = all.len();
        let keep = all.split_off(total.saturating_sub(limit.max(1)));
        // Compact files that have grown well past what we keep.
        if total > keep.len() * 4 + 100 {
            let tmp = path.with_extension("jsonl.tmp");
            let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            for m in &keep {
                serde_json::to_writer(&mut f, m)?;
                f.write_all(b"\n")?;
            }
            f.flush()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
        }
        out.insert(ch.id.clone(), keep.into());
    }
    Ok((out, max_id))
}

fn spawn_persister(dir: PathBuf) -> mpsc::UnboundedSender<ChatMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChatMessage>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let path = history_file(&dir, &msg.channel);
            let line = match serde_json::to_string(&msg) {
                Ok(l) => l + "\n",
                Err(_) => continue,
            };
            let res = async {
                let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?;
                f.write_all(line.as_bytes()).await?;
                f.flush().await
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("persisting history to {}: {e}", path.display());
            }
        }
    });
    tx
}
