//! `den doctor`: what this machine's network looks like to Den, without
//! touching a running app (a throwaway endpoint does the looking). One line
//! per fact, or one JSON object with `--json`, so agents can check it.

use std::time::Duration;

use denchat_core::config::{self, AppConfig};
use denchat_core::net::{self, Net, SecretKey};
use denchat_core::paths::Paths;
use serde_json::{Value, json};

pub async fn run(paths: &Paths, as_json: bool) -> anyhow::Result<()> {
    let out = match report(paths).await {
        Ok(out) => out,
        Err(e) => {
            // The setups that can't even start an endpoint are the ones worth
            // diagnosing: still give the script its JSON.
            if as_json {
                println!("{}", serde_json::to_string_pretty(&json!({"version": denchat_core::VERSION, "error": format!("{e:#}")}))?);
            }
            return Err(e);
        }
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_text(&out);
    }
    Ok(())
}

async fn report(paths: &Paths) -> anyhow::Result<Value> {
    let cfg: AppConfig = config::load_or_default(&paths.config_file())?;
    let direct = cfg.net.direct_enabled();
    // Port mapping is checked on its own below; the endpoint shouldn't map too.
    let net_cfg = config::NetConfig { port_mapping: false, ..cfg.net.clone() };
    let net = Net::bind(SecretKey::generate(), &cfg.bind, 0, false, &net_cfg).await?;
    let addr = net.ready_addr().await;
    let report = net.wait_report(Duration::from_secs(6)).await;

    let port = net.bound().iter().find(|a| a.is_ipv4()).map(|a| a.port()).unwrap_or(0);
    let portmap = if cfg.net.port_mapping && direct { Some(net::check_port_mapping(port, Duration::from_secs(4)).await) } else { None };

    let mut facts = net.facts();
    // Only a mapping to a public address helps (not one behind CGNAT or a second router).
    facts.mapped |= portmap
        .as_ref()
        .and_then(|p| p.mapped.as_deref())
        .is_some_and(|m| m.parse::<std::net::SocketAddr>().is_ok_and(|a| denchat_core::relay::is_public(a.ip())));
    // No relay answered address discovery: UDP may be blocked, or the relays
    // (a self-hosted `den relay`) don't offer it. Either way, not measured.
    let udp_probed = report.as_ref().is_some_and(|r| r.udp_v4 || r.udp_v6);
    if !udp_probed {
        facts.measured = false;
    }
    let reach = if direct { facts.reach() } else { net.reach() };

    // The home relay connects shortly after it's picked.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut health = net.relay_health();
    while health.iter().any(|h| !h.connected && h.auth_denied.is_none()) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        health = net.relay_health();
    }
    let mut relays = Vec::new();
    for url in net.relay_urls() {
        let shown = denchat_core::relay::without_token(&url);
        let latency = report.as_ref().and_then(|r| r.relay_latency_ms.iter().find(|(u, _)| *u == shown).map(|(_, ms)| *ms));
        let tcp = net::tcp_rtt(&url, Duration::from_secs(3)).await;
        let home = health.iter().find(|h| h.url == shown);
        relays.push(json!({
            "url": shown,
            "latency_ms": latency.or_else(|| tcp.as_ref().ok().map(|d| d.as_millis() as u64)),
            "reachable": tcp.is_ok(),
            "error": tcp.err(),
            "home": home.is_some(),
            "connected": home.map(|h| h.connected),
            "refused": home.and_then(|h| h.auth_denied.clone()),
        }));
    }
    net.close().await;
    let firewall = firewall().await;

    let out = json!({
        "version": denchat_core::VERSION,
        "paths": {
            "config": paths.config_file(),
            "data": paths.data,
            "state": paths.state,
        },
        "direct": direct,
        "reach": reach,
        "udp": report.as_ref().map(|r| json!({"v4": r.udp_v4, "v6": r.udp_v6, "probed": udp_probed})),
        "public": report.as_ref().map(|r| json!({"v4": r.global_v4, "v6": r.global_v6})),
        "nat_varies": report.as_ref().map(|r| json!({"v4": r.varies_v4, "v6": r.varies_v6})),
        "captive_portal": report.as_ref().and_then(|r| r.captive_portal),
        "addresses": addr.ip_addrs().map(|a| a.to_string()).collect::<Vec<_>>(),
        "port_mapping": match &portmap {
            Some(p) => json!({"enabled": true, "offered": p.offered, "mapped": p.mapped, "error": p.error}),
            None => json!({"enabled": false}),
        },
        "relays": relays,
        "firewall": firewall,
    });
    Ok(out)
}

fn print_text(o: &Value) {
    let s = |v: &Value| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
    println!("den {} doctor", s(&o["version"]));
    println!("config    {}", s(&o["paths"]["config"]));
    println!("data      {}", s(&o["paths"]["data"]));
    println!("state     {}", s(&o["paths"]["state"]));
    if o["direct"] == true {
        println!("direct    on");
    } else {
        println!("direct    off: relay only ([net] direct = false or DEN_NO_DIRECT)");
    }
    let udp = match &o["udp"] {
        Value::Null => "no network report (no relays to measure against)".to_owned(),
        u if u["probed"] != true => {
            "udp not measured: no relay answered address discovery (UDP blocked, or the relays don't offer it)".to_owned()
        }
        u => format!("udp v4 {}, v6 {}", yes(&u["v4"]), yes(&u["v6"])),
    };
    println!("reach     {} ({udp})", s(&o["reach"]));
    let public: Vec<String> = ["v4", "v6"].iter().filter_map(|k| o["public"][k].as_str().map(str::to_owned)).collect();
    if !public.is_empty() {
        println!("public    {}", public.join(", "));
    }
    if o["nat_varies"]["v4"] == true || o["nat_varies"]["v6"] == true {
        println!("nat       strict: the port changes per destination, hole-punching rarely works");
    }
    if o["captive_portal"] == true {
        println!("portal    a captive portal seems to intercept HTTP");
    }
    let addrs: Vec<String> = o["addresses"].as_array().into_iter().flatten().map(s).collect();
    println!("addresses {}", if addrs.is_empty() { "none".to_owned() } else { addrs.join(", ") });
    let pm = &o["port_mapping"];
    let line = if pm["enabled"] != true {
        "off ([net] port_mapping = false, or direct off)".to_owned()
    } else {
        let offered: Vec<String> = pm["offered"].as_array().into_iter().flatten().map(s).collect();
        let offered = if offered.is_empty() { "router offers none".to_owned() } else { format!("router offers {}", offered.join(", ")) };
        match (pm["mapped"].as_str(), pm["error"].as_str()) {
            (Some(m), _) => format!("{offered}; mapped {m}"),
            (None, Some(e)) => format!("{offered}; {e}"),
            (None, None) => offered,
        }
    };
    println!("port map  {line}");
    let relays = o["relays"].as_array().cloned().unwrap_or_default();
    if relays.is_empty() {
        println!("relays    none ([net] relays = [])");
    }
    for r in relays {
        let mut parts = vec![match (r["reachable"].as_bool(), r["latency_ms"].as_u64()) {
            (Some(true), Some(ms)) => format!("{ms} ms"),
            (Some(true), None) => "reachable".to_owned(),
            _ => format!("unreachable ({})", r["error"].as_str().unwrap_or("?")),
        }];
        if r["home"] == true {
            parts.push(if r["connected"] == true { "home, connected".into() } else { "home, not connected".into() });
        }
        if let Some(why) = r["refused"].as_str() {
            parts.push(format!("refused: {why}"));
        }
        println!("relay     {} {}", s(&r["url"]), parts.join(", "));
    }
    let fw = &o["firewall"];
    if fw["checked"] != true {
        println!("firewall  not checked ({})", fw["note"].as_str().unwrap_or("?"));
    } else {
        let rules = fw["rules"].as_array().cloned().unwrap_or_default();
        let profile = fw["profile"].as_str().map(|p| format!(", network profile {p}")).unwrap_or_default();
        if rules.is_empty() {
            println!("firewall  no rule for {}{profile}; Windows may block incoming connections", s(&fw["program"]));
        }
        for r in rules {
            println!(
                "firewall  {} {} {} {} profiles {}{}",
                s(&r["action"]),
                s(&r["direction"]),
                s(&r["protocol"]),
                if r["enabled"] == true { "enabled" } else { "disabled" },
                s(&r["profiles"]),
                profile
            );
        }
    }
}

fn yes(v: &Value) -> &'static str {
    if v == true { "yes" } else { "no" }
}

/// Windows Defender Firewall rules for this executable (best effort: it
/// reads `netsh`'s English output).
async fn firewall() -> Value {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let run = |args: &'static [&'static str]| async move {
            let out = tokio::time::timeout(Duration::from_secs(15), tokio::process::Command::new("netsh").args(args).output()).await;
            match out {
                Ok(Ok(o)) if o.status.success() => Ok(console_text(&o.stdout)),
                Ok(Ok(o)) => Err(console_text(&o.stdout).trim().to_owned()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("netsh took too long".to_owned()),
            }
        };
        let rules = match run(&["advfirewall", "firewall", "show", "rule", "name=all", "verbose"]).await {
            Ok(text) => text,
            Err(e) => return json!({"checked": false, "note": format!("netsh failed: {e}")}),
        };
        if !rules.contains("Rule Name:") {
            return json!({"checked": false, "note": "couldn't read netsh's output (non-English Windows?)"});
        }
        let profile = run(&["advfirewall", "show", "currentprofile"]).await.ok().and_then(|t| current_profile(&t));
        json!({"checked": true, "program": exe, "profile": profile, "rules": rules_for(&rules, &exe)})
    }
    #[cfg(not(windows))]
    json!({"checked": false, "note": "only on Windows"})
}

/// netsh writes in the console's code page (the OEM one unless `chcp` changed
/// it), not UTF-8: decode it so a path like `C:\Users\José\…` still matches.
#[cfg(windows)]
fn console_text(b: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;
    if b.is_ascii() {
        return String::from_utf8_lossy(b).into_owned();
    }
    let Ok(len) = i32::try_from(b.len()) else { return String::from_utf8_lossy(b).into_owned() };
    // SAFETY: the buffers and lengths passed are the slices' own.
    unsafe {
        let cp = match GetConsoleOutputCP() {
            0 => CP_OEMCP,
            cp => cp,
        };
        let n = MultiByteToWideChar(cp, 0, b.as_ptr(), len, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(b).into_owned();
        }
        let mut wide = vec![0u16; n as usize];
        let n = MultiByteToWideChar(cp, 0, b.as_ptr(), len, wide.as_mut_ptr(), n);
        String::from_utf16_lossy(&wide[..n.max(0) as usize])
    }
}

/// The rules in `netsh advfirewall firewall show rule name=all verbose`
/// output whose program is `exe`.
#[cfg_attr(not(windows), allow(dead_code))]
fn rules_for(text: &str, exe: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut rule: Vec<(String, String)> = Vec::new();
    let mut flush = |rule: &mut Vec<(String, String)>| {
        let get = |k: &str| rule.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).unwrap_or_default();
        if !rule.is_empty() && get("Program").eq_ignore_ascii_case(exe) {
            out.push(json!({
                "name": get("Rule Name"),
                "enabled": get("Enabled").eq_ignore_ascii_case("yes"),
                "direction": get("Direction"),
                "profiles": get("Profiles"),
                "protocol": get("Protocol"),
                "action": get("Action"),
            }));
        }
        rule.clear();
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("Rule Name") {
            flush(&mut rule);
        }
        if !k.is_empty() && !k.starts_with('-') {
            rule.push((k.to_owned(), v.to_owned()));
        }
    }
    flush(&mut rule);
    out
}

/// `Public`, `Private` or `Domain` from `netsh advfirewall show currentprofile`.
#[cfg_attr(not(windows), allow(dead_code))]
fn current_profile(text: &str) -> Option<String> {
    let first = text.lines().map(str::trim).find(|l| l.ends_with("Profile Settings:"))?;
    Some(first.trim_end_matches("Profile Settings:").trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETSH: &str = "\r
Rule Name:                            den\r
----------------------------------------------------------------------\r
Enabled:                              Yes\r
Direction:                            In\r
Profiles:                             Private\r
Protocol:                             UDP\r
Program:                              C:\\Users\\x\\den.exe\r
Action:                               Allow\r
\r
Rule Name:                            den\r
----------------------------------------------------------------------\r
Enabled:                              Yes\r
Direction:                            In\r
Profiles:                             Public\r
Protocol:                             UDP\r
Program:                              C:\\Users\\x\\den.exe\r
Action:                               Block\r
\r
Rule Name:                            Other app\r
----------------------------------------------------------------------\r
Enabled:                              No\r
Direction:                            Out\r
Profiles:                             Domain,Private,Public\r
Protocol:                             TCP\r
Program:                              C:\\Program Files\\other.exe\r
Action:                               Allow\r
Ok.\r
";

    #[test]
    fn netsh_rules_for_the_exe() {
        let rules = rules_for(NETSH, r"c:\users\x\DEN.exe");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["action"], "Allow");
        assert_eq!(rules[0]["profiles"], "Private");
        assert_eq!(rules[1]["action"], "Block");
        assert_eq!(rules[1]["enabled"], true);
        assert!(rules_for(NETSH, r"C:\nowhere.exe").is_empty());
        let profile = "\r\nPublic Profile Settings:\r\n----\r\nState                                 ON\r\n";
        assert_eq!(current_profile(profile).as_deref(), Some("Public"));
    }
}
