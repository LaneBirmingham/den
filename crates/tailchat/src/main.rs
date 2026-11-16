//! `tailchat` — one binary, several frontends over the same core.
//!
//! * `tailchat` / `tailchat gui`        the desktop app (egui shell)
//! * `tailchat host` / `join <addr>`    the app, hosting or joining straight away
//! * `tailchat chat <addr>`             a line-based terminal client
//! * `tailchat serve`                   a headless, persistent room with an admin console
//! * `tailchat api <method> [params]`   drive any running instance (mods, scripts, agents)
//! * `tailchat discover | mods | config | devices | voice-test`

mod cli;
mod serve;
mod term;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tailchat_core::config::{self, AppConfig};
use tailchat_core::paths::Paths;
use tailchat_core::session::{HostOptions, Session, SessionOptions};

#[derive(Parser)]
#[command(name = "tailchat", version, about = "Minimal, moddable voice + text chat over Tailscale")]
struct Cli {
    /// Keep config, data and state under this directory (same as TAILCHAT_HOME).
    #[arg(long, global = true, env = "TAILCHAT_HOME", hide_env_values = true)]
    home: Option<PathBuf>,
    /// Override the nick for this run.
    #[arg(long, global = true)]
    nick: Option<String>,
    /// More logging (-v info, -vv debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the desktop app (default).
    Gui,
    /// Host a room from this machine and join it (peer-to-peer mode).
    Host {
        #[command(flatten)]
        room: HostArgs,
        /// Use the terminal client instead of the GUI.
        #[arg(long)]
        cli: bool,
    },
    /// Join a room by tailnet name or address, e.g. `tailchat join lanes-mac-mini`.
    Join {
        addr: String,
        /// Use the terminal client instead of the GUI.
        #[arg(long)]
        cli: bool,
    },
    /// Terminal chat client (text; `/voice` works when built with voice).
    Chat {
        /// Room address. Omit with --host to host one.
        addr: Option<String>,
        #[command(flatten)]
        room: HostArgs,
        /// Host a room instead of joining one.
        #[arg(long)]
        host: bool,
    },
    /// Run a persistent headless room with an admin console.
    Serve(serve::ServeArgs),
    /// Find rooms on your tailnet.
    Discover {
        #[arg(long)]
        json: bool,
    },
    /// Call the control API of a running tailchat (`tailchat api api.methods`).
    Api(cli::ApiArgs),
    /// Manage mods.
    Mods {
        #[command(subcommand)]
        cmd: cli::ModsCmd,
    },
    /// Show or initialise configuration files.
    Config {
        #[command(subcommand)]
        cmd: cli::ConfigCmd,
    },
    /// List audio devices.
    Devices,
    /// Mic → codec → speakers loopback test.
    VoiceTest {
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// opus or pcm16
        #[arg(long, default_value = "opus")]
        codec: String,
    },
}

#[derive(clap::Args, Clone)]
struct HostArgs {
    /// Room name.
    #[arg(long)]
    name: Option<String>,
    /// tailnet (default), localhost, all, or an IP.
    #[arg(long, default_value = "tailnet")]
    bind: String,
    #[arg(long)]
    port: Option<u16>,
    /// Keep history on disk between runs.
    #[arg(long)]
    persist: bool,
}

impl HostArgs {
    fn options(&self, cfg: &AppConfig, nick: &str) -> HostOptions {
        HostOptions {
            name: self.name.clone().unwrap_or_else(|| format!("{nick}'s room")),
            bind: self.bind.clone(),
            port: self.port.unwrap_or(cfg.port),
            persist: self.persist,
        }
    }
}

/// What the GUI/terminal client should do right after starting.
pub enum Start {
    Lobby,
    Host(HostOptions),
    Join(String),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(home) = &cli.home {
        // SAFETY: single-threaded at this point; nothing else reads the environment yet.
        unsafe { std::env::set_var("TAILCHAT_HOME", home) };
    }
    let quiet_default = matches!(cli.cmd, None | Some(Cmd::Gui | Cmd::Host { .. } | Cmd::Join { .. } | Cmd::Chat { .. } | Cmd::Api(_)));
    init_logging(cli.verbose, quiet_default);
    let paths = Paths::detect();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("tailchat-rt").build()?;

    match cli.cmd {
        None | Some(Cmd::Gui) => run_app(&rt, &paths, cli.nick, Start::Lobby, false),
        Some(Cmd::Host { room, cli: term }) => {
            let cfg = load_config(&paths, cli.nick.clone())?;
            let nick = cfg.nick.clone().unwrap_or_else(config::os_username);
            run_app(&rt, &paths, cli.nick, Start::Host(room.options(&cfg, &nick)), term)
        }
        Some(Cmd::Join { addr, cli: term }) => run_app(&rt, &paths, cli.nick, Start::Join(addr), term),
        Some(Cmd::Chat { addr, room, host }) => {
            let start = match (addr, host) {
                (_, true) => {
                    let cfg = load_config(&paths, cli.nick.clone())?;
                    let nick = cfg.nick.clone().unwrap_or_else(config::os_username);
                    Start::Host(room.options(&cfg, &nick))
                }
                (Some(a), false) => Start::Join(a),
                (None, false) => anyhow::bail!("give a room address, or --host to host one"),
            };
            run_app(&rt, &paths, cli.nick, start, true)
        }
        Some(Cmd::Serve(args)) => rt.block_on(serve::run(args, paths)),
        Some(Cmd::Discover { json }) => rt.block_on(cli::discover(&paths, json)),
        Some(Cmd::Api(args)) => rt.block_on(cli::api(args, &paths)),
        Some(Cmd::Mods { cmd }) => rt.block_on(cli::mods(cmd, &paths)),
        Some(Cmd::Config { cmd }) => cli::config(cmd, &paths),
        Some(Cmd::Devices) => cli::devices(),
        Some(Cmd::VoiceTest { seconds, codec }) => cli::voice_test(&paths, seconds, &codec),
    }
}

fn init_logging(verbose: u8, quiet_default: bool) {
    let default = match (verbose, quiet_default) {
        (0, true) => "warn",
        (0, false) | (1, _) => "info",
        _ => "debug",
    };
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("TAILCHAT_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

pub fn load_config(paths: &Paths, nick: Option<String>) -> anyhow::Result<AppConfig> {
    let mut cfg: AppConfig = config::load_or_default(&paths.config_file())?;
    if nick.is_some() {
        cfg.nick = nick;
    }
    Ok(cfg)
}

fn start_session(rt: &tokio::runtime::Runtime, paths: &Paths, nick: Option<String>, client: &str) -> anyhow::Result<Session> {
    let config = load_config(paths, nick)?;
    rt.block_on(Session::start(SessionOptions {
        paths: paths.clone(),
        config,
        client_name: format!("{client}/{}", tailchat_core::VERSION),
    }))
    .context("starting session")
}

fn run_app(rt: &tokio::runtime::Runtime, paths: &Paths, nick: Option<String>, start: Start, terminal: bool) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    if !terminal {
        let session = start_session(rt, paths, nick, "tailchat-gui")?;
        apply_start(&session, start);
        let _guard = rt.enter();
        let res = tailchat_gui::run(session.clone());
        session.shutdown();
        return res;
    }
    #[cfg(not(feature = "gui"))]
    if !terminal {
        eprintln!("this build has no GUI; using the terminal client (build with --features gui for the app)");
    }
    let session = start_session(rt, paths, nick, "tailchat-cli")?;
    if matches!(start, Start::Lobby) {
        anyhow::bail!("this build has no GUI: use `tailchat chat <addr>` or `tailchat chat --host`");
    }
    apply_start(&session, start);
    let res = rt.block_on(term::run(session.clone()));
    session.shutdown();
    res
}

fn apply_start(session: &Session, start: Start) {
    match start {
        Start::Lobby => {}
        Start::Host(opts) => session.host(opts),
        Start::Join(addr) => session.join(addr),
    }
}
