//! Tailnet integration through the local `tailscale` CLI.
//!
//! tailchat does not embed a Tailscale node: it rides on the machine's
//! existing `tailscaled`. That keeps the binary small and lets the OS client
//! handle login, ACLs and NAT traversal. We use the CLI for three things:
//!
//! * `tailscale ip -4` / `status --json` — what address to bind and share.
//! * `tailscale status --json` — peers to probe for rooms (discovery).
//! * `tailscale whois --json <ip>` — who is on the other end of a connection,
//!   so the server can show verified tailnet identities without passwords.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const CLI_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug)]
pub struct Tailscale {
    bin: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TailnetPeer {
    pub host_name: String,
    /// MagicDNS name without the trailing dot, e.g. `box.tail1234.ts.net`.
    pub dns_name: Option<String>,
    pub ips: Vec<IpAddr>,
    pub os: String,
    pub online: bool,
    pub owner_login: Option<String>,
    pub owner_name: Option<String>,
    pub is_self: bool,
}

impl TailnetPeer {
    pub fn ipv4(&self) -> Option<IpAddr> {
        self.ips.iter().copied().find(IpAddr::is_ipv4)
    }

    /// Short MagicDNS host (`box`) if available, else the host name.
    pub fn short_name(&self) -> String {
        self.dns_name
            .as_deref()
            .and_then(|d| d.split('.').next())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.host_name.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TailnetStatus {
    pub backend_state: String,
    pub me: Option<TailnetPeer>,
    pub peers: Vec<TailnetPeer>,
    pub tailnet: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WhoIs {
    pub login: Option<String>,
    pub display_name: Option<String>,
    pub node: Option<String>,
}

impl Tailscale {
    /// Find the CLI: explicit path, `TAILCHAT_TAILSCALE`, PATH, then the usual install spots.
    pub fn locate(explicit: Option<&Path>) -> Option<Tailscale> {
        if let Some(p) = explicit {
            return Some(Tailscale { bin: p.to_path_buf() });
        }
        if let Some(p) = std::env::var_os("TAILCHAT_TAILSCALE").filter(|v| !v.is_empty()) {
            return Some(Tailscale { bin: PathBuf::from(p) });
        }
        let exe = if cfg!(windows) { "tailscale.exe" } else { "tailscale" };
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let cand = dir.join(exe);
                if cand.is_file() {
                    return Some(Tailscale { bin: cand });
                }
            }
        }
        let fallbacks: &[&str] = if cfg!(target_os = "macos") {
            &["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale"]
        } else if cfg!(windows) {
            &[r"C:\Program Files\Tailscale\tailscale.exe"]
        } else {
            &["/usr/bin/tailscale", "/usr/sbin/tailscale", "/usr/local/bin/tailscale"]
        };
        fallbacks.iter().map(PathBuf::from).find(|p| p.is_file()).map(|bin| Tailscale { bin })
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    async fn run_json(&self, args: &[&str]) -> anyhow::Result<serde_json::Value> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).kill_on_drop(true);
        hide_console(&mut cmd);
        let out = tokio::time::timeout(CLI_TIMEOUT, cmd.output())
            .await
            .context("tailscale CLI timed out")?
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !out.status.success() {
            bail!("tailscale {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        // The CLI prints version-mismatch warnings on stderr; stdout is clean JSON.
        serde_json::from_slice(&out.stdout).context("parsing tailscale json")
    }

    pub async fn status(&self) -> anyhow::Result<TailnetStatus> {
        let v = self.run_json(&["status", "--json"]).await?;
        Ok(parse_status(&v))
    }

    /// This machine's tailnet IPv4, if Tailscale is up.
    pub async fn self_ipv4(&self) -> anyhow::Result<Option<IpAddr>> {
        let st = self.status().await?;
        Ok(st.me.and_then(|m| m.ipv4()))
    }

    /// Identify the owner of a tailnet address.
    pub async fn whois(&self, ip: IpAddr) -> anyhow::Result<Option<WhoIs>> {
        if !is_tailnet_ip(ip) {
            return Ok(None);
        }
        let v = match self.run_json(&["whois", "--json", &ip.to_string()]).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("whois {ip}: {e:#}");
                return Ok(None);
            }
        };
        let profile = &v["UserProfile"];
        let node = v["Node"]["Name"]
            .as_str()
            .map(|n| n.trim_end_matches('.').split('.').next().unwrap_or(n).to_owned())
            .or_else(|| v["Node"]["ComputedName"].as_str().map(str::to_owned));
        Ok(Some(WhoIs {
            login: profile["LoginName"].as_str().map(str::to_owned),
            display_name: profile["DisplayName"].as_str().map(str::to_owned),
            node,
        }))
    }
}

fn parse_peer(v: &serde_json::Value, users: &serde_json::Value, is_self: bool) -> TailnetPeer {
    let uid = v["UserID"].as_u64().map(|u| u.to_string());
    let user = uid.as_ref().map(|id| &users[id.as_str()]);
    TailnetPeer {
        host_name: v["HostName"].as_str().unwrap_or_default().to_owned(),
        dns_name: v["DNSName"].as_str().map(|d| d.trim_end_matches('.').to_owned()).filter(|d| !d.is_empty()),
        ips: v["TailscaleIPs"].as_array().map(|a| a.iter().filter_map(|ip| ip.as_str()?.parse().ok()).collect()).unwrap_or_default(),
        os: v["OS"].as_str().unwrap_or_default().to_owned(),
        online: v["Online"].as_bool().unwrap_or(is_self),
        owner_login: user.and_then(|u| u["LoginName"].as_str().map(str::to_owned)),
        owner_name: user.and_then(|u| u["DisplayName"].as_str().map(str::to_owned)),
        is_self,
    }
}

pub fn parse_status(v: &serde_json::Value) -> TailnetStatus {
    let users = &v["User"];
    let me = v.get("Self").filter(|s| s.is_object()).map(|s| parse_peer(s, users, true));
    let mut peers: Vec<TailnetPeer> =
        v["Peer"].as_object().map(|m| m.values().map(|p| parse_peer(p, users, false)).collect()).unwrap_or_default();
    peers.sort_by(|a, b| b.online.cmp(&a.online).then(a.host_name.cmp(&b.host_name)));
    TailnetStatus {
        backend_state: v["BackendState"].as_str().unwrap_or_default().to_owned(),
        me,
        peers,
        tailnet: v["CurrentTailnet"]["Name"].as_str().map(str::to_owned),
    }
}

/// True for Tailscale CGNAT IPv4 (100.64.0.0/10) and ULA IPv6 (fd7a:115c:a1e0::/48).
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

#[cfg(windows)]
pub(crate) fn hide_console(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn hide_console(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_ranges() {
        assert!(is_tailnet_ip("100.106.195.111".parse().unwrap()));
        assert!(is_tailnet_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_tailnet_ip("192.168.0.1".parse().unwrap()));
        assert!(is_tailnet_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_tailnet_ip("::1".parse().unwrap()));
    }

    #[test]
    fn parses_status_subset() {
        let v = serde_json::json!({
            "BackendState": "Running",
            "CurrentTailnet": {"Name": "example.com"},
            "Self": {"HostName": "Mac", "DNSName": "mac.tail.ts.net.", "TailscaleIPs": ["100.1.2.3"], "OS": "macOS", "UserID": 1},
            "Peer": {"k": {"HostName": "box", "DNSName": "box.tail.ts.net.", "TailscaleIPs": ["100.1.2.4", "fd7a:115c:a1e0::4"], "OS": "linux", "Online": true, "UserID": 1}},
            "User": {"1": {"LoginName": "a@example.com", "DisplayName": "A"}}
        });
        let st = parse_status(&v);
        let me = st.me.unwrap();
        assert!(me.is_self && me.online);
        assert_eq!(me.short_name(), "mac");
        assert_eq!(st.peers[0].ipv4().unwrap().to_string(), "100.1.2.4");
        assert_eq!(st.peers[0].owner_login.as_deref(), Some("a@example.com"));
        assert_eq!(st.tailnet.as_deref(), Some("example.com"));
    }
}
