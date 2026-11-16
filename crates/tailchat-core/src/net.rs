//! Async framing and address helpers shared by server, client and discovery.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Serialize, de::DeserializeOwned};
use tailchat_proto::{ClientMsg, MAX_FRAME, ServerMsg, encode_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

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

/// Split `host`, `host:port`, `[v6]:port`, bare v6, or `tailchat://host:port`.
pub fn split_host_port(raw: &str, default_port: u16) -> (String, u16) {
    let s = raw.trim();
    let s = s.strip_prefix("tailchat://").unwrap_or(s).trim_end_matches('/');
    if let Some(rest) = s.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default_port);
        return (host.to_owned(), port);
    }
    if s.parse::<std::net::Ipv6Addr>().is_ok() {
        return (s.to_owned(), default_port);
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h.to_owned(), port),
            Err(_) => (s.to_owned(), default_port),
        },
        _ => (s.to_owned(), default_port),
    }
}

/// Resolve an address string, preferring IPv4 (tailnet IPv4 is always present).
pub async fn resolve(raw: &str, default_port: u16) -> anyhow::Result<SocketAddr> {
    let addrs = resolve_all(raw, default_port).await?;
    Ok(addrs[0])
}

/// Every address a name resolves to (never empty): tailnet addresses first,
/// then other IPv4, then the rest. A short machine name can resolve to a LAN
/// address as well as (or instead of) the tailnet one.
pub async fn resolve_all(raw: &str, default_port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    let (host, port) = split_host_port(raw, default_port);
    if host.is_empty() {
        bail!("empty address");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let mut addrs: Vec<SocketAddr> =
        tokio::net::lookup_host((host.as_str(), port)).await.with_context(|| format!("resolving {host}"))?.collect();
    addrs.dedup();
    addrs.sort_by_key(|a| (!crate::tailscale::is_tailnet_ip(a.ip()), !a.is_ipv4()));
    if addrs.is_empty() {
        bail!("{host} has no addresses");
    }
    Ok(addrs)
}

/// Ask a host for its room info without joining.
pub async fn probe(addr: SocketAddr, timeout: Duration) -> anyhow::Result<ServerMsg> {
    let fut = async {
        let mut s = TcpStream::connect(addr).await?;
        write_frame(&mut s, &ClientMsg::Probe).await?;
        let msg: Option<ServerMsg> = read_frame(&mut s).await?;
        msg.context("closed without reply")
    };
    tokio::time::timeout(timeout, fut).await.context("probe timed out")?
}

#[cfg(test)]
mod tests {
    use super::split_host_port as s;

    #[test]
    fn host_port_forms() {
        assert_eq!(s("box", 8245), ("box".into(), 8245));
        assert_eq!(s("box:9000", 8245), ("box".into(), 9000));
        assert_eq!(s("100.1.2.3:7", 1), ("100.1.2.3".into(), 7));
        assert_eq!(s("[fd7a::1]:9", 1), ("fd7a::1".into(), 9));
        assert_eq!(s("fd7a::1", 5), ("fd7a::1".into(), 5));
        assert_eq!(s("tailchat://box.ts.net:44/", 1), ("box.ts.net".into(), 44));
    }
}
