//! Small subcommands: discover, api, mods, config, devices, voice-test.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use tailchat_core::api::{ApiClient, list_instances};
use tailchat_core::config::{self, AppConfig, ServerConfig};
use tailchat_core::mods::{self, ModsFile};
use tailchat_core::paths::Paths;
use tailchat_core::session::discover_rooms;
use tailchat_core::tailscale::Tailscale;

pub async fn discover(paths: &Paths, as_json: bool) -> anyhow::Result<()> {
    let cfg: AppConfig = config::load_or_default(&paths.config_file())?;
    let ts = Tailscale::locate(cfg.tailscale_bin.as_deref()).context("tailscale CLI not found")?;
    let status = ts.status().await?;
    let rooms = discover_rooms(&status, cfg.port).await;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&rooms)?);
        return Ok(());
    }
    let online = status.peers.iter().filter(|p| p.online).count();
    println!("tailnet {} — probed {} online peer(s) + this machine on port {}", status.tailnet.unwrap_or_default(), online, cfg.port);
    if rooms.is_empty() {
        println!("no rooms found. Host one with `tailchat host` or `tailchat serve`.");
    }
    for r in rooms {
        let me = if r.peer.is_self { " (this machine)" } else { "" };
        let saved = match tailchat_core::rooms::load(paths, &r.server.room_id) {
            Ok(s) => format!(" · saved (term {})", s.term),
            Err(_) => " · not saved".into(),
        };
        println!(
            "  {:<28} {:<24} {} member(s) · {} · term {}{saved}{me}\n    tailchat join {}",
            r.server.name,
            r.peer.short_name(),
            r.members,
            r.server.mode,
            r.server.term,
            r.addr
        );
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct ApiArgs {
    /// Method name, e.g. `state.get`, `message.send`, `api.methods`, `events.subscribe`.
    method: Option<String>,
    /// JSON object, or key=value pairs (values parsed as JSON when possible).
    params: Vec<String>,
    /// Talk to a headless server instance.
    #[arg(long, conflicts_with = "client")]
    server: bool,
    /// Talk to an app / terminal-client instance.
    #[arg(long)]
    client: bool,
    /// List running instances.
    #[arg(long)]
    list: bool,
    /// Print compact JSON.
    #[arg(long)]
    raw: bool,
}

fn parse_params(items: &[String]) -> anyhow::Result<Value> {
    if items.len() == 1 && items[0].trim_start().starts_with('{') {
        return serde_json::from_str(&items[0]).context("params must be a JSON object");
    }
    let mut obj = serde_json::Map::new();
    for item in items {
        let (k, v) = item.split_once('=').with_context(|| format!("expected key=value, got {item:?}"))?;
        let val = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_owned()));
        obj.insert(k.to_owned(), val);
    }
    Ok(Value::Object(obj))
}

pub async fn api(args: ApiArgs, paths: &Paths) -> anyhow::Result<()> {
    if args.list {
        for (path, i) in list_instances(paths) {
            println!("{:<7} pid {:<7} {:<22} {} ({})", i.kind, i.pid, i.addr, i.label, path.display());
        }
        return Ok(());
    }
    let method = args.method.clone().unwrap_or_else(|| "api.methods".into());
    let params = parse_params(&args.params)?;
    let kind = if args.server {
        Some("server")
    } else if args.client {
        Some("client")
    } else {
        None
    };
    let (mut client, _) = ApiClient::discover(paths, kind).await?;
    let print = |v: &Value| {
        if args.raw {
            println!("{v}");
        } else {
            println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        }
    };
    if method == "events.subscribe" {
        let filter: Vec<String> = serde_json::from_value(params["events"].clone()).unwrap_or_default();
        let mut stream = client.subscribe(&filter).await?;
        while let Some(ev) = stream.next().await? {
            println!("{}", serde_json::to_string(&ev)?);
        }
        return Ok(());
    }
    let result = client.call(&method, params).await?;
    if method == "api.methods" && !args.raw {
        println!("{} methods:", result["kind"].as_str().unwrap_or(""));
        for m in result["methods"].as_array().into_iter().flatten() {
            println!(
                "  {:<18} {:<34} {}",
                m["name"].as_str().unwrap_or(""),
                m["params"].as_str().unwrap_or(""),
                m["description"].as_str().unwrap_or("")
            );
        }
        let events: Vec<&str> = result["events"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("events: {}", events.join(", "));
        return Ok(());
    }
    print(&result);
    Ok(())
}

#[derive(Subcommand)]
pub enum ModsCmd {
    /// Installed mods and their status.
    List,
    /// Print the mods directory.
    Path,
    /// Create a starter mod.
    New {
        /// Directory to create (default: <mods dir>/<id>).
        dir: Option<PathBuf>,
        #[arg(long)]
        id: String,
    },
    /// Use a mod from anywhere on disk (like a symlink).
    Link {
        path: PathBuf,
    },
    /// Forget a linked mod directory.
    Unlink {
        path: PathBuf,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
}

fn save_mods_file(paths: &Paths, f: &ModsFile) -> anyhow::Result<()> {
    config::save(&paths.mods_file(), f)
}

/// Tell running instances to pick up mod changes.
async fn reload_running(paths: &Paths) {
    for (_, info) in list_instances(paths) {
        if let Ok(mut c) = ApiClient::connect(&info.addr, &info.token).await
            && c.call("mods.reload", json!({})).await.is_ok()
        {
            println!("reloaded mods in running {} (pid {})", info.kind, info.pid);
        }
    }
}

pub async fn mods(cmd: ModsCmd, paths: &Paths) -> anyhow::Result<()> {
    match cmd {
        ModsCmd::Path => println!("{}", paths.mods_dir().display()),
        ModsCmd::List => {
            let (client, problems) = mods::discover(paths, "client");
            let (server, _) = mods::discover(paths, "server");
            if client.is_empty() && problems.is_empty() {
                println!("no mods. Create one: tailchat mods new --id me.hello");
            }
            for m in &client {
                let srv = server.iter().find(|s| s.manifest.id == m.manifest.id);
                let runs = match (m.inactive_reason.is_none(), srv.is_some_and(|s| s.inactive_reason.is_none())) {
                    (true, true) => "client+server",
                    (true, false) => "client",
                    (false, true) => "server",
                    (false, false) => "inactive",
                };
                let state = if m.enabled { "enabled" } else { "disabled" };
                println!("{} {} [{state}, {runs}, {}]", m.manifest.id, m.manifest.version, m.source);
                if let Some(d) = &m.manifest.description {
                    println!("    {d}");
                }
                let cmds: Vec<String> = m.manifest.commands.iter().map(|c| format!("/{}", c.name)).collect();
                let panels: Vec<String> = m.manifest.panels.iter().map(|p| format!("{}/{}", m.manifest.id, p.id)).collect();
                if !cmds.is_empty() {
                    println!("    commands: {}", cmds.join(" "));
                }
                if !panels.is_empty() {
                    println!("    panels:   {}", panels.join(" "));
                }
                println!("    {}", m.root.display());
            }
            for p in problems {
                println!("! {}: {}", p.path.display(), p.error);
            }
        }
        ModsCmd::New { dir, id } => {
            let dir = dir.unwrap_or_else(|| paths.mods_dir().join(&id));
            mods::scaffold(&dir, &id)?;
            println!("created {}\n  edit {}/tailchat-mod.toml, then try /hello in chat", dir.display(), dir.display());
            if !dir.starts_with(paths.mods_dir()) {
                println!("  it is outside the mods dir; run `tailchat mods link {}`", dir.display());
            }
            reload_running(paths).await;
        }
        ModsCmd::Link { path } => {
            let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
            let m = mods::read_manifest(&path)?;
            let mut f = mods::load_mods_file(paths);
            if !f.linked.contains(&path) {
                f.linked.push(path.clone());
            }
            save_mods_file(paths, &f)?;
            println!("linked {} ({})", m.id, path.display());
            reload_running(paths).await;
        }
        ModsCmd::Unlink { path } => {
            let canon = path.canonicalize().unwrap_or(path.clone());
            let mut f = mods::load_mods_file(paths);
            let before = f.linked.len();
            f.linked.retain(|p| p != &canon && p != &path);
            if f.linked.len() == before {
                bail!("{} is not linked", path.display());
            }
            save_mods_file(paths, &f)?;
            println!("unlinked {}", path.display());
            reload_running(paths).await;
        }
        ModsCmd::Enable { id } | ModsCmd::Disable { id } if id.is_empty() => bail!("give a mod id"),
        ModsCmd::Enable { id } => {
            let mut f = mods::load_mods_file(paths);
            f.disabled.retain(|d| d != &id);
            save_mods_file(paths, &f)?;
            println!("enabled {id}");
            reload_running(paths).await;
        }
        ModsCmd::Disable { id } => {
            let mut f = mods::load_mods_file(paths);
            if !f.disabled.contains(&id) {
                f.disabled.push(id.clone());
            }
            save_mods_file(paths, &f)?;
            println!("disabled {id}");
            reload_running(paths).await;
        }
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print where everything lives.
    Path,
    /// Write starter config, server config, themes and layouts (never overwrites).
    Init,
}

pub fn config(cmd: ConfigCmd, paths: &Paths) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Path => {
            println!("config   {}", paths.config.display());
            println!("  app      {}", paths.config_file().display());
            println!("  server   {}", paths.server_config_file().display());
            println!("  mods     {}", paths.mods_dir().display());
            println!("  themes   {}", paths.themes_dir().display());
            println!("  layouts  {}", paths.layouts_dir().display());
            println!("data     {}", paths.data.display());
            println!("state    {}", paths.state.display());
        }
        ConfigCmd::Init => {
            let mut wrote = Vec::new();
            if !paths.config_file().exists() {
                config::save(&paths.config_file(), &AppConfig::default())?;
                wrote.push(paths.config_file());
            }
            if !paths.server_config_file().exists() {
                config::save(&paths.server_config_file(), &ServerConfig::default())?;
                wrote.push(paths.server_config_file());
            }
            #[cfg(feature = "gui")]
            wrote.extend(tailchat_gui::assets::export(paths)?);
            std::fs::create_dir_all(paths.mods_dir())?;
            if wrote.is_empty() {
                println!("nothing to do; everything already exists under {}", paths.config.display());
            }
            for p in wrote {
                println!("wrote {}", p.display());
            }
        }
    }
    Ok(())
}

pub fn devices() -> anyhow::Result<()> {
    #[cfg(feature = "voice")]
    {
        let d = tailchat_core::audio::list_devices()?;
        println!("inputs:");
        for i in &d.inputs {
            println!("  {}{}", i, if Some(i) == d.default_input.as_ref() { "  (default)" } else { "" });
        }
        println!("outputs:");
        for o in &d.outputs {
            println!("  {}{}", o, if Some(o) == d.default_output.as_ref() { "  (default)" } else { "" });
        }
        println!("codecs: {:?}", tailchat_core::audio::supported_codecs());
        Ok(())
    }
    #[cfg(not(feature = "voice"))]
    bail!("this build has no voice support")
}

pub fn voice_test(paths: &Paths, seconds: u64, codec: &str) -> anyhow::Result<()> {
    #[cfg(feature = "voice")]
    {
        use tailchat_core::proto::voice::Codec;
        let cfg: AppConfig = config::load_or_default(&paths.config_file())?;
        let codec = match codec {
            "pcm16" | "pcm" => Codec::Pcm16,
            _ => Codec::Opus,
        };
        println!("speak now — you should hear yourself for {seconds}s (use headphones)…");
        let stats = tailchat_core::audio::loopback_test(
            tailchat_core::voice::settings(&cfg.voice),
            codec,
            std::time::Duration::from_secs(seconds),
        )?;
        println!(
            "input {:?} → output {:?}, codec {:?}, last mic level {:.3}{}",
            stats.input_device.unwrap_or_default(),
            stats.output_device.unwrap_or_default(),
            stats.tx_codec,
            stats.input_level,
            stats.last_error.map(|e| format!(", note: {e}")).unwrap_or_default()
        );
        Ok(())
    }
    #[cfg(not(feature = "voice"))]
    {
        let _ = (paths, seconds, codec);
        bail!("this build has no voice support")
    }
}
