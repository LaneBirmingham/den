//! `tailchat serve`: an always-on host for a room, with an admin console.
//!
//! Which room: `--room <id|name>` (a saved room, e.g. one copied from a
//! member's data folder), `--adopt <addr>` (the room someone is hosting
//! there right now), or by default the saved room named like the config's
//! `name` (created on first run).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tailchat_core::config::{self, ChannelSpec, ServerConfig};
use tailchat_core::events::Event;
use tailchat_core::paths::Paths;
use tailchat_core::proto::{ChannelKind, ChatMessage, Member, MessageKind};
use tailchat_core::rooms::{self, RoomSave};
use tailchat_core::server_node::{NodeOptions, ServerNode};
use tailchat_core::tailscale::Tailscale;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

use crate::term::clock;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Server config file (default: <config>/server.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Host this saved room (id or name, see `<data>/rooms/`). Its name,
    /// channels and join rules win over server.toml's; flags still override.
    #[arg(long, conflicts_with = "adopt")]
    room: Option<String>,
    /// Take over the room hosted at this address (`machine` or `host:port`).
    /// Members' copies fill in its history as they move over.
    #[arg(long)]
    adopt: Option<String>,
    #[arg(long)]
    name: Option<String>,
    /// tailnet (default), localhost, all, or an IP.
    #[arg(long)]
    bind: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    motd: Option<String>,
    /// Channels, replacing the config's list: `general`, `lounge:voice`. Repeatable.
    #[arg(long = "channel")]
    channels: Vec<String>,
    /// Only admit peers identified by Tailscale.
    #[arg(long)]
    require_tailnet: bool,
    /// Only admit these tailnet logins. Repeatable.
    #[arg(long = "allow")]
    allow: Vec<String>,
    /// Don't load server-side mods.
    #[arg(long)]
    no_mods: bool,
    /// Don't expose the local control API.
    #[arg(long)]
    no_api: bool,
    /// Don't read admin commands from stdin (for services).
    #[arg(long)]
    no_console: bool,
    /// Don't print chat traffic.
    #[arg(long)]
    quiet: bool,
    /// Write the effective config to the config file and exit.
    #[arg(long)]
    write_config: bool,
}

pub async fn run(args: ServeArgs, paths: Paths) -> anyhow::Result<()> {
    let path = args.config.clone().unwrap_or_else(|| paths.server_config_file());
    let mut cfg: ServerConfig = config::load_or_default(&path)?;
    let save = match (&args.room, &args.adopt) {
        (Some(key), _) => {
            let save = rooms::find(&paths, key).with_context(|| format!("no saved room {key:?} in {}", paths.rooms_dir().display()))?;
            cfg = save.server_config(cfg);
            save
        }
        (None, Some(addr)) => {
            let save = rooms::adopt(&paths, addr, cfg.port).await.with_context(|| format!("adopting the room at {addr}"))?;
            cfg = save.server_config(cfg);
            save
        }
        (None, None) => {
            let name = args.name.clone().unwrap_or_else(|| cfg.name.clone());
            match rooms::find(&paths, &name) {
                Some(save) => save,
                None => {
                    let save = RoomSave::new(&ServerConfig { name, ..cfg.clone() });
                    // History from before rooms were saved per room.
                    let n = rooms::import_history(&paths.legacy_history_dir(), &rooms::history_dir(&paths, &save.id))?;
                    if n > 0 {
                        println!("imported history from {}", paths.legacy_history_dir().display());
                    }
                    save
                }
            }
        }
    };
    if let Some(v) = args.name {
        cfg.name = v;
    }
    if let Some(v) = args.bind {
        cfg.bind = v;
    }
    if let Some(v) = args.port {
        cfg.port = v;
    }
    if args.motd.is_some() {
        cfg.motd = args.motd;
    }
    if !args.channels.is_empty() {
        cfg.channels = args
            .channels
            .iter()
            .map(|c| {
                let (id, kind) = match c.split_once(':') {
                    Some((id, "voice")) => (id, ChannelKind::Voice),
                    Some((id, _)) => (id, ChannelKind::Text),
                    None => (c.as_str(), ChannelKind::Text),
                };
                ChannelSpec { id: id.into(), name: None, kind, topic: None }
            })
            .collect();
    }
    cfg.require_tailnet |= args.require_tailnet;
    cfg.allow.extend(args.allow);
    if args.write_config {
        config::save(&path, &cfg)?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    let node =
        ServerNode::start(cfg.clone(), save, paths.clone(), NodeOptions { api: !args.no_api, mods: !args.no_mods, tailscale_bin: None })
            .await?;
    print_banner(&node, &cfg, &paths, !args.no_console).await;

    let quiet = args.quiet;
    let mut events = node.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => log_event(&ev, quiet),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });

    if !args.no_console {
        let console = node.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            // EOF (e.g. under a service manager) just ends the console; the room keeps running.
            while let Ok(Some(line)) = lines.next_line().await {
                if !console_command(&console, line.trim()).await {
                    std::process::exit(0);
                }
            }
        });
    }

    wait_for_signal().await;
    println!("shutting down…");
    node.shutdown();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn print_banner(node: &ServerNode, cfg: &ServerConfig, paths: &Paths, console: bool) {
    let addr = node.local_addr();
    println!("tailchat {} — room \"{}\" on {addr} (tcp+udp)", tailchat_core::VERSION, cfg.name);
    if let Some(ts) = Tailscale::locate(None)
        && let Ok(st) = ts.status().await
        && let Some(me) = st.me
    {
        let port = if addr.port() == tailchat_core::proto::DEFAULT_PORT { String::new() } else { format!(":{}", addr.port()) };
        println!("  join with:  tailchat join {}{port}", me.short_name());
        if let Some(d) = me.dns_name {
            println!("              tailchat join {d}{port}");
        }
    }
    let channels: Vec<String> = node
        .server()
        .channels()
        .iter()
        .map(|c| if c.kind == ChannelKind::Voice { format!("~{}", c.id) } else { format!("#{}", c.id) })
        .collect();
    println!("  channels:   {}", channels.join(" "));
    let save = node.save();
    println!("  room:       {} (term {}) · saved in {}", save.id, save.term, rooms::dir(paths, &save.id).display());
    if let Some(from) = node.claimed_from() {
        println!("  took over:  the room was hosted at {from}; members are moving here");
    }
    if let Some(api) = node.api_addr() {
        println!("  control API {api} (tailchat api --server …)");
    }
    let mods: Vec<String> = node.mods().active().map(|m| m.manifest.id.clone()).collect();
    if !mods.is_empty() {
        println!("  mods:       {}", mods.join(", "));
    }
    if console {
        println!("  admin console: type `help`");
    }
}

fn log_event(ev: &Event, quiet: bool) {
    let d = &ev.data;
    match ev.event.as_str() {
        "message.created" if !quiet => {
            if let Ok(m) = serde_json::from_value::<ChatMessage>(d["message"].clone()) {
                let who = if m.kind == MessageKind::Server { format!("** {}", m.author) } else { format!("<{}>", m.author) };
                println!("[{}] #{} {who} {}", clock(m.ts), m.channel, m.text);
            }
        }
        "member.joined" => {
            if let Ok(m) = serde_json::from_value::<Member>(d["member"].clone()) {
                let who = m
                    .identity
                    .login
                    .clone()
                    .map(|l| format!("{l} on {}", m.identity.node.clone().unwrap_or_default()))
                    .unwrap_or_else(|| "unverified".into());
                println!("+ [{}] {} joined ({who})", m.id, m.nick);
            }
        }
        "member.left" => {
            if let Ok(m) = serde_json::from_value::<Member>(d["member"].clone()) {
                println!("- [{}] {} left ({})", m.id, m.nick, d["reason"].as_str().unwrap_or(""));
            }
        }
        "room.moved" => println!("== the room moved to {} (term {})", d["to"][0].as_str().unwrap_or("?"), d["term"]),
        _ => {}
    }
}

/// Returns false to exit.
async fn console_command(node: &ServerNode, line: &str) -> bool {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let srv = node.server();
    match cmd {
        "" => {}
        "help" | "?" => println!(
            "who · channels · say [#channel] <text> · notice <text> · kick <id|nick> [reason]\n\
             motd [text] · mods · reload · /<mod-command> [args] · quit"
        ),
        "who" | "members" => {
            let members = srv.members();
            if members.is_empty() {
                println!("(nobody here)");
            }
            for m in members {
                let voice = m.voice.map(|v| format!(" ~{v}{}", if m.muted { " (muted)" } else { "" })).unwrap_or_default();
                let who = m.identity.login.unwrap_or_else(|| "unverified".into());
                println!("[{}] {} — {who}{voice} · {}", m.id, m.nick, m.client);
            }
        }
        "channels" => {
            for c in srv.channels() {
                println!("{}{} ({:?})", if c.kind == ChannelKind::Voice { "~" } else { "#" }, c.id, c.kind);
            }
        }
        "say" => {
            let (channel, text) = match rest.strip_prefix('#') {
                Some(r) => {
                    let (c, t) = r.split_once(char::is_whitespace).unwrap_or((r, ""));
                    (Some(c), t.trim())
                }
                None => (None, rest),
            };
            if let Err(e) = srv.say(channel, text) {
                println!("! {e:#}");
            }
        }
        "notice" => srv.notice_all(rest),
        "kick" => {
            let (who, reason) = rest.split_once(char::is_whitespace).unwrap_or((rest, "kicked by admin"));
            let target = srv.members().into_iter().find(|m| who.parse::<u32>().ok() == Some(m.id) || m.nick.eq_ignore_ascii_case(who));
            match target {
                Some(m) => {
                    srv.kick(m.id, reason.trim());
                    println!("kicked {}", m.nick);
                }
                None => println!("! no member {who:?} (see `who`)"),
            }
        }
        "motd" => {
            srv.set_motd((!rest.is_empty()).then(|| rest.to_owned()));
            println!("motd {}", if rest.is_empty() { "cleared" } else { "set" });
        }
        "mods" => {
            let mods = node.mods();
            for m in mods.mods() {
                let status = if m.active() {
                    "active".to_owned()
                } else if !m.enabled {
                    "disabled".into()
                } else {
                    m.inactive_reason.clone().unwrap_or_default()
                };
                println!("{} {} — {status}", m.manifest.id, m.manifest.version);
            }
            for p in mods.problems() {
                println!("! {}: {}", p.path.display(), p.error);
            }
        }
        "reload" => {
            node.reload_mods();
            println!("mods reloaded ({} active)", node.mods().active().count());
        }
        "quit" | "exit" | "stop" => {
            node.shutdown();
            tokio::time::sleep(Duration::from_millis(300)).await;
            return false;
        }
        c if c.starts_with('/') => {
            let args = tailchat_core::session::split_args(rest);
            if let Err(e) = node.run_mod_command(&c[1..], args).await {
                println!("! {e:#}");
            }
        }
        other => println!("! unknown command {other:?} — try help"),
    }
    true
}
