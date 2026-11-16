//! Commands for agents and scripts: drive a running app (`den ui …`), wait
//! for events (`den wait`), summarise its state (`den status`) and run
//! throwaway instances in the background (`den sandbox …`).
//!
//! Output is short plain text by default (one line per fact, `@ref` handles
//! for widgets) so an agent spends few tokens reading it; `--json` where it
//! helps. Failures exit non-zero with one line saying why.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Subcommand;
use denchat_core::api::{ApiClient, InstanceInfo, list_instances};
use denchat_core::config::{self, AppConfig};
use denchat_core::mods;
use denchat_core::paths::Paths;
use serde_json::{Value, json};

async fn client(paths: &Paths) -> anyhow::Result<(ApiClient, InstanceInfo)> {
    ApiClient::discover(paths, Some("client")).await
}

// ----- den ui ------------------------------------------------------------

#[derive(Subcommand)]
pub enum UiCmd {
    /// Widgets on screen, one per line: `@ref role "text" flags`, grouped by panel.
    Tree {
        /// Only this panel (e.g. core.header, my.mod/panel, or just my.mod).
        #[arg(long)]
        panel: Option<String>,
        /// Include widgets scrolled out of view, and don't shorten text.
        #[arg(long)]
        all: bool,
        /// Add each widget's rectangle: [x,y wxh] in points.
        #[arg(long)]
        rects: bool,
        #[arg(long)]
        json: bool,
    },
    /// Click a widget: `@12`, its text (`Roll`), `role:text` (`button:Join`), or a point `X,Y`.
    Click {
        target: String,
        /// Right-click (context menus).
        #[arg(long)]
        right: bool,
        #[arg(long)]
        double: bool,
        /// Click with the pointer at the widget's centre, not its accessibility action.
        #[arg(long)]
        pointer: bool,
        #[command(flatten)]
        pick: Pick,
    },
    /// Type text into the focused text box, or into `--into <target>` first.
    Type {
        text: String,
        #[arg(long)]
        into: Option<String>,
        /// Press Enter afterwards (sends a message in the composer).
        #[arg(long)]
        enter: bool,
        #[command(flatten)]
        pick: Pick,
    },
    /// Press keys or chords in order: `den ui key ctrl+a Delete`, `Escape`, `cmd+k`.
    Key {
        #[arg(required = true)]
        keys: Vec<String>,
        #[command(flatten)]
        pick: Pick,
    },
    /// Scroll by DY points (positive scrolls down), over `--over <target>` or where the pointer is.
    Scroll {
        #[arg(allow_hyphen_values = true)]
        dy: f32,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dx: f32,
        #[arg(long)]
        over: Option<String>,
        #[command(flatten)]
        pick: Pick,
    },
    /// Move the pointer over a widget or point (shows tooltips and hover states).
    Hover {
        target: String,
        #[command(flatten)]
        pick: Pick,
    },
    /// Wait until a widget matching TARGET is on screen (or, with --gone, isn't). Exit 1 on timeout.
    Wait {
        target: String,
        #[arg(long)]
        gone: bool,
        /// Seconds.
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        #[arg(long)]
        panel: Option<String>,
    },
    /// Save a PNG of the window, a panel (--panel) or a widget (--target). Prints `path WxH renderer`.
    Shot {
        path: Option<PathBuf>,
        #[arg(long)]
        panel: Option<String>,
        #[arg(long)]
        target: Option<String>,
        /// Shrink, e.g. 0.5: smaller images are cheaper to look at.
        #[arg(long)]
        scale: Option<f32>,
        /// Draw it on the CPU even if the window is visible.
        #[arg(long)]
        cpu: bool,
    },
    /// Show the window's state; flags change it first.
    Window {
        /// WxH in points, e.g. 800x600.
        #[arg(long)]
        size: Option<String>,
        /// X,Y of the outer window.
        #[arg(long)]
        pos: Option<String>,
        #[arg(long)]
        minimize: bool,
        /// Un-minimise and un-maximise.
        #[arg(long)]
        restore: bool,
        #[arg(long)]
        maximize: bool,
        #[arg(long)]
        fullscreen: bool,
        #[arg(long)]
        no_fullscreen: bool,
        /// Bring it to the front (takes focus from the user: avoid on their desktop).
        #[arg(long)]
        focus: bool,
        #[arg(long)]
        show: bool,
        #[arg(long)]
        hide: bool,
        #[arg(long)]
        json: bool,
    },
    /// UI zoom, like display scaling: 1.0 = 100 %, 1.5, 2.
    Scale { scale: f32 },
    /// Text the app last copied (Copy invite link, copy message…).
    Clip,
    /// Close the app like its close button (hands the room on first).
    Quit,
}

/// Options shared by the input commands.
#[derive(clap::Args, Clone, Default)]
pub struct Pick {
    /// Only match widgets in this panel.
    #[arg(long)]
    panel: Option<String>,
    /// Pick the Nth match (0-based) when several widgets match.
    #[arg(long)]
    index: Option<usize>,
    /// Afterwards, wait (up to 10 s) until a widget matching this is on screen.
    #[arg(long, value_name = "TARGET")]
    wait: Option<String>,
}

/// `X,Y` → a point.
fn point(target: &str) -> Option<(f64, f64)> {
    let (x, y) = target.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn target_params(target: &str, pick: &Pick) -> Value {
    let mut p = match point(target) {
        Some((x, y)) => json!({"x": x, "y": y}),
        None => json!({"target": target}),
    };
    if let Some(panel) = &pick.panel {
        p["panel"] = json!(panel);
    }
    if let Some(i) = pick.index {
        p["index"] = json!(i);
    }
    p
}

async fn wait_after(c: &mut ApiClient, pick: &Pick) -> anyhow::Result<()> {
    if let Some(t) = &pick.wait {
        let r = c.call("ui.wait", json!({"target": t, "timeout_ms": 10_000, "panel": pick.panel})).await?;
        println!("found {} after {} ms", r["found"].as_str().unwrap_or(t), r["after_ms"]);
    }
    Ok(())
}

pub async fn ui(cmd: UiCmd, paths: &Paths) -> anyhow::Result<()> {
    let (mut c, _) = client(paths).await?;
    match cmd {
        UiCmd::Tree { panel, all, rects, json } => {
            let format = if json { "json" } else { "text" };
            let r = c.call("ui.tree", json!({"panel": panel, "all": all, "rects": rects, "format": format})).await?;
            if json {
                println!("{r}");
            } else {
                println!("{}", r["text"].as_str().unwrap_or_default());
            }
        }
        UiCmd::Click { target, right, double, pointer, pick } => {
            let mut p = target_params(&target, &pick);
            p["double"] = json!(double);
            p["pointer"] = json!(pointer);
            if right {
                p["button"] = json!("right");
            }
            let r = c.call("ui.click", p).await?;
            println!("clicked {} ({})", r["clicked"].as_str().unwrap_or(&target), r["via"].as_str().unwrap_or("?"));
            wait_after(&mut c, &pick).await?;
        }
        UiCmd::Type { text, into, enter, pick } => {
            let mut p = json!({"text": text, "enter": enter, "panel": pick.panel, "index": pick.index});
            if let Some(t) = into {
                p["target"] = json!(t);
            }
            let r = c.call("ui.type", p).await?;
            println!("typed {} chars into {}", r["typed"], r["into"].as_str().unwrap_or("?"));
            wait_after(&mut c, &pick).await?;
        }
        UiCmd::Key { keys, pick } => {
            for k in &keys {
                c.call("ui.key", json!({"key": k})).await?;
            }
            println!("pressed {}", keys.join(" "));
            wait_after(&mut c, &pick).await?;
        }
        UiCmd::Scroll { dy, dx, over, pick } => {
            let mut p = match &over {
                Some(t) => target_params(t, &pick),
                None => json!({}),
            };
            p["dy"] = json!(dy);
            p["dx"] = json!(dx);
            c.call("ui.scroll", p).await?;
            println!("scrolled {dx},{dy}{}", over.map(|o| format!(" over {o}")).unwrap_or_default());
            wait_after(&mut c, &pick).await?;
        }
        UiCmd::Hover { target, pick } => {
            let r = c.call("ui.hover", target_params(&target, &pick)).await?;
            println!("hovering {}", r["hovering"].as_str().unwrap_or(&target));
            wait_after(&mut c, &pick).await?;
        }
        UiCmd::Wait { target, gone, timeout, panel } => {
            let ms = timeout_secs(timeout)?.as_millis() as u64;
            let r = c.call("ui.wait", json!({"target": target, "gone": gone, "timeout_ms": ms, "panel": panel})).await?;
            match r["found"].as_str() {
                Some(f) => println!("found {f} after {} ms", r["after_ms"]),
                None => println!("gone after {} ms", r["after_ms"]),
            }
        }
        UiCmd::Shot { path, panel, target, scale, cpu } => {
            let path = path.map(|p| std::path::absolute(&p).unwrap_or(p));
            let mut p = json!({"panel": panel, "target": target, "scale": scale});
            if let Some(path) = path {
                p["path"] = json!(path);
            }
            if cpu {
                p["renderer"] = json!("cpu");
            }
            let r = c.call("ui.screenshot", p).await?;
            println!("{} {}x{} {}", r["path"].as_str().unwrap_or("?"), r["width"], r["height"], r["renderer"].as_str().unwrap_or(""));
        }
        UiCmd::Window { size, pos, minimize, restore, maximize, fullscreen, no_fullscreen, focus, show, hide, json } => {
            let mut p = serde_json::Map::new();
            if let Some(s) = size {
                p.insert("size".into(), json!(s));
            }
            if let Some(s) = pos {
                p.insert("pos".into(), json!(s));
            }
            if minimize {
                p.insert("minimized".into(), json!(true));
            }
            if restore {
                p.insert("minimized".into(), json!(false));
                p.insert("maximized".into(), json!(false));
            }
            if maximize {
                p.insert("maximized".into(), json!(true));
            }
            if fullscreen || no_fullscreen {
                p.insert("fullscreen".into(), json!(fullscreen));
            }
            if focus {
                p.insert("focus".into(), json!(true));
            }
            if show || hide {
                p.insert("visible".into(), json!(show));
            }
            let r = c.call("ui.window", Value::Object(p)).await?;
            if json {
                println!("{r}");
            } else {
                println!("{}", window_line(&r));
            }
        }
        UiCmd::Scale { scale } => {
            c.call("ui.scale", json!({"scale": scale})).await?;
            println!("zoom {scale}");
        }
        UiCmd::Clip => {
            let r = c.call("ui.clipboard", json!({})).await?;
            match r["copied"]["text"].as_str() {
                Some(t) => println!("{t}"),
                None => bail!("the app hasn't copied anything"),
            }
        }
        UiCmd::Quit => {
            c.call("app.quit", json!({})).await?;
            println!("quitting");
        }
    }
    Ok(())
}

fn window_line(w: &Value) -> String {
    let pair = |v: &Value| match v.as_array() {
        Some(a) if a.len() == 2 => format!("{:.0}x{:.0}", a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0)),
        _ => "?".into(),
    };
    let mut flags = Vec::new();
    for (k, word) in [
        ("focused", "focused"),
        ("minimized", "minimized"),
        ("maximized", "maximized"),
        ("fullscreen", "fullscreen"),
        ("occluded", "covered"),
    ] {
        if w[k].as_bool() == Some(true) {
            flags.push(word.to_owned());
        }
    }
    if let Some(z) = w["zoom"].as_f64().filter(|z| (z - 1.0).abs() > 0.01) {
        flags.push(format!("zoom {z:.2}"));
    }
    let pos = match w["pos"].as_array() {
        Some(a) if a.len() == 2 => format!(" at {:.0},{:.0}", a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0)),
        _ => String::new(),
    };
    format!(
        "{} {}{pos} {} ppp {}{}",
        w["renderer"].as_str().unwrap_or("?"),
        pair(&w["size"]),
        w["title"],
        w["pixels_per_point"],
        if flags.is_empty() { String::new() } else { format!(" · {}", flags.join(", ")) }
    )
}

// ----- den wait ----------------------------------------------------------

#[derive(clap::Args)]
pub struct WaitArgs {
    /// Event names or patterns: message.created, voice.*, ui.action.
    #[arg(required = true)]
    events: Vec<String>,
    /// Only events whose data has this value: `message.text=hi` (substring match, dotted path into `data`).
    #[arg(long = "where", value_name = "PATH=VALUE")]
    filters: Vec<String>,
    /// Stop after this many matching events.
    #[arg(long, default_value_t = 1)]
    count: usize,
    /// Seconds; exit 1 if nothing matched by then.
    #[arg(long, default_value_t = 10.0)]
    timeout: f64,
    #[arg(long)]
    server: bool,
    /// A command to run once listening, so nothing it causes is missed:
    /// `den -S a wait message.created -- den -S b ui type hi --enter`.
    #[arg(last = true, value_name = "COMMAND")]
    then: Vec<String>,
}

/// `--timeout` seconds as a duration (an hour at most).
fn timeout_secs(s: f64) -> anyhow::Result<Duration> {
    if !(s.is_finite() && s >= 0.0) {
        bail!("--timeout wants seconds (0 or more), got {s}");
    }
    Ok(Duration::from_secs_f64(s.min(3600.0)))
}

fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, k| match v {
        Value::Array(a) => a.get(k.parse::<usize>().ok()?),
        _ => v.get(k),
    })
}

fn matches_filters(data: &Value, filters: &[(String, String)]) -> bool {
    filters.iter().all(|(path, want)| {
        lookup(data, path).is_some_and(|v| match v {
            Value::String(s) => s.contains(want.as_str()),
            other => *other == serde_json::from_str::<Value>(want).unwrap_or(Value::Null),
        })
    })
}

pub async fn wait(args: WaitArgs, paths: &Paths) -> anyhow::Result<()> {
    let filters: Vec<(String, String)> = args
        .filters
        .iter()
        .map(|f| {
            f.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())).with_context(|| format!("--where wants PATH=VALUE, got {f:?}"))
        })
        .collect::<anyhow::Result<_>>()?;
    let limit = timeout_secs(args.timeout)?;
    let kind = if args.server { "server" } else { "client" };
    let (c, _) = ApiClient::discover(paths, Some(kind)).await?;
    let mut stream = c.subscribe(&args.events).await?;
    if let Some((prog, rest)) = args.then.split_first() {
        // `den` is this den, even when it isn't on PATH (`cargo run`).
        let exe = if prog == "den" { std::env::current_exe()? } else { PathBuf::from(prog) };
        let status = tokio::process::Command::new(&exe).args(rest).status().await.with_context(|| format!("running {prog}"))?;
        if !status.success() {
            bail!("{prog} failed ({status})");
        }
    }
    let deadline = Instant::now() + limit;
    let mut seen = 0;
    while seen < args.count {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = match tokio::time::timeout(left, stream.next()).await {
            Err(_) => bail!("no matching {} within {} s", args.events.join("/"), args.timeout),
            Ok(r) => r?.context("the app went away")?,
        };
        if ev.event == "api.lagged" || !matches_filters(&ev.data, &filters) {
            continue;
        }
        println!("{} {}", ev.event, ev.data);
        seen += 1;
    }
    Ok(())
}

// ----- den status --------------------------------------------------------

/// A few lines instead of `state.get`'s page of JSON.
pub async fn status(paths: &Paths, as_json: bool) -> anyhow::Result<()> {
    let (mut c, info) = client(paths).await?;
    let s = c.call("state.get", json!({})).await?;
    if as_json {
        println!("{s}");
        return Ok(());
    }
    let str_of = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let frontend = s["frontend"].as_str().unwrap_or("no window");
    let control = if s["agent_control"] == false { " · agent control off" } else { "" };
    println!("den pid {} · {frontend} · nick {}{control}", info.pid, str_of(&s["nick"]));
    let conn = &s["conn"];
    let state = conn["state"].as_str().unwrap_or("idle");
    match &s["server"] {
        Value::Null => println!("lobby ({state}{})", conn["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default()),
        server => {
            let mut bits = vec![state.to_owned()];
            if let Some(v) = s["via"].as_str() {
                bits.push(format!("via {v}"));
            }
            if let Some(l) = s["latency_ms"].as_u64() {
                bits.push(format!("{l} ms"));
            }
            if s["is_host"].as_bool() == Some(true) {
                bits.push("you host".into());
            }
            println!("room {} ({}) · {}", server["name"], str_of(&server["room_id"]).chars().take(8).collect::<String>(), bits.join(" · "));
        }
    }
    let focus = s["focus"].as_str().unwrap_or_default();
    let channels: Vec<String> = s["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|ch| {
            let id = ch["id"].as_str().unwrap_or_default();
            let unread = s["unread"][id].as_u64().filter(|n| *n > 0).map(|n| format!("({n})")).unwrap_or_default();
            match ch["kind"].as_str() {
                Some("voice") => format!("voice:{}{unread}", ch["name"].as_str().unwrap_or(id)),
                _ => format!("#{}{}{unread}", ch["name"].as_str().unwrap_or(id), if id == focus { "*" } else { "" }),
            }
        })
        .collect();
    if !channels.is_empty() {
        println!("channels {}", channels.join(" "));
    }
    let me = s["me"].as_u64();
    let members: Vec<String> = s["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let mut n = m["nick"].as_str().unwrap_or("?").to_owned();
            if m["id"].as_u64() == me {
                n.push_str(" (you)");
            }
            if let Some(v) = m["voice"].as_str() {
                n.push_str(&format!(" [voice:{v}]"));
            }
            n
        })
        .collect();
    if !members.is_empty() {
        println!("members {}", members.join(", "));
    }
    let v = &s["voice"];
    if let Some(ch) = v["channel"].as_str() {
        let mut bits = vec![format!("voice {ch}")];
        for (k, word) in [("muted", "muted"), ("deafened", "deafened")] {
            if v[k].as_bool() == Some(true) {
                bits.push(word.into());
            }
        }
        if let Some(e) = v["error"].as_str() {
            bits.push(format!("error: {e}"));
        }
        println!("{}", bits.join(" · "));
    }
    let list = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
    let (m, p) = (list(&s["mods"]), list(&s["panels"]));
    if !m.is_empty() || !p.is_empty() {
        println!("mods {} · panels {}", if m.is_empty() { "-" } else { &m }, if p.is_empty() { "-" } else { &p });
    }
    Ok(())
}

// ----- den sandbox -------------------------------------------------------

/// Where sandboxes live: `$DEN_SANDBOXES`, else `<tmp>/den-sandboxes`.
pub fn sandbox_root() -> PathBuf {
    std::env::var_os("DEN_SANDBOXES").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("den-sandboxes"))
}

pub fn sandbox_home(name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("sandbox names are letters, digits, - and _ (got {name:?})");
    }
    Ok(sandbox_root().join(name))
}

#[derive(Subcommand)]
pub enum SandboxCmd {
    /// Start a Den in the background with its own home: no window, test audio
    /// devices, copies to a file instead of your clipboard. It hosts a room unless --join/--lobby.
    Start {
        /// Name to refer to it by (`den -S <name> …`).
        #[arg(default_value = "a")]
        name: String,
        /// Link this mod directory into it (repeatable).
        #[arg(long = "mod", value_name = "DIR")]
        mods: Vec<PathBuf>,
        /// Join another sandbox's room (its name) or an invite link, instead of hosting.
        #[arg(long)]
        join: Option<String>,
        /// Stay in the lobby.
        #[arg(long, conflicts_with = "join")]
        lobby: bool,
        /// Room name when hosting.
        #[arg(long)]
        room: Option<String>,
        /// Start from a copy of your own config (theme, layout, themes/, layouts/, linked mods).
        #[arg(long)]
        copy_config: bool,
        #[arg(long)]
        theme: Option<String>,
        #[arg(long)]
        layout: Option<String>,
        /// WxH in points (default 1100x720).
        #[arg(long)]
        size: Option<String>,
        /// Show a real window instead of running headless.
        #[arg(long)]
        window: bool,
        /// Delete what an earlier sandbox of this name left behind first.
        #[arg(long)]
        fresh: bool,
        /// Test microphone: test:silence (default), test:sine:<hz> or test:file:<wav>.
        #[arg(long, value_name = "DEVICE")]
        input: Option<String>,
        /// Test speakers: test:null (default) or test:capture:<wav>.
        #[arg(long, value_name = "DEVICE")]
        output: Option<String>,
    },
    /// Sandboxes and whether they run.
    List,
    /// Stop sandboxes (asks them to quit, like closing the window).
    Stop {
        names: Vec<String>,
        #[arg(long)]
        all: bool,
        /// Also delete their folders.
        #[arg(long)]
        rm: bool,
    },
    /// Print a sandbox's home folder (`DEN_HOME=$(den sandbox path a)`).
    Path { name: String },
}

/// A live client instance in `home`, if any.
async fn live_instance(home: &Path) -> Option<(ApiClient, InstanceInfo)> {
    let paths = Paths::under(home);
    for (_, info) in list_instances(&paths) {
        if info.kind != "client" {
            continue;
        }
        if let Ok(Ok(mut c)) = tokio::time::timeout(Duration::from_secs(1), ApiClient::connect(&info.addr, &info.token)).await
            && matches!(tokio::time::timeout(Duration::from_secs(2), c.call("api.ping", json!({}))).await, Ok(Ok(_)))
        {
            return Some((c, info));
        }
    }
    None
}

pub async fn sandbox(cmd: SandboxCmd, user_paths: &Paths) -> anyhow::Result<()> {
    match cmd {
        SandboxCmd::Path { name } => println!("{}", sandbox_home(&name)?.display()),
        SandboxCmd::List => {
            let root = sandbox_root();
            let mut names: Vec<String> = std::fs::read_dir(&root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            if names.is_empty() {
                println!("no sandboxes (start one: den sandbox start)");
            }
            for name in names {
                let home = root.join(&name);
                match live_instance(&home).await {
                    Some((mut c, info)) => {
                        let s = tokio::time::timeout(Duration::from_secs(3), c.call("state.get", json!({})))
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or_default();
                        let room = s["server"]["name"].as_str().map(|r| format!("in {r:?}")).unwrap_or_else(|| "in the lobby".into());
                        let host = if s["is_host"].as_bool() == Some(true) { ", hosting" } else { "" };
                        println!("{name:<10} pid {:<7} {} {room}{host}", info.pid, s["frontend"].as_str().unwrap_or("?"));
                    }
                    None if profile_free(&home) => println!("{name:<10} stopped"),
                    None => println!("{name:<10} running, not answering (`den sandbox stop {name}` kills it)"),
                }
            }
        }
        SandboxCmd::Stop { names, all, rm } => {
            let names = if all {
                std::fs::read_dir(sandbox_root())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            } else if names.is_empty() {
                bail!("name the sandboxes to stop, or --all");
            } else {
                names
            };
            let mut stuck = Vec::new();
            for name in names {
                let home = sandbox_home(&name)?;
                if !stop_sandbox(&name, &home).await {
                    stuck.push(name);
                    continue;
                }
                if rm && home.exists() {
                    remove_dir(&home).await?;
                    println!("removed {}", home.display());
                }
            }
            if !stuck.is_empty() {
                bail!("couldn't stop {}; left in place", stuck.join(", "));
            }
        }
        SandboxCmd::Start { name, mods, join, lobby, room, copy_config, theme, layout, size, window, fresh, input, output } => {
            let test_device = |d: &Option<String>| d.as_deref().is_some_and(|d| d.to_ascii_lowercase().starts_with("test:"));
            for (flag, dev) in [("--input", &input), ("--output", &output)] {
                if dev.is_some() && !test_device(dev) {
                    bail!("{flag} takes a test device (test:…); a sandbox never opens real audio devices");
                }
            }
            let home = sandbox_home(&name)?;
            if let Some((_, info)) = live_instance(&home).await {
                bail!("sandbox {name} is already running (pid {}); `den sandbox stop {name}` first, or pick another name", info.pid);
            }
            if home.exists() && !wait_until(Duration::from_secs(5), || async { profile_free(&home) }).await {
                bail!("sandbox {name} is still shutting down; try again in a moment");
            }
            if fresh && home.exists() {
                std::fs::remove_dir_all(&home)?;
            }
            let paths = Paths::under(&home);
            std::fs::create_dir_all(&paths.config)?;
            if copy_config {
                copy_user_config(user_paths, &paths)?;
            }
            // Its own config: never the user's devices, never the user's clipboard.
            let mut cfg: AppConfig = config::load_or_default(&paths.config_file())?;
            cfg.nick = Some(name.clone());
            // A copied config may have these off; a sandbox is for driving.
            cfg.modules.api = true;
            cfg.modules.agent_control = true;
            if input.is_some() {
                cfg.voice.input_device = input;
            } else if !test_device(&cfg.voice.input_device) {
                cfg.voice.input_device = Some("test:silence".into());
            }
            if output.is_some() {
                cfg.voice.output_device = output;
            } else if !test_device(&cfg.voice.output_device) {
                cfg.voice.output_device = Some("test:null".into());
            }
            if let Some(t) = theme {
                cfg.ui.theme = t;
            }
            if let Some(l) = layout {
                cfg.ui.layout = l;
            }
            config::save(&paths.config_file(), &cfg)?;
            let mut mf = mods::load_mods_file(&paths);
            for m in &mods {
                let dir = m.canonicalize().with_context(|| format!("mod {} not found", m.display()))?;
                let manifest = mods::read_manifest(&dir)?;
                if !mf.linked.contains(&dir) {
                    mf.linked.push(dir);
                }
                mf.disabled.retain(|d| d != &manifest.id);
            }
            config::save(&paths.mods_file(), &mf)?;

            let invite = match &join {
                Some(j) if !j.contains("://") => {
                    let other = sandbox_home(j)?;
                    let (mut c, _) = live_instance(&other).await.with_context(|| format!("sandbox {j} isn't running"))?;
                    let r = c.call("session.invite", json!({})).await.with_context(|| format!("sandbox {j} isn't in a room"))?;
                    r["invite"].as_str().map(str::to_owned)
                }
                other => other.clone(),
            };
            let mut args: Vec<String> = vec!["--home".into(), home.display().to_string()];
            match (&invite, lobby) {
                (Some(link), _) => args.extend(["join".into(), link.clone()]),
                (None, true) => args.push("gui".into()),
                (None, false) => args.extend(["host".into(), "--name".into(), room.clone().unwrap_or_else(|| format!("{name}'s room"))]),
            }
            let log_path = home.join("den.log");
            let log = std::fs::File::create(&log_path)?;
            let mut cmd = std::process::Command::new(std::env::current_exe()?);
            cmd.args(&args)
                .env_remove("DEN_API")
                .env_remove("DEN_API_TOKEN")
                .env_remove("DEN_SANDBOX")
                .env_remove("DEN_SIZE")
                .env("DEN_HOME", &home)
                .env("DEN_CLIPBOARD", home.join("clipboard.txt"))
                // Never the user's microphone or speakers, whatever is picked in the app.
                .env("DEN_TEST_AUDIO", "1")
                .env("DEN_WINDOW", if window { "normal" } else { "headless" })
                .env("DEN_LOG", std::env::var("DEN_LOG").unwrap_or_else(|_| "info,iroh=warn".into()))
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log);
            if let Some(s) = &size {
                cmd.env("DEN_SIZE", s);
            }
            detach(&mut cmd);
            let mut child = cmd.spawn().context("starting den")?;
            let pid = child.id();
            let (mut api, state) = match until_ready(&name, &home, &log_path, &mut child, lobby).await {
                Ok(ready) => ready,
                Err(e) => {
                    // Don't leave a half-started sandbox behind.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            };
            let shown = if window { "window" } else { "headless" };
            match state["server"]["name"].as_str() {
                Some(room) => {
                    let what = if state["is_host"].as_bool() == Some(true) { "hosting" } else { "joined" };
                    println!("sandbox {name}: pid {pid}, {shown}, {what} {room:?}");
                }
                None => println!("sandbox {name}: pid {pid}, {shown}, in the lobby"),
            }
            println!("  home   {}", home.display());
            if let Ok(r) = api.call("session.invite", json!({})).await
                && let Some(link) = r["invite"].as_str()
            {
                println!("  invite {link}");
            }
            println!("  drive  den -S {name} ui tree · den -S {name} status · stop: den sandbox stop {name}");
        }
    }
    Ok(())
}

/// Theme/layout choices, `themes/`, `layouts/` and linked mods from the user's
/// own setup, so a mod can be tried against it without touching the real app.
fn copy_user_config(from: &Paths, to: &Paths) -> anyhow::Result<()> {
    if from.config == to.config {
        return Ok(());
    }
    if from.config_file().exists() {
        std::fs::copy(from.config_file(), to.config_file())?;
    }
    for (src, dst) in [(from.themes_dir(), to.themes_dir()), (from.layouts_dir(), to.layouts_dir())] {
        if src.is_dir() {
            std::fs::create_dir_all(&dst)?;
            for e in std::fs::read_dir(&src)?.flatten() {
                if e.path().is_file() {
                    std::fs::copy(e.path(), dst.join(e.file_name()))?;
                }
            }
        }
    }
    // Mods stay where they are: link the user's mod folders and linked mods.
    let theirs = mods::load_mods_file(from);
    let mut ours = mods::load_mods_file(to);
    let installed =
        std::fs::read_dir(from.mods_dir()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("den-mod.toml").exists());
    for dir in theirs.linked.into_iter().chain(installed) {
        if !ours.linked.contains(&dir) {
            ours.linked.push(dir);
        }
    }
    ours.disabled = theirs.disabled;
    config::save(&to.mods_file(), &ours)
}

/// Wait until a sandbox that was just started is up and (unless it stays in
/// the lobby) in its room, for up to 30 s.
async fn until_ready(
    name: &str,
    home: &Path,
    log_path: &Path,
    child: &mut std::process::Child,
    lobby: bool,
) -> anyhow::Result<(ApiClient, Value)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let pid = child.id();
    let mut api = loop {
        if let Some(status) = child.try_wait()? {
            bail!("sandbox {name} exited ({status}) while starting:\n{}", tail(log_path, 15));
        }
        if Instant::now() > deadline {
            bail!("sandbox {name} didn't come up in 30 s:\n{}", tail(log_path, 15));
        }
        if let Some((c, info)) = live_instance(home).await
            && info.pid == pid
        {
            break c;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    };
    if lobby {
        return Ok((api, Value::Null));
    }
    loop {
        let state = tokio::time::timeout(Duration::from_secs(5), api.call("state.get", json!({})))
            .await
            .context("the sandbox stopped answering")??;
        if state["conn"]["state"] == "connected" && !state["server"].is_null() {
            return Ok((api, state));
        }
        if state["conn"]["state"] == "failed" {
            bail!("sandbox {name} couldn't get into the room: {}", state["conn"]["error"].as_str().unwrap_or("?"));
        }
        if Instant::now() > deadline {
            bail!("sandbox {name} started but isn't in the room after 30 s (state {}); see {}", state["conn"], log_path.display());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Stop the sandbox in `home`: ask it to quit, else kill it. True once no
/// process uses its folder any more.
async fn stop_sandbox(name: &str, home: &Path) -> bool {
    let gone = |pid: u32| move || async move { profile_free(home) && !alive(pid) };
    match live_instance(home).await {
        Some((mut c, info)) => {
            let _ = tokio::time::timeout(Duration::from_secs(5), c.call("app.quit", json!({}))).await;
            // Gone once it lets go of its profile (after handing the room on) and exits.
            let quit = wait_until(Duration::from_secs(10), gone(info.pid)).await;
            if !quit {
                kill(info.pid);
            }
            let stopped = quit || wait_until(Duration::from_secs(3), gone(info.pid)).await;
            println!("stopped {name} (pid {}){}", info.pid, if quit { "" } else { ", forcibly" });
            stopped
        }
        None if profile_free(home) => {
            println!("{name} wasn't running");
            true
        }
        None => {
            // It holds its profile but doesn't answer: stop it by the pid in its instance file.
            let newest = list_instances(&Paths::under(home)).into_iter().map(|(_, i)| i).max_by_key(|i| i.started);
            let Some(info) = newest else {
                println!("{name} is in use by a process that doesn't answer and left no pid");
                return false;
            };
            kill(info.pid);
            let stopped = wait_until(Duration::from_secs(5), gone(info.pid)).await;
            println!("stopped {name} (pid {}), forcibly: it wasn't answering", info.pid);
            stopped
        }
    }
}

/// Remove a stopped sandbox's folder. Windows can hold a file open for a
/// moment after its process exits, so try for a few seconds.
async fn remove_dir(dir: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if Instant::now() > deadline => return Err(e).with_context(|| format!("removing {}", dir.display())),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// The process is still running.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = i32::try_from(pid) else { return false };
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        // SAFETY: plain Win32 calls on a handle we open and close here.
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return false;
            }
            let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
            CloseHandle(h);
            running
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// No process holds this home's profile.
fn profile_free(home: &Path) -> bool {
    denchat_core::net::lock_key(&Paths::under(home).data.join("device.key")).is_ok()
}

async fn wait_until<F, Fut>(limit: Duration, mut done: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Keep the sandbox running when the shell (or agent) that started it goes
/// away or gets Ctrl-C, and don't give it a console window.
fn detach(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
}

fn kill(pid: u32) {
    #[cfg(unix)]
    let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_filters() {
        let data = json!({"message": {"text": "alice rolled 6", "author": {"nick": "alice"}}, "n": 3});
        let f = |k: &str, v: &str| vec![(k.to_owned(), v.to_owned())];
        assert!(matches_filters(&data, &f("message.text", "rolled")));
        assert!(matches_filters(&data, &f("message.author.nick", "alice")));
        assert!(matches_filters(&data, &f("n", "3")));
        assert!(!matches_filters(&data, &f("message.text", "bob")));
        assert!(!matches_filters(&data, &f("missing", "x")));
    }

    #[test]
    fn points_and_names() {
        assert_eq!(point("120,40"), Some((120.0, 40.0)));
        assert_eq!(point("Roll"), None);
        assert!(sandbox_home("a-1").is_ok());
        assert!(sandbox_home("../x").is_err());
    }
}
