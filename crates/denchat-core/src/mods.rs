//! Mods: manifest-declared extensions in any language.
//!
//! Modelled on herdr plugins. A mod is a directory with a
//! `den-mod.toml` manifest and whatever executables it needs. Den
//! owns the host surface — discovery, enable/disable, slash-command routing,
//! event hooks, panel slots, process lifetime and environment — and the mod
//! owns its implementation. Mods call back in through the control API
//! (`$DEN_API` + `$DEN_API_TOKEN`, or simply `$DEN_BIN api …`).
//!
//! ```toml
//! id = "example.dice"
//! name = "Dice"
//! version = "0.1.0"
//!
//! [[commands]]              # /roll 20
//! name = "roll"
//! command = ["python3", "roll.py"]
//! output = "send"           # send | notice | none
//!
//! [[hooks]]                 # run on events (exact, prefix.*, or *)
//! on = "message.created"
//! command = ["python3", "hook.py"]
//!
//! [[panels]]                # a panel any layout can place
//! id = "stats"
//! title = "Dice stats"
//! command = ["python3", "panel.py"]   # stdout = widgets JSON or text
//! interval_seconds = 10
//!
//! [[startup]]               # one-shot, or daemon = true to keep running
//! command = ["python3", "bot.py"]
//! daemon = true
//!
//! themes = ["themes/neon.toml"]
//! layouts = ["layouts/stream.toml"]
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use denchat_proto::{MemberId, NoticeLevel};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, broadcast, watch};

use crate::events::{self, Event};
use crate::paths::Paths;
use crate::ui::{UiStore, parse_widgets};

pub const MANIFEST_FILE: &str = "den-mod.toml";
/// The manifest's name before the app was renamed; still read when a mod has
/// no `den-mod.toml`.
const OLD_MANIFEST_FILE: &str = "tailchat-mod.toml";

/// `dir`'s manifest: `den-mod.toml`, else an old `tailchat-mod.toml`.
fn manifest_path(dir: &Path) -> Option<PathBuf> {
    [MANIFEST_FILE, OLD_MANIFEST_FILE].into_iter().map(|f| dir.join(f)).find(|p| p.is_file())
}
const OUTPUT_CAP: usize = 64 * 1024;
const DEFAULT_TIMEOUT: u64 = 30;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub min_den_version: Option<String>,
    /// `linux`, `macos`, `windows`. Omitted = everywhere.
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    /// `client` (the app / terminal client), `server` (headless). Omitted = both.
    #[serde(default)]
    pub runs_on: Option<Vec<String>>,
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    #[serde(default)]
    pub panels: Vec<PanelSpec>,
    #[serde(default)]
    pub startup: Vec<StartupSpec>,
    /// Theme TOML files shipped by the mod (relative to the mod root).
    #[serde(default)]
    pub themes: Vec<String>,
    /// Layout TOML files shipped by the mod.
    #[serde(default)]
    pub layouts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// Post stdout to the channel as a chat message.
    Send,
    /// Show stdout locally only.
    #[default]
    Notice,
    /// Discard stdout.
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommandSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub usage: Option<String>,
    pub command: Vec<String>,
    #[serde(default)]
    pub output: Output,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookSpec {
    pub on: String,
    pub command: Vec<String>,
    #[serde(default = "output_none")]
    pub output: Output,
    /// Also fire for messages this node sent itself (beware loops).
    #[serde(default)]
    pub include_self: bool,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

fn output_none() -> Output {
    Output::None
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PanelSpec {
    pub id: String,
    pub title: String,
    /// Suggested slot when a layout has a `mods` placeholder.
    #[serde(default)]
    pub slot: Option<String>,
    /// Command whose stdout becomes the panel content.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Re-run the command this often. Omitted = once at startup.
    #[serde(default)]
    pub interval_seconds: Option<u64>,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StartupSpec {
    pub command: Vec<String>,
    /// Keep the process running for the life of Den.
    #[serde(default)]
    pub daemon: bool,
    #[serde(default)]
    pub platforms: Option<Vec<String>>,
}

/// `mods.toml`: user choices about mods.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModsFile {
    /// Extra mod directories (like `herdr plugin link`).
    pub linked: Vec<PathBuf>,
    /// Mod ids that are installed but switched off.
    pub disabled: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadedMod {
    pub manifest: Manifest,
    pub root: PathBuf,
    pub enabled: bool,
    /// `user`, `linked` or `env`.
    pub source: String,
    /// Why it is not active on this node/platform, if so.
    pub inactive_reason: Option<String>,
}

impl LoadedMod {
    pub fn active(&self) -> bool {
        self.enabled && self.inactive_reason.is_none()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ModProblem {
    pub path: PathBuf,
    pub error: String,
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn platform_ok(p: &Option<Vec<String>>) -> bool {
    p.as_ref().is_none_or(|list| list.iter().any(|x| x.eq_ignore_ascii_case(current_platform())))
}

fn valid_id(s: &str, allow_dot: bool) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' || (allow_dot && c == '.'))
}

fn version_tuple(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn validate(m: &Manifest) -> anyhow::Result<()> {
    if !valid_id(&m.id, true) {
        bail!("invalid mod id {:?} (letters, digits, . - _ :)", m.id);
    }
    if let Some(min) = &m.min_den_version
        && version_tuple(min) > version_tuple(crate::VERSION)
    {
        bail!("needs Den {min} or newer (this is {})", crate::VERSION);
    }
    let mut seen = HashSet::new();
    for c in &m.commands {
        if !valid_id(&c.name, false) {
            bail!("invalid command name {:?}", c.name);
        }
        if !seen.insert(c.name.clone()) {
            bail!("duplicate command {:?}", c.name);
        }
        if c.command.is_empty() {
            bail!("command {:?} has an empty argv", c.name);
        }
    }
    let mut seen = HashSet::new();
    for p in &m.panels {
        if !valid_id(&p.id, false) {
            bail!("invalid panel id {:?}", p.id);
        }
        if !seen.insert(p.id.clone()) {
            bail!("duplicate panel {:?}", p.id);
        }
        if p.command.as_ref().is_some_and(Vec::is_empty) {
            bail!("panel {:?} has an empty argv", p.id);
        }
    }
    for h in &m.hooks {
        if h.on.is_empty() || h.command.is_empty() {
            bail!("hooks need `on` and a non-empty `command`");
        }
    }
    for s in &m.startup {
        if s.command.is_empty() {
            bail!("startup entries need a non-empty `command`");
        }
    }
    Ok(())
}

pub fn read_manifest(dir: &Path) -> anyhow::Result<Manifest> {
    let path = manifest_path(dir).unwrap_or_else(|| dir.join(MANIFEST_FILE));
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let m: Manifest = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    validate(&m)?;
    Ok(m)
}

/// Directories that directly contain a manifest, or whose children do.
fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    if manifest_path(root).is_some() {
        return vec![root.to_path_buf()];
    }
    let mut out: Vec<PathBuf> =
        std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| manifest_path(p).is_some()).collect();
    out.sort();
    out
}

pub fn load_mods_file(paths: &Paths) -> ModsFile {
    crate::config::load_or_default(&paths.mods_file()).unwrap_or_else(|e| {
        tracing::warn!("{e:#}");
        ModsFile::default()
    })
}

/// Find every mod visible to this user. `node` is `client` or `server`.
pub fn discover(paths: &Paths, node: &str) -> (Vec<LoadedMod>, Vec<ModProblem>) {
    let file = load_mods_file(paths);
    let mut sources: Vec<(PathBuf, &str)> = vec![(paths.mods_dir(), "user")];
    sources.extend(file.linked.iter().map(|p| (p.clone(), "linked")));
    if let Some(extra) = std::env::var_os("DEN_MODS_PATH") {
        sources.extend(std::env::split_paths(&extra).map(|p| (p, "env")));
    }
    let mut mods: Vec<LoadedMod> = Vec::new();
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    for (root, source) in sources {
        for dir in candidate_dirs(&root) {
            match read_manifest(&dir) {
                Ok(manifest) => {
                    if !seen.insert(manifest.id.clone()) {
                        problems.push(ModProblem { path: dir, error: format!("duplicate mod id {}", manifest.id) });
                        continue;
                    }
                    let inactive_reason = if !platform_ok(&manifest.platforms) {
                        Some(format!("not for {}", current_platform()))
                    } else if manifest.runs_on.as_ref().is_some_and(|r| !r.iter().any(|x| x == node)) {
                        Some(format!("does not run on the {node}"))
                    } else {
                        None
                    };
                    mods.push(LoadedMod {
                        enabled: !file.disabled.contains(&manifest.id),
                        root: dir.canonicalize().unwrap_or(dir),
                        source: source.into(),
                        inactive_reason,
                        manifest,
                    });
                }
                Err(e) => problems.push(ModProblem { path: dir, error: format!("{e:#}") }),
            }
        }
    }
    (mods, problems)
}

/// What a mod runtime needs from the node it runs in.
pub trait ModHost: Send + Sync + 'static {
    /// `client` or `server`.
    fn node_kind(&self) -> &'static str;
    /// Post a chat message (as this user on a client, as the server on a server).
    fn send_text(&self, channel: Option<String>, text: String);
    fn notice(&self, level: NoticeLevel, text: String);
    /// Extra environment for mod processes (API address/token, nick, channel…).
    fn env(&self) -> Vec<(String, String)>;
    fn self_id(&self) -> Option<MemberId>;
    fn ui(&self) -> &UiStore;
}

/// Everything about one process launch.
pub struct Launch<'a> {
    pub m: &'a LoadedMod,
    pub argv: &'a [String],
    pub args: &'a [String],
    pub env: Vec<(String, String)>,
    pub stdin: Option<String>,
    pub timeout: Duration,
}

pub struct ModRuntime {
    mods: Vec<LoadedMod>,
    problems: Vec<ModProblem>,
    paths: Paths,
    daemons: Mutex<Vec<Child>>,
    limiter: Arc<Semaphore>,
    stopped: watch::Sender<bool>,
}

impl ModRuntime {
    pub fn load(paths: &Paths, node: &str, enabled: bool) -> ModRuntime {
        let (mods, problems) = if enabled { discover(paths, node) } else { (Vec::new(), Vec::new()) };
        for p in &problems {
            tracing::warn!("mod at {}: {}", p.path.display(), p.error);
        }
        ModRuntime {
            mods,
            problems,
            paths: paths.clone(),
            daemons: Mutex::new(Vec::new()),
            limiter: Arc::new(Semaphore::new(8)),
            stopped: watch::channel(false).0,
        }
    }

    pub fn empty(paths: &Paths) -> ModRuntime {
        ModRuntime::load(paths, "client", false)
    }

    pub fn mods(&self) -> &[LoadedMod] {
        &self.mods
    }

    pub fn problems(&self) -> &[ModProblem] {
        &self.problems
    }

    pub fn active(&self) -> impl Iterator<Item = &LoadedMod> {
        self.mods.iter().filter(|m| m.active())
    }

    /// Resolve `/name` or `/mod.id:name`.
    pub fn find_command(&self, name: &str) -> Option<(&LoadedMod, &CommandSpec)> {
        let (mod_id, cmd) = match name.rsplit_once(':') {
            Some((m, c)) if self.mods.iter().any(|x| x.manifest.id == m) => (Some(m), c),
            _ => (None, name),
        };
        self.active()
            .filter(|m| mod_id.is_none_or(|id| m.manifest.id == id))
            .find_map(|m| m.manifest.commands.iter().find(|c| c.name == cmd && platform_ok(&c.platforms)).map(|c| (m, c)))
    }

    pub fn commands(&self) -> Vec<(String, CommandSpec)> {
        self.active()
            .flat_map(|m| m.manifest.commands.iter().filter(|c| platform_ok(&c.platforms)).map(|c| (m.manifest.id.clone(), c.clone())))
            .collect()
    }

    /// Theme and layout files shipped by active mods.
    pub fn asset_files(&self, kind: &str) -> Vec<PathBuf> {
        self.active()
            .flat_map(|m| {
                let list = if kind == "themes" { &m.manifest.themes } else { &m.manifest.layouts };
                list.iter().map(|rel| m.root.join(rel)).collect::<Vec<_>>()
            })
            .filter(|p| p.is_file())
            .collect()
    }

    fn base_env(&self, host: &dyn ModHost, m: &LoadedMod) -> Vec<(String, String)> {
        let state = self.paths.mod_state_dir(&m.manifest.id);
        let config = self.paths.mod_config_dir(&m.manifest.id);
        let _ = std::fs::create_dir_all(&state);
        let _ = std::fs::create_dir_all(&config);
        let mut env = vec![
            ("DEN_ENV".into(), "1".into()),
            ("DEN_NODE".into(), host.node_kind().into()),
            ("DEN_MOD_ID".into(), m.manifest.id.clone()),
            ("DEN_MOD_ROOT".into(), m.root.display().to_string()),
            ("DEN_MOD_STATE_DIR".into(), state.display().to_string()),
            ("DEN_MOD_CONFIG_DIR".into(), config.display().to_string()),
            ("DEN_VERSION".into(), crate::VERSION.into()),
        ];
        if let Ok(exe) = std::env::current_exe() {
            env.push(("DEN_BIN".into(), exe.display().to_string()));
        }
        env.extend(host.env());
        env
    }

    /// Spawn a mod process and collect its stdout.
    pub async fn launch(&self, l: Launch<'_>) -> anyhow::Result<String> {
        let _permit = self.limiter.clone().acquire_owned().await?;
        let mut cmd = build_command(l.m, l.argv, l.args);
        cmd.envs(l.env)
            .stdin(if l.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hide_console(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("starting {:?} for mod {}", l.argv, l.m.manifest.id))?;
        if let (Some(input), Some(mut pipe)) = (l.stdin, child.stdin.take()) {
            let _ = pipe.write_all(input.as_bytes()).await;
        }
        let out = tokio::time::timeout(l.timeout, child.wait_with_output())
            .await
            .with_context(|| format!("mod {} timed out after {:?}", l.m.manifest.id, l.timeout))??;
        let stderr = String::from_utf8_lossy(&out.stderr);
        for line in stderr.lines().filter(|l| !l.trim().is_empty()).take(50) {
            tracing::info!(target: "den::mod", "[{}] {line}", l.m.manifest.id);
        }
        let mut stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if stdout.len() > OUTPUT_CAP {
            let mut cut = OUTPUT_CAP;
            while !stdout.is_char_boundary(cut) {
                cut -= 1;
            }
            stdout.truncate(cut);
        }
        if !out.status.success() {
            let tail = stderr.lines().last().unwrap_or("").trim().to_owned();
            bail!(
                "mod {} exited with {}{}",
                l.m.manifest.id,
                out.status,
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            );
        }
        Ok(stdout)
    }

    /// Run a mod slash command and route its output.
    pub async fn run_command(
        &self,
        host: Arc<dyn ModHost>,
        name: &str,
        args: Vec<String>,
        channel: Option<String>,
        context: Value,
    ) -> anyhow::Result<()> {
        let (m, spec) = self.find_command(name).with_context(|| format!("no mod command /{name}"))?;
        let mut env = self.base_env(host.as_ref(), m);
        env.push(("DEN_COMMAND".into(), spec.name.clone()));
        env.push(("DEN_ARGS".into(), args.join(" ")));
        env.push(("DEN_CONTEXT_JSON".into(), context.to_string()));
        if let Some(c) = &channel {
            env.push(("DEN_CHANNEL".into(), c.clone()));
        }
        let out = self
            .launch(Launch {
                m,
                argv: &spec.command,
                args: &args,
                env,
                stdin: None,
                timeout: Duration::from_secs(spec.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT)),
            })
            .await?;
        route_output(host.as_ref(), spec.output, channel, &out);
        Ok(())
    }

    /// Start hooks, panel pollers and startup commands.
    pub fn start(self: &Arc<Self>, host: Arc<dyn ModHost>, events: broadcast::Receiver<Event>) {
        for m in self.active() {
            for p in &m.manifest.panels {
                if platform_ok(&p.platforms) {
                    let id = format!("{}/{}", m.manifest.id, p.id);
                    host.ui().declare_panel(&id, &p.title, p.slot.clone(), Some(m.manifest.id.clone()));
                }
            }
        }
        let rt = self.clone();
        let h = host.clone();
        tokio::spawn(async move { rt.hook_loop(h, events).await });

        for (mi, m) in self.mods.iter().enumerate().filter(|(_, m)| m.active()) {
            for (pi, p) in m.manifest.panels.iter().enumerate() {
                if p.command.is_some() && platform_ok(&p.platforms) {
                    let rt = self.clone();
                    let h = host.clone();
                    tokio::spawn(async move { rt.panel_loop(h, mi, pi).await });
                }
            }
            for s in m.manifest.startup.iter().filter(|s| platform_ok(&s.platforms)) {
                if s.daemon {
                    self.spawn_daemon(host.as_ref(), m, &s.command);
                } else {
                    let rt = self.clone();
                    let h = host.clone();
                    let argv = s.command.clone();
                    tokio::spawn(async move {
                        let m = &rt.mods[mi];
                        let mut env = rt.base_env(h.as_ref(), m);
                        env.push(("DEN_EVENT".into(), "startup".into()));
                        let res = rt.launch(Launch { m, argv: &argv, args: &[], env, stdin: None, timeout: Duration::from_secs(60) }).await;
                        if let Err(e) = res {
                            tracing::warn!("{e:#}");
                        }
                    });
                }
            }
        }
    }

    fn spawn_daemon(&self, host: &dyn ModHost, m: &LoadedMod, argv: &[String]) {
        let mut cmd = build_command(m, argv, &[]);
        let mut env = self.base_env(host, m);
        env.push(("DEN_EVENT".into(), "startup".into()));
        cmd.envs(env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true);
        hide_console(&mut cmd);
        match cmd.spawn() {
            Ok(child) => {
                tracing::info!("mod {} daemon started: {argv:?}", m.manifest.id);
                self.daemons.lock().push(child);
            }
            Err(e) => tracing::warn!("mod {} daemon {argv:?} failed to start: {e}", m.manifest.id),
        }
    }

    /// Stop daemons (they are also killed when the runtime drops).
    pub fn stop(&self) {
        self.stopped.send_replace(true);
        for mut c in self.daemons.lock().drain(..) {
            let _ = c.start_kill();
        }
    }

    async fn hook_loop(self: Arc<Self>, host: Arc<dyn ModHost>, mut events: broadcast::Receiver<Event>) {
        let mut stopped = self.stopped.subscribe();
        loop {
            let next = tokio::select! {
                ev = events.recv() => ev,
                _ = stopped.wait_for(|s| *s) => break,
            };
            let ev = match next {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("mod hooks skipped {n} events (too slow)");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            for (mi, m) in self.mods.iter().enumerate().filter(|(_, m)| m.active()) {
                for (hi, hook) in m.manifest.hooks.iter().enumerate() {
                    if !platform_ok(&hook.platforms) || !events::matches(&hook.on, &ev.event) {
                        continue;
                    }
                    if !hook.include_self && is_own_message(host.as_ref(), &ev) {
                        continue;
                    }
                    let rt = self.clone();
                    let h = host.clone();
                    let ev = ev.clone();
                    tokio::spawn(async move {
                        let m = &rt.mods[mi];
                        let hook = &m.manifest.hooks[hi];
                        let json = serde_json::to_string(&ev).unwrap_or_default();
                        let channel = event_channel(&ev);
                        let mut env = rt.base_env(h.as_ref(), m);
                        env.push(("DEN_EVENT".into(), ev.event.clone()));
                        env.push(("DEN_EVENT_JSON".into(), json.clone()));
                        if let Some(c) = &channel {
                            env.push(("DEN_CHANNEL".into(), c.clone()));
                        }
                        let res = rt
                            .launch(Launch {
                                m,
                                argv: &hook.command,
                                args: &[],
                                env,
                                stdin: Some(json),
                                timeout: Duration::from_secs(hook.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT)),
                            })
                            .await;
                        match res {
                            Ok(out) => route_output(h.as_ref(), hook.output, channel, &out),
                            Err(e) => tracing::warn!("hook {} on {}: {e:#}", m.manifest.id, ev.event),
                        }
                    });
                }
            }
        }
    }

    async fn panel_loop(self: Arc<Self>, host: Arc<dyn ModHost>, mi: usize, pi: usize) {
        let m = &self.mods[mi];
        let spec = &m.manifest.panels[pi];
        let Some(argv) = &spec.command else { return };
        let id = format!("{}/{}", m.manifest.id, spec.id);
        let mut stopped = self.stopped.subscribe();
        while !*stopped.borrow() {
            let mut env = self.base_env(host.as_ref(), m);
            env.push(("DEN_PANEL_ID".into(), id.clone()));
            let res = self.launch(Launch { m, argv, args: &[], env, stdin: None, timeout: Duration::from_secs(DEFAULT_TIMEOUT) }).await;
            match res {
                Ok(out) => host.ui().set_panel(&id, None, parse_widgets(&out)),
                Err(e) => host.ui().set_panel(
                    &id,
                    None,
                    vec![crate::ui::Widget::Text { text: format!("{e:#}"), color: Some("danger".into()), mono: false, small: true }],
                ),
            }
            let Some(secs) = spec.interval_seconds.filter(|s| *s > 0) else { break };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = stopped.wait_for(|s| *s) => break,
            }
        }
    }
}

impl Drop for ModRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn build_command(m: &LoadedMod, argv: &[String], args: &[String]) -> Command {
    let program = &argv[0];
    // `./x`, `bin/x` and other relative paths resolve against the mod root.
    let resolved = if program.contains('/') || program.contains('\\') {
        let p = Path::new(program);
        if p.is_absolute() { p.to_path_buf() } else { m.root.join(p) }
    } else {
        PathBuf::from(program)
    };
    let mut cmd = Command::new(resolved);
    cmd.args(&argv[1..]).args(args).current_dir(&m.root);
    cmd
}

fn route_output(host: &dyn ModHost, output: Output, channel: Option<String>, out: &str) {
    let text = out.trim();
    if text.is_empty() {
        return;
    }
    match output {
        Output::Send => host.send_text(channel, text.to_owned()),
        Output::Notice => host.notice(NoticeLevel::Info, text.to_owned()),
        Output::None => {}
    }
}

fn event_channel(ev: &Event) -> Option<String> {
    ev.data["message"]["channel"].as_str().or_else(|| ev.data["channel"].as_str()).map(str::to_owned)
}

fn is_own_message(host: &dyn ModHost, ev: &Event) -> bool {
    if ev.event != events::names::MESSAGE_CREATED {
        return false;
    }
    let msg = &ev.data["message"];
    match host.node_kind() {
        "server" => msg["kind"] == "server",
        _ => match (host.self_id(), msg["author_id"].as_u64()) {
            (Some(me), Some(author)) => me as u64 == author,
            _ => false,
        },
    }
}

/// Write a starter mod to `dir`. Used by `den mods new`.
pub fn scaffold(dir: &Path, id: &str) -> anyhow::Result<()> {
    if !valid_id(id, true) {
        bail!("invalid mod id {id:?}");
    }
    if manifest_path(dir).is_some() {
        bail!("{} already has a manifest", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let name = id.rsplit('.').next().unwrap_or(id);
    std::fs::write(
        dir.join(MANIFEST_FILE),
        format!(
            r#"id = "{id}"
name = "{name}"
version = "0.1.0"
description = "Describe what this mod does"
# platforms = ["linux", "macos", "windows"]
# runs_on = ["client", "server"]

# /hello [name]  -> posts a greeting to the current channel
[[commands]]
name = "hello"
description = "Say hello"
usage = "/hello [name]"
command = ["python3", "mod.py", "hello"]
output = "send"

# A panel any layout can place with "{id}/status"
[[panels]]
id = "status"
title = "{name}"
command = ["python3", "mod.py", "panel"]
interval_seconds = 15

# React to chat. The event JSON arrives on stdin and in $DEN_EVENT_JSON.
# [[hooks]]
# on = "message.created"
# command = ["python3", "mod.py", "hook"]
# output = "none"
"#
        ),
    )?;
    std::fs::write(
        dir.join("mod.py"),
        r#"#!/usr/bin/env python3
"""Starter Den mod. Talks back to Den through `$DEN_BIN api`."""
import json, os, subprocess, sys

def api(method, **params):
    out = subprocess.run([os.environ["DEN_BIN"], "api", method, json.dumps(params)],
                         capture_output=True, text=True, check=True).stdout
    return json.loads(out) if out.strip() else None

mode = sys.argv[1] if len(sys.argv) > 1 else ""
if mode == "hello":
    who = " ".join(sys.argv[2:]) or "everyone"
    print(f"hello, {who}!")
elif mode == "panel":
    state = api("state.get")
    members = state.get("members", {}) if state else {}
    print(json.dumps([
        {"type": "heading", "text": "Room"},
        {"type": "kv", "key": "members", "value": str(len(members))},
        {"type": "button", "label": "Say hello", "command": "hello"},
    ]))
elif mode == "hook":
    event = json.load(sys.stdin)
"#,
    )?;
    Ok(())
}

/// Don't flash a console window for mod processes on Windows.
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_validation() {
        let good: Manifest = toml::from_str(
            r#"id = "a.b"
name = "x"
version = "0.1.0"
[[commands]]
name = "roll"
command = ["sh", "r.sh"]
"#,
        )
        .unwrap();
        validate(&good).unwrap();
        let mut bad = good.clone();
        bad.commands[0].name = "has.dot".into();
        assert!(validate(&bad).is_err());
        let mut future = good;
        future.min_den_version = Some("99.0.0".into());
        assert!(validate(&future).is_err());
    }

    #[test]
    fn scaffold_is_loadable() {
        let tmp = std::env::temp_dir().join(format!("den-scaffold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        scaffold(&tmp, "me.starter").unwrap();
        let m = read_manifest(&tmp).unwrap();
        assert_eq!(m.id, "me.starter");
        assert_eq!(m.commands[0].output, Output::Send);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
