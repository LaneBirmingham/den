//! The network: one iroh endpoint per app (or `serve`), plus framing, links
//! and key proofs shared by server, client and room lookups.
//!
//! * **Endpoint** ([`Net`]) — a QUIC endpoint dialed by its public key. iroh
//!   hole-punches a direct path and falls back to a relay only when it can't.
//!   No accounts, no coordination server: connecting needs a [`Link`].
//!   One endpoint serves every room the process hosts; the first frame of a
//!   connection names the room, and [`Net`] routes it there.
//! * **Links** — `den://<endpoint id>?room=<id>&invite=<key>&relay=<url>&ip=<addr>`.
//!   An *invite* carries the room's invite key; the addresses rooms pass
//!   around (handoffs, saves) never do.
//! * **Proofs** — a member signs the connection's TLS exporter with their
//!   room key (and the invite key, if the room has one), so a proof only
//!   works on the connection it was made for. Hosts store public keys only.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, bail};
use data_encoding::{BASE32_DNSSEC, HEXLOWER};
use denchat_proto::{ALPN, ClientMsg, MAX_FRAME, Proof, Reach, ServerMsg, encode_frame};
use iroh::endpoint::{BindOpts, Connection, PortmapperConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, RelayMap, RelayMode, RelayUrl, TransportAddr, Watcher};
use parking_lot::Mutex;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

pub use iroh::{EndpointAddr, EndpointId, PublicKey, SecretKey};

use crate::config::NetConfig;

/// Read one length-prefixed JSON frame. `Ok(None)` on clean EOF.
pub async fn read_frame<R, T>(r: &mut R) -> anyhow::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("peer sent a {len} byte frame (limit {MAX_FRAME})");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).context("bad frame")?))
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = encode_frame(msg)?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

// ----- keys -----------------------------------------------------------------

/// A fresh room invite key, as it appears in invite links.
pub fn new_invite() -> String {
    BASE32_DNSSEC.encode(&SecretKey::generate().to_bytes())
}

/// Parse an invite key (from a link or a room save).
pub fn invite_key(s: &str) -> anyhow::Result<SecretKey> {
    let bytes = BASE32_DNSSEC.decode(s.trim().to_ascii_lowercase().as_bytes()).context("bad invite key")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("bad invite key length"))?;
    Ok(SecretKey::from_bytes(&arr))
}

/// Another process already uses this endpoint key: it's running with the
/// same profile. Two endpoints with one id fight over the relay (it keeps
/// only the newest) and over direct paths, so the first goes unreachable.
#[derive(Debug, thiserror::Error)]
#[error("Den is already running with this profile ({}); use that one, or set DEN_HOME to run another with its own profile", .dir.display())]
pub struct AlreadyRunning {
    pub dir: PathBuf,
}

/// Holds `<key>.lock` while it lives (the OS drops it if the process dies).
#[derive(Debug)]
pub struct KeyLock {
    _file: std::fs::File,
}

/// Claim the endpoint key at `path` for this process.
pub fn lock_key(path: &Path) -> anyhow::Result<KeyLock> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let lock = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .with_context(|| format!("opening {}", lock.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(KeyLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(AlreadyRunning { dir: dir.to_path_buf() }.into()),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::Error::new(e).context(format!("locking {}", lock.display()))),
    }
}

/// Load a secret key from `path` (hex), creating it on first use.
pub fn load_or_create_key(path: &Path) -> anyhow::Result<SecretKey> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let bytes = HEXLOWER.decode(text.trim().as_bytes()).with_context(|| format!("reading key {}", path.display()))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("{} is not a key", path.display()))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("key.tmp");
    std::fs::write(&tmp, HEXLOWER.encode(&key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing key {}", path.display()))?;
    Ok(key)
}

/// What both ends of `conn` can compute and nobody else can: the TLS
/// exporter, bound to the room. Signing it proves a key on this connection.
fn transcript(conn: &Connection, room: &str) -> anyhow::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"den/3 proof", room.as_bytes()).map_err(|_| anyhow::anyhow!("no keying material"))?;
    Ok(out)
}

fn signed(kind: &[u8], t: &[u8; 32]) -> Vec<u8> {
    [kind, b"\0", t].concat()
}

/// Prove `member` (our key for this room) and `invite` (the room's invite
/// key, if we have it) on `conn`.
pub fn prove(conn: &Connection, room: &str, member: Option<&SecretKey>, invite: Option<&SecretKey>) -> anyhow::Result<Proof> {
    let t = transcript(conn, room)?;
    Ok(Proof {
        key: member.map(|k| k.public().to_string()),
        sig: member.map(|k| HEXLOWER.encode(&k.sign(&signed(b"member", &t)).to_bytes())),
        invite: invite.map(|k| HEXLOWER.encode(&k.sign(&signed(b"invite", &t)).to_bytes())),
    })
}

/// What a [`Proof`] on `conn` shows.
#[derive(Debug, Default, PartialEq)]
pub struct Verified {
    /// The member's key, if they proved it.
    pub key: Option<String>,
    /// They signed with the room's invite key.
    pub invited: bool,
}

pub fn verify(conn: &Connection, room: &str, proof: &Proof, invite: Option<&PublicKey>) -> Verified {
    let Ok(t) = transcript(conn, room) else { return Verified::default() };
    let sig = |hex: &str| -> Option<iroh::Signature> {
        let bytes = HEXLOWER.decode(hex.as_bytes()).ok()?;
        Some(iroh::Signature::from_bytes(&bytes.try_into().ok()?))
    };
    let key = match (&proof.key, &proof.sig) {
        (Some(k), Some(s)) => PublicKey::from_str(k)
            .ok()
            .zip(sig(s))
            .filter(|(pk, s)| pk.verify(&signed(b"member", &t), s).is_ok())
            .map(|(pk, _)| pk.to_string()),
        _ => None,
    };
    let invited = match (invite, &proof.invite) {
        (Some(pk), Some(s)) => sig(s).is_some_and(|s| pk.verify(&signed(b"invite", &t), &s).is_ok()),
        _ => false,
    };
    Verified { key, invited }
}

// ----- links ----------------------------------------------------------------

/// Where a host is (its endpoint id plus hints: relay, direct addresses),
/// optionally which room and the room's invite key.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub addr: EndpointAddr,
    pub room: Option<String>,
    pub invite: Option<String>,
}

impl Link {
    pub fn new(addr: EndpointAddr) -> Link {
        Link { addr, room: None, invite: None }
    }

    pub fn room(mut self, room: impl Into<String>) -> Link {
        self.room = Some(room.into());
        self
    }

    pub fn invite(mut self, invite: Option<String>) -> Link {
        self.invite = invite;
        self
    }

    /// The same place without the invite key (for handoffs and saves).
    pub fn without_invite(&self) -> Link {
        Link { invite: None, ..self.clone() }
    }

    pub fn id(&self) -> EndpointId {
        self.addr.id
    }

    /// Short form of the endpoint id, for showing people.
    pub fn short(&self) -> String {
        self.addr.id.fmt_short().to_string()
    }

    /// Parse `den://<id>?…`, a bare id (hex or z-base-32), or either
    /// with the query. `tailchat://` (the old name) still parses, so older
    /// invite links and saved rooms keep working.
    pub fn parse(raw: &str) -> anyhow::Result<Link> {
        let s = raw.trim();
        let s = s.strip_prefix("den://").or_else(|| s.strip_prefix("tailchat://")).unwrap_or(s).trim_end_matches('/');
        let (id, query) = s.split_once('?').unwrap_or((s, ""));
        let id = id.trim_end_matches('/');
        if id.is_empty() {
            bail!("empty address");
        }
        let key = if id.len() == 52 { PublicKey::from_z32(id).ok() } else { None };
        let key = match key {
            Some(k) => k,
            None => PublicKey::from_str(id).map_err(|e| anyhow::anyhow!("{id:?} isn't a Den address or invite ({e})"))?,
        };
        let mut link = Link::new(EndpointAddr::new(key));
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = unescape(v);
            match k {
                "room" if crate::rooms::valid_id(&v) => link.room = Some(v),
                "invite" => link.invite = Some(v),
                "relay" => link.addr = link.addr.with_relay_url(RelayUrl::from_str(&v).with_context(|| format!("bad relay url {v:?}"))?),
                "ip" => link.addr = link.addr.with_ip_addr(v.parse().with_context(|| format!("bad address {v:?}"))?),
                _ => {}
            }
        }
        Ok(link)
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "den://{}", self.addr.id.to_z32())?;
        let mut sep = '?';
        let mut put = |f: &mut fmt::Formatter<'_>, k: &str, v: &str| {
            let r = write!(f, "{sep}{k}={}", escape(v));
            sep = '&';
            r
        };
        if let Some(r) = &self.room {
            put(f, "room", r)?;
        }
        if let Some(i) = &self.invite {
            put(f, "invite", i)?;
        }
        for a in &self.addr.addrs {
            match a {
                TransportAddr::Relay(u) => put(f, "relay", u.as_str())?,
                TransportAddr::Ip(ip) => put(f, "ip", &ip.to_string())?,
                _ => {}
            }
        }
        Ok(())
    }
}

fn escape(v: &str) -> String {
    v.replace('%', "%25").replace('&', "%26").replace('#', "%23").replace(' ', "%20").replace('?', "%3F").replace('=', "%3D")
}

fn unescape(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len() + 1
            && let Some(h) = v.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----- the endpoint ---------------------------------------------------------

/// A connection that named a room this endpoint hosts.
pub struct Incoming {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    /// The first frame (`hello`, `probe` or `claim`).
    pub first: ClientMsg,
}

struct Route {
    id: u64,
    room: String,
    tx: mpsc::Sender<Incoming>,
}

struct NetInner {
    ep: Endpoint,
    relays: bool,
    /// Relay URLs to tell peers instead of the endpoint's own (a server
    /// whose home relay is its own, reached locally at another URL).
    advertise: Mutex<Option<Vec<RelayUrl>>>,
    /// `ready_addr` waited once already: don't wait again.
    ready: AtomicBool,
    routes: Mutex<Vec<Route>>,
    next_route: AtomicU64,
    /// A second endpoint for joining rooms this one hosts (an endpoint can't dial itself).
    loopback: tokio::sync::OnceCell<Endpoint>,
    /// The router offers port mapping (UPnP, NAT-PMP or PCP), so the
    /// endpoint's own port mapper (on by default) has a port forwarded to it.
    router_maps: AtomicBool,
}

/// This process's endpoint. Cheap to clone.
#[derive(Clone)]
pub struct Net {
    inner: Arc<NetInner>,
}

/// Keeps a room routed to its server; dropping it unroutes the room.
pub struct Registration {
    net: Weak<NetInner>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(n) = self.net.upgrade() {
            n.routes.lock().retain(|r| r.id != self.id);
        }
    }
}

/// Every relay `cfg` names (`default` is n0's public set).
pub fn relay_map(cfg: &NetConfig) -> anyhow::Result<RelayMap> {
    let map = RelayMap::empty();
    for r in cfg.effective_relays() {
        if r == "default" {
            map.extend(&iroh::defaults::prod::default_relay_map());
        } else {
            map.extend(&RelayMap::from(crate::relay::parse_url(&r)?));
        }
    }
    Ok(map)
}

fn relay_mode(cfg: &NetConfig) -> anyhow::Result<(RelayMode, bool)> {
    let map = relay_map(cfg)?;
    if map.is_empty() { Ok((RelayMode::Disabled, false)) } else { Ok((RelayMode::Custom(map), true)) }
}

/// What an endpoint knows about its network, for [`Net::reach`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NetFacts {
    /// A network report has run (it needs relays).
    pub measured: bool,
    pub udp_v4: bool,
    pub udp_v6: bool,
    /// The NAT gives each destination its own port (hole-punching rarely works).
    pub varies_v4: bool,
    pub varies_v6: bool,
    /// The machine itself has a public address (not just its router).
    pub public_v4: bool,
    pub public_v6: bool,
    /// The router forwards a port to the endpoint (UPnP / NAT-PMP / PCP).
    pub mapped: bool,
}

impl NetFacts {
    pub fn reach(self) -> Reach {
        let v4_open = self.public_v4 || self.mapped;
        if (v4_open && (self.udp_v4 || !self.measured)) || (self.public_v6 && (self.udp_v6 || !self.measured)) {
            return Reach::Public;
        }
        if !self.measured {
            return Reach::Unknown;
        }
        let v4 = self.udp_v4 && !self.varies_v4;
        let v6 = self.udp_v6 && !self.varies_v6;
        if v4 || v6 { Reach::Nat } else { Reach::Strict }
    }
}

/// Keep asking the router (every 5 minutes) whether it maps ports. Only
/// asks; the endpoint's own port mapper makes the mapping.
async fn watch_router(inner: Weak<NetInner>) {
    let client = portmapper::Client::new(portmapper::Config::default());
    loop {
        let found = match client.probe().await {
            Ok(Ok(out)) => out.upnp || out.pcp || out.nat_pmp,
            Ok(Err(e)) => {
                tracing::debug!("port mapping probe: {e}");
                false
            }
            Err(_) => false,
        };
        let Some(i) = inner.upgrade() else { return };
        if i.ep.is_closed() {
            return;
        }
        if i.router_maps.swap(found, Ordering::Relaxed) != found {
            tracing::info!("the router {} port mapping", if found { "offers" } else { "doesn't offer" });
        }
        drop(i);
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

/// The address this machine would send from to the internet (no packet is
/// sent: connecting a UDP socket only picks a route; the targets are
/// documentation addresses).
fn primary_ip(v6: bool) -> Option<IpAddr> {
    let (bind, to) = if v6 { ("[::]:0", "[2001:db8::1]:9") } else { ("0.0.0.0:0", "192.0.2.1:9") };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(to).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// Can a fresh endpoint connect to `id` at `addrs` directly within
/// `timeout`? It has no relay, no port mapping and no history with the
/// peer, so nothing can hole-punch for it: success means anyone could.
pub async fn reachable(id: EndpointId, addrs: Vec<SocketAddr>, timeout: Duration) -> bool {
    if addrs.is_empty() {
        return false;
    }
    let ep = match Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .portmapper_config(PortmapperConfig::Disabled)
        .bind()
        .await
    {
        Ok(ep) => ep,
        Err(e) => {
            tracing::debug!("reach check: no endpoint: {e:#}");
            return false;
        }
    };
    let addr = EndpointAddr::from_parts(id, addrs.into_iter().map(TransportAddr::Ip));
    let ok = match tokio::time::timeout(timeout, ep.connect(addr, ALPN)).await {
        Ok(Ok(conn)) => {
            conn.close(0u32.into(), b"reach check");
            true
        }
        Ok(Err(e)) => {
            tracing::debug!("reach check of {}: {e:#}", id.fmt_short());
            false
        }
        Err(_) => false,
    };
    ep.close().await;
    ok
}

/// A short, human form of a link (its endpoint id), or `raw` if it isn't one.
pub fn short_addr(raw: &str) -> String {
    Link::parse(raw).map(|l| l.short()).unwrap_or_else(|_| raw.to_owned())
}

/// Resolve a bind spec (`all`, `localhost`, or an IP) to an address.
pub fn bind_ip(spec: &str) -> anyhow::Result<Option<IpAddr>> {
    Ok(match spec.trim() {
        // `tailnet` is what older configs said; it means everywhere now.
        "" | "all" | "any" | "0.0.0.0" | "tailnet" | "tailscale" => None,
        "localhost" | "loopback" => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        other => Some(other.parse().with_context(|| format!("bind must be all, localhost or an IP, got {other:?}"))?),
    })
}

/// How a connection's packets travel right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// Straight between the two machines (LAN, hole-punched, or a public address).
    Direct,
    /// Forwarded by a relay: something (often a firewall) stops a direct path.
    Relay,
}

impl Via {
    /// The path `conn` sends on now, if it has one.
    pub fn of(conn: &Connection) -> Option<Via> {
        let paths = conn.paths();
        let selected = paths.iter().find(|p| p.is_selected())?;
        Some(if selected.is_relay() { Via::Relay } else { Via::Direct })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Via::Direct => "direct",
            Via::Relay => "relay",
        }
    }
}

impl Net {
    /// Start the endpoint with identity `key`, listening on `bind` (`all`,
    /// `localhost` or an IP) and UDP `port` (0: any). Unless `strict`, a
    /// taken port falls back to a free one.
    pub async fn bind(key: SecretKey, bind: &str, port: u16, strict: bool, cfg: &NetConfig) -> anyhow::Result<Net> {
        let ip = bind_ip(bind)?;
        let (mode, relays) = relay_mode(cfg)?;
        let build = |port: u16| -> anyhow::Result<iroh::endpoint::Builder> {
            let mut b = Endpoint::builder(presets::Minimal).secret_key(key.clone()).alpns(vec![ALPN.to_vec()]).relay_mode(mode.clone());
            if !cfg.port_mapping {
                b = b.portmapper_config(PortmapperConfig::Disabled);
            }
            match ip {
                None if port == 0 => {}
                None => {
                    b = b.clear_ip_transports().bind_addr(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))?.bind_addr_with_opts(
                        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
                        BindOpts::default().set_is_required(false),
                    )?;
                }
                Some(ip) => b = b.clear_ip_transports().bind_addr(SocketAddr::new(ip, port))?,
            }
            Ok(b)
        };
        let ep = match build(port)?.bind().await {
            Ok(ep) => ep,
            Err(e) if !strict && port != 0 => {
                tracing::info!("UDP port {port} is taken ({e}); using a free one");
                build(0)?.bind().await.context("starting the network endpoint")?
            }
            Err(e) => return Err(e).with_context(|| format!("listening on UDP port {port}")),
        };
        let inner = Arc::new(NetInner {
            ep: ep.clone(),
            relays,
            advertise: Mutex::new(None),
            ready: AtomicBool::new(false),
            routes: Mutex::new(Vec::new()),
            next_route: AtomicU64::new(1),
            loopback: tokio::sync::OnceCell::new(),
            router_maps: AtomicBool::new(false),
        });
        tokio::spawn(accept_loop(Arc::downgrade(&inner), ep.clone()));
        if cfg.port_mapping && !ep.bound_sockets().iter().all(|a| a.ip().is_loopback()) {
            tokio::spawn(watch_router(Arc::downgrade(&inner)));
        }
        tracing::info!(
            "network endpoint {} up on {:?} (relays {})",
            inner.ep.id().fmt_short(),
            inner.ep.bound_sockets(),
            if relays { "on" } else { "off" }
        );
        Ok(Net { inner })
    }

    pub fn id(&self) -> EndpointId {
        self.inner.ep.id()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.ep
    }

    /// Relays are configured (otherwise direct connections only).
    pub fn relays_on(&self) -> bool {
        self.inner.relays
    }

    /// UDP sockets the endpoint listens on.
    pub fn bound(&self) -> Vec<SocketAddr> {
        self.inner.ep.bound_sockets()
    }

    /// How to reach this endpoint right now (may still be filling in).
    pub fn addr(&self) -> EndpointAddr {
        let a = self.inner.ep.addr();
        match self.inner.advertise.lock().as_ref() {
            Some(urls) => EndpointAddr::from_parts(
                a.id,
                a.addrs.into_iter().filter(|t| !matches!(t, TransportAddr::Relay(_))).chain(urls.iter().cloned().map(TransportAddr::Relay)),
            ),
            None => a,
        }
    }

    /// Tell peers to reach us through these relays (and not the endpoint's
    /// own relay URLs). Empty: no relay at all.
    pub fn advertise_relays(&self, urls: Vec<RelayUrl>) {
        *self.inner.advertise.lock() = Some(urls);
    }

    /// The relays peers are told about, if set with [`Net::advertise_relays`].
    pub fn advertised_relays(&self) -> Option<Vec<RelayUrl>> {
        self.inner.advertise.lock().clone()
    }

    /// How to reach this endpoint, waiting (the first time) up to a few
    /// seconds for the home relay (when relays are on) and direct addresses
    /// to be known.
    pub async fn ready_addr(&self) -> EndpointAddr {
        if self.inner.ready.load(Ordering::Relaxed) {
            return self.addr();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            let a = self.inner.ep.addr();
            let ready = if self.inner.relays { a.relay_urls().next().is_some() } else { a.ip_addrs().next().is_some() };
            if ready || tokio::time::Instant::now() >= deadline {
                self.inner.ready.store(true, Ordering::Relaxed);
                return self.addr();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// How reachable this endpoint looks from here (see [`Reach`]), from the
    /// endpoint's network report and this machine's addresses. Never `open`:
    /// that takes someone else connecting in.
    pub fn reach(&self) -> Reach {
        let loopback = self.bound().iter().all(|a| a.ip().is_loopback());
        let public = |v6| !loopback && primary_ip(v6).is_some_and(crate::relay::is_public);
        // The router forwards a port to us, and we know our public IPv4.
        let mapped = self.inner.router_maps.load(Ordering::Relaxed)
            && self.inner.ep.addr().ip_addrs().any(|a| a.is_ipv4() && crate::relay::is_public(a.ip()));
        let mut report = self.inner.ep.net_report();
        let facts = match report.get() {
            Some(r) => NetFacts {
                measured: true,
                udp_v4: r.udp_v4,
                udp_v6: r.udp_v6,
                varies_v4: r.mapping_varies_by_dest_ipv4 == Some(true),
                varies_v6: r.mapping_varies_by_dest_ipv6 == Some(true),
                public_v4: public(false),
                public_v6: public(true),
                mapped,
            },
            None => NetFacts { public_v4: public(false), public_v6: public(true), mapped, ..NetFacts::default() },
        };
        facts.reach()
    }

    /// A link to this endpoint (no room).
    pub async fn link(&self) -> Link {
        Link::new(self.ready_addr().await)
    }

    /// Route connections for `room` to `tx` until the registration drops.
    /// The newest registration of a room wins.
    pub fn register(&self, room: &str, tx: mpsc::Sender<Incoming>) -> Registration {
        let id = self.inner.next_route.fetch_add(1, Ordering::Relaxed);
        self.inner.routes.lock().push(Route { id, room: room.to_owned(), tx });
        Registration { net: Arc::downgrade(&self.inner), id }
    }

    /// Dial `addr` (our own endpoint included).
    pub async fn connect(&self, addr: EndpointAddr, timeout: Duration) -> anyhow::Result<Connection> {
        let who = addr.id.fmt_short().to_string();
        let fut = async {
            if addr.id == self.id() {
                let lo = self
                    .inner
                    .loopback
                    .get_or_try_init(|| async {
                        Endpoint::builder(presets::Minimal)
                            .relay_mode(RelayMode::Disabled)
                            // It's only for this machine: no router mapping.
                            .portmapper_config(PortmapperConfig::Disabled)
                            .clear_ip_transports()
                            .bind_addr("127.0.0.1:0")?
                            .bind()
                            .await
                            .context("starting the loopback endpoint")
                    })
                    .await?;
                let local: Vec<TransportAddr> = self
                    .bound()
                    .into_iter()
                    .filter(|s| s.is_ipv4())
                    .map(|s| {
                        let ip = if s.ip().is_unspecified() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { s.ip() };
                        TransportAddr::Ip(SocketAddr::new(ip, s.port()))
                    })
                    .collect();
                Ok(lo.connect(EndpointAddr::from_parts(addr.id, local), ALPN).await?)
            } else {
                Ok::<_, anyhow::Error>(self.inner.ep.connect(addr, ALPN).await?)
            }
        };
        tokio::time::timeout(timeout, fut)
            .await
            .with_context(|| format!("timed out reaching {who}"))?
            .with_context(|| format!("reaching {who}"))
    }

    /// One request, one reply: connect, send `first` (built once the
    /// connection exists, so it can carry proofs), read the answer, close.
    pub async fn ask(
        &self,
        addr: EndpointAddr,
        timeout: Duration,
        first: impl FnOnce(&Connection) -> anyhow::Result<ClientMsg>,
    ) -> anyhow::Result<ServerMsg> {
        let conn = self.connect(addr, timeout).await?;
        let talk = async {
            let (mut send, mut recv) = conn.open_bi().await?;
            write_frame(&mut send, &first(&conn)?).await?;
            read_frame::<_, ServerMsg>(&mut recv).await?.context("closed without reply")
        };
        let reply = tokio::time::timeout(timeout, talk).await.context("no reply in time");
        conn.close(0u32.into(), b"done");
        reply?
    }

    /// Stop the endpoint (closing every connection).
    pub async fn close(&self) {
        self.inner.routes.lock().clear();
        if let Some(lo) = self.inner.loopback.get() {
            lo.close().await;
        }
        self.inner.ep.close().await;
    }
}

/// Ask whoever is at `link` about `room` (or its only room).
pub async fn probe(net: &Net, link: &Link, room: Option<&str>, timeout: Duration) -> anyhow::Result<ServerMsg> {
    let room = room.map(str::to_owned).or_else(|| link.room.clone());
    net.ask(link.addr.clone(), timeout, |_| Ok(ClientMsg::Probe { room })).await
}

/// Finish a stream we're done writing and let the peer read it before the
/// connection goes away.
pub async fn finish(conn: &Connection, mut send: SendStream) {
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
}

async fn accept_loop(inner: Weak<NetInner>, ep: Endpoint) {
    while let Some(incoming) = ep.accept().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            if let Err(e) = route(inner, incoming).await {
                tracing::debug!("incoming connection: {e:#}");
            }
        });
    }
}

async fn route(inner: Weak<NetInner>, incoming: iroh::endpoint::Incoming) -> anyhow::Result<()> {
    let conn = incoming.accept()?.await?;
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await.context("no stream opened")??;
    let first: ClientMsg =
        tokio::time::timeout(Duration::from_secs(10), read_frame(&mut recv)).await.context("no hello")??.context("closed")?;
    let room = match &first {
        ClientMsg::Hello { room, .. } | ClientMsg::Probe { room } => room.clone(),
        ClientMsg::Claim { room, .. } => Some(room.clone()),
        _ => None,
    };
    let tx = inner.upgrade().and_then(|n| {
        let routes = n.routes.lock();
        let r = match &room {
            Some(id) => routes.iter().rev().find(|r| &r.room == id),
            None => routes.last(),
        };
        r.map(|r| r.tx.clone())
    });
    match tx {
        Some(tx) => {
            if let Err(mpsc::error::SendError(inc)) = tx.send(Incoming { conn, send, recv, first }).await {
                let Incoming { conn, mut send, .. } = inc;
                let _ = write_frame(&mut send, &no_room(room.as_deref())).await;
                finish(&conn, send).await;
            }
        }
        None => {
            let _ = write_frame(&mut send, &no_room(room.as_deref())).await;
            finish(&conn, send).await;
        }
    }
    Ok(())
}

fn no_room(room: Option<&str>) -> ServerMsg {
    let message = match room {
        Some(id) => format!("room {id} isn't hosted here"),
        None => "no room is hosted here".into(),
    };
    ServerMsg::Error { code: "no_room".into(), message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_from_facts() {
        let nat = NetFacts { measured: true, udp_v4: true, ..NetFacts::default() };
        assert_eq!(nat.reach(), Reach::Nat);
        assert_eq!(NetFacts { varies_v4: true, ..nat }.reach(), Reach::Strict);
        // Strict over IPv4, but IPv6 works without a NAT in the way.
        assert_eq!(NetFacts { varies_v4: true, udp_v6: true, ..nat }.reach(), Reach::Nat);
        assert_eq!(NetFacts { public_v4: true, ..nat }.reach(), Reach::Public);
        // A strict NAT, but the router forwards a port to us.
        assert_eq!(NetFacts { varies_v4: true, mapped: true, ..nat }.reach(), Reach::Public);
        assert_eq!(NetFacts { measured: true, ..NetFacts::default() }.reach(), Reach::Strict);
        assert_eq!(NetFacts::default().reach(), Reach::Unknown);
        // No report (no relays), but the machine has a public address.
        assert_eq!(NetFacts { public_v4: true, ..NetFacts::default() }.reach(), Reach::Public);
        assert!(Reach::Open > Reach::Public && Reach::Public > Reach::Nat && Reach::Nat > Reach::Unknown && Reach::Unknown > Reach::Strict);
    }

    #[test]
    fn links_roundtrip() {
        let key = SecretKey::generate().public();
        let addr = EndpointAddr::new(key)
            .with_relay_url(RelayUrl::from_str("https://relay.example.com./").unwrap())
            .with_ip_addr("192.0.2.4:8245".parse().unwrap());
        let link = Link::new(addr).room("abc123").invite(Some(new_invite()));
        let s = link.to_string();
        assert!(s.starts_with("den://"), "{s}");
        assert_eq!(Link::parse(&s).unwrap(), link);
        assert_eq!(Link::parse(&s).unwrap().without_invite().invite, None);
        // Links from before the rename still parse.
        assert_eq!(Link::parse(&s.replacen("den://", "tailchat://", 1)).unwrap(), link);
        // A bare id works too, in either encoding.
        assert_eq!(Link::parse(&key.to_z32()).unwrap().id(), key);
        assert_eq!(Link::parse(&key.to_string()).unwrap().id(), key);
        assert!(Link::parse("box:8245").is_err());
        assert_eq!(unescape("a%26b%20c"), "a&b c");
    }

    #[test]
    fn invite_keys_roundtrip() {
        let s = new_invite();
        let k = invite_key(&s).unwrap();
        assert_eq!(BASE32_DNSSEC.encode(&k.to_bytes()), s);
        assert!(invite_key("nope").is_err());
    }
}
