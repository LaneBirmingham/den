//! A relay of our own (iroh-relay, in-process).
//!
//! Peers that can't reach each other directly (strict NATs, UDP blocked) talk
//! through a relay: both connect out to it, it forwards packets by endpoint
//! id, and it only ever sees encrypted QUIC. It also carries the messages
//! that let two peers hole-punch a direct path. `den serve` runs one
//! next to its room, and `den relay` runs one on its own, so a room
//! needs nothing from a third party.
//!
//! The relay serves plain HTTP (no certificate or domain needed): everything
//! it carries is already end-to-end encrypted. Put a TLS reverse proxy in
//! front of it and give members the `https://` URL if you prefer.
//!
//! **It isn't open to the internet.** Every relay has a token (derived from
//! `<data>/relay.key`), and it admits only clients that present it. The token
//! rides in the relay URL (`http://host:8245/?token=…`), so it reaches exactly
//! the people who got an invite link from a room that uses the relay (or a
//! member's saved copy of the room), and nobody else can use your bandwidth.
//! Over plain HTTP someone on the network path could read the token; that
//! would let them relay through you, never join a room (rooms check the
//! invite key and member keys themselves). Delete `relay.key` to change the
//! token; links made before then lose the relay.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use data_encoding::BASE32_DNSSEC;
use iroh::{EndpointAddr, RelayUrl, SecretKey};
use iroh_relay::server::{Access, AccessControl, ClientRateLimit, ClientRequest, Limits, RelayConfig, Server, ServerConfig};

/// The URL query parameter (and `Authorization: Bearer` value) that carries
/// the token; iroh-relay reads either.
const TOKEN_PARAM: &str = "token";

/// A running relay.
pub struct LocalRelay {
    server: Server,
    /// The TCP address it listens on.
    pub addr: SocketAddr,
    /// How this machine reaches it (loopback when it listens everywhere),
    /// token included.
    pub local_url: RelayUrl,
    token: String,
}

impl LocalRelay {
    /// Listen on `ip` (None: everywhere) and TCP `port` (0: any free one),
    /// admitting only clients that present `token`.
    pub async fn start(ip: Option<IpAddr>, port: u16, token: String) -> anyhow::Result<LocalRelay> {
        let bind = SocketAddr::new(ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), port);
        let mut relay = RelayConfig::new(bind);
        // Generous for voice (a member sends tens of KB/s), but stops one
        // client from swamping a home connection.
        let mut limits = Limits::default();
        let mut rx = ClientRateLimit::new(NonZeroU32::new(4 << 20).expect("non-zero"));
        rx.max_burst_bytes = NonZeroU32::new(8 << 20);
        limits.client_rx = Some(rx);
        relay.limits = limits;
        relay.access = Arc::new(TokenGate { token: token.clone() });
        let mut config = ServerConfig::default();
        config.relay = Some(relay);
        let server = Server::spawn(config).await.with_context(|| format!("starting the relay on TCP {bind}"))?;
        let addr = server.http_addr().context("the relay has no HTTP address")?;
        let host = if addr.ip().is_unspecified() { IpAddr::V4(Ipv4Addr::LOCALHOST) } else { addr.ip() };
        let local_url = with_token(&url_for(SocketAddr::new(host, addr.port()))?, &token);
        tracing::info!("relay listening on TCP {addr}");
        Ok(LocalRelay { server, addr, local_url, token })
    }

    /// `base` (how others reach this relay) with the token added.
    pub fn url(&self, base: &RelayUrl) -> RelayUrl {
        with_token(base, &self.token)
    }

    pub async fn shutdown(self) {
        if let Err(e) = self.server.shutdown().await {
            tracing::debug!("relay shutdown: {e:#}");
        }
    }
}

/// Admits clients that present the relay's token, refuses everyone else.
#[derive(Debug)]
struct TokenGate {
    token: String,
}

impl AccessControl for TokenGate {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        match request.auth_token() {
            Some(t) if same(t.as_bytes(), self.token.as_bytes()) => Access::Allow,
            got => {
                tracing::debug!("relay: refused {} ({})", request.endpoint_id(), if got.is_some() { "wrong token" } else { "no token" });
                Access::Deny { reason: Some("this relay only carries rooms that use it: the invite link has its token".into()) }
            }
        }
    }
}

/// Compare without leaking how much of the token matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token for a relay whose key is `key` (stable across restarts).
pub fn token_for(key: &SecretKey) -> String {
    BASE32_DNSSEC.encode(&key.sign(b"den relay token\0").to_bytes()[..20])
}

/// The token of the relay that runs from data dir `data` (`relay.key` there,
/// created on first use).
pub fn load_token(data: &Path) -> anyhow::Result<String> {
    Ok(token_for(&crate::net::load_or_create_key(&data.join("relay.key"))?))
}

/// `url` with its token set to `token` (any other query is kept).
pub fn with_token(url: &RelayUrl, token: &str) -> RelayUrl {
    let mut u = (**url).clone();
    let rest: Vec<(String, String)> =
        u.query_pairs().filter(|(k, _)| k != TOKEN_PARAM).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    u.set_query(None);
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in &rest {
            q.append_pair(k, v);
        }
        q.append_pair(TOKEN_PARAM, token);
    }
    RelayUrl::from(u)
}

/// `url` without its token, for showing to people.
pub fn without_token(url: &RelayUrl) -> String {
    let mut u = (**url).clone();
    let rest: Vec<(String, String)> =
        u.query_pairs().filter(|(k, _)| k != TOKEN_PARAM).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    u.set_query(None);
    if !rest.is_empty() {
        u.query_pairs_mut().extend_pairs(&rest);
    }
    u.to_string()
}

/// `http://ip:port/` as a relay URL.
pub fn url_for(addr: SocketAddr) -> anyhow::Result<RelayUrl> {
    RelayUrl::from_str(&format!("http://{addr}/")).context("relay url")
}

/// A relay URL from config (`http://host:port`, `https://…`, or `host:port`).
pub fn parse_url(raw: &str) -> anyhow::Result<RelayUrl> {
    let raw = raw.trim();
    let full = if raw.contains("://") { raw.to_owned() } else { format!("http://{raw}") };
    RelayUrl::from_str(&full).with_context(|| format!("bad relay url {raw:?}"))
}

/// An address on the public internet (not private, loopback, link-local,
/// or carrier-grade NAT / Tailscale's 100.64/10).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            // Global unicast is 2000::/3.
            (0x2000..0x4000).contains(&first)
        }
    }
}

/// The relay URL peers should use, guessed from what the endpoint knows about
/// its public address (learned from relays' address discovery or the
/// router's port mapping): `http://<public IPv4>:<port>`.
pub fn public_url(addr: &EndpointAddr, port: u16) -> Option<RelayUrl> {
    let ip = addr.ip_addrs().map(|a| a.ip()).filter(|ip| ip.is_ipv4() && is_public(*ip)).min()?;
    url_for(SocketAddr::new(ip, port)).ok()
}

/// Wait up to `wait` for the endpoint to learn its public address, then
/// guess the relay URL from it.
pub async fn discover_public_url(net: &crate::net::Net, port: u16, wait: Duration) -> Option<RelayUrl> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(u) = public_url(&net.endpoint().addr(), port) {
            return Some(u);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for (ip, public) in [
            ("8.8.8.8", true),
            ("192.168.0.21", false),
            ("10.1.2.3", false),
            ("100.106.195.111", false),
            ("127.0.0.1", false),
            ("2606:4700::1111", true),
            ("fe80::1", false),
            ("::1", false),
        ] {
            assert_eq!(is_public(ip.parse().unwrap()), public, "{ip}");
        }
        assert_eq!(parse_url("box.example.com:8245").unwrap().as_str(), "http://box.example.com:8245/");
        assert_eq!(parse_url("https://relay.example.com").unwrap().as_str(), "https://relay.example.com/");
    }

    #[test]
    fn tokens_in_urls() {
        let base = parse_url("box.example.com:8245").unwrap();
        let t = token_for(&SecretKey::from_bytes(&[7; 32]));
        assert_eq!(t.len(), 32);
        let u = with_token(&base, &t);
        assert_eq!(u.as_str(), format!("http://box.example.com:8245/?token={t}"));
        // Setting it again replaces it; other query parameters stay.
        let odd = RelayUrl::from_str("https://r.example.com/?region=au&token=old").unwrap();
        assert_eq!(with_token(&odd, "new").as_str(), "https://r.example.com/?region=au&token=new");
        assert_eq!(without_token(&u), "http://box.example.com:8245/");
        assert_eq!(without_token(&odd), "https://r.example.com/?region=au");
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
    }
}
