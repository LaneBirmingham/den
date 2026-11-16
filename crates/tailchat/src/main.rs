//! `tailchat` — one binary, several frontends over the same core.
//!
//! * `tailchat` / `tailchat gui`        the desktop app (egui shell)
//! * `tailchat host` / `join <invite>`  the app, hosting or joining straight away
//! * `tailchat chat <invite>`           a line-based terminal client
//! * `tailchat serve`                   an always-on host for a room, with an admin console
//! * `tailchat relay`                   a relay on its own, for rooms hosted from apps
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
#[command(name = "tailchat", version, about = "Minimal, moddable peer-to-peer voice + text chat")]
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
    /// Host a room from this machine and join it (peer-to-peer mode). Anyone
    /// in it can host it later from their copy.
    Host {
        #[command(flatten)]
        room: HostArgs,
        /// Use the terminal client instead of the GUI.
        #[arg(long)]
        cli: bool,
    },
    /// Join a room by invite link, e.g. `tailchat join 'tailchat://…?room=…&invite=…'`.
    Join {
        addr: String,
        /// Use the terminal client instead of the GUI.
        #[arg(long)]
        cli: bool,
    },
    /// Terminal chat client (text; `/voice` works when built with voice).
    Chat {
        /// Invite link. Omit with --host to host one.
        addr: Option<String>,
        #[command(flatten)]
        room: HostArgs,
        /// Host a room instead of joining one.
        #[arg(long)]
        host: bool,
    },
    /// Run an always-on host for a room, with an admin console.
    Serve(serve::ServeArgs),
    /// Run a relay on its own, for rooms hosted from apps: whoever hosts
    /// sets `[net] relays = ["http://<this machine>:<port>"]`.
    Relay {
        /// TCP port (forward it on your router).
        #[arg(long, default_value_t = 3340)]
        port: u16,
        /// all (default), localhost, or an IP.
        #[arg(long, default_value = "all")]
        bind: String,
    },
    /// Which of your saved rooms someone is hosting right now.
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
    /// Name for a new room.
    #[arg(long)]
    name: Option<String>,
    /// Open a saved room instead (id or name): join whoever hosts it now, or
    /// host it from your copy.
    #[arg(long, conflicts_with = "name")]
    room: Option<String>,
    /// A new room without an invite key: anyone who has its address can join.
    #[arg(long, conflicts_with = "room")]
    open: bool,
}

impl HostArgs {
    fn start(&self, nick: &str) -> Start {
        if let Some(room) = &self.room {
            return Start::Open(room.clone());
        }
        Start::Host(HostOptions { name: self.name.clone().unwrap_or_else(|| format!("{nick}'s room")), open: self.open })
    }
}

/// What the GUI/terminal client should do right after starting.
pub enum Start {
    Lobby,
    Host(HostOptions),
    Open(String),
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
            run_app(&rt, &paths, cli.nick, room.start(&nick), term)
        }
        Some(Cmd::Join { addr, cli: term }) => run_app(&rt, &paths, cli.nick, Start::Join(addr), term),
        Some(Cmd::Chat { addr, room, host }) => {
            let start = match (addr, host) {
                (_, true) => {
                    let cfg = load_config(&paths, cli.nick.clone())?;
                    let nick = cfg.nick.clone().unwrap_or_else(config::os_username);
                    room.start(&nick)
                }
                (Some(a), false) => Start::Join(a),
                (None, false) => anyhow::bail!("give an invite link, or --host to host one"),
            };
            run_app(&rt, &paths, cli.nick, start, true)
        }
        Some(Cmd::Serve(args)) => rt.block_on(serve::run(args, paths)),
        Some(Cmd::Relay { port, bind }) => rt.block_on(cli::relay(&paths, port, &bind)),
        Some(Cmd::Discover { json }) => rt.block_on(cli::discover(&paths, json)),
        Some(Cmd::Api(args)) => rt.block_on(cli::api(args, &paths)),
        Some(Cmd::Mods { cmd }) => rt.block_on(cli::mods(cmd, &paths)),
        Some(Cmd::Config { cmd }) => cli::config(cmd, &paths),
        Some(Cmd::Devices) => cli::devices(),
        Some(Cmd::VoiceTest { seconds, codec }) => cli::voice_test(&paths, seconds, &codec),
    }
}

fn init_logging(verbose: u8, quiet_default: bool) {
    // iroh reports network weather (NAT changes, relay hops) as warnings:
    // only show those when asked for detail.
    let default = match (verbose, quiet_default) {
        (0, true) => "warn,iroh=error",
        (0, false) => "info,iroh=error",
        (1, _) => "info",
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
        close_on_signal(rt, &session);
        let res = {
            let _guard = rt.enter();
            tailchat_gui::run(session.clone())
        };
        // Hand the room on if we host it, then stop.
        tracing::info!("window closed");
        rt.block_on(session.close());
        return res;
    }
    #[cfg(not(feature = "gui"))]
    if !terminal {
        eprintln!("this build has no GUI; using the terminal client (build with --features gui for the app)");
    }
    let session = start_session(rt, paths, nick, "tailchat-cli")?;
    if matches!(start, Start::Lobby) {
        anyhow::bail!("this build has no GUI: use `tailchat chat <invite>` or `tailchat chat --host`");
    }
    apply_start(&session, start);
    let res = rt.block_on(term::run(session.clone()));
    rt.block_on(session.close());
    res
}

/// SIGTERM / Ctrl-C while the window is open: close like the window's close
/// button would (hand the room on if we host it, ~6 s at most), then exit.
/// Done here rather than through the window, which may draw no frames (and so
/// handle no events) while the display is off.
#[cfg(feature = "gui")]
fn close_on_signal(rt: &tokio::runtime::Runtime, session: &Session) {
    let session = session.clone();
    rt.spawn(async move {
        serve::wait_for_signal().await;
        tracing::info!("signal received; closing");
        session.close().await;
        std::process::exit(0);
    });
}

fn apply_start(session: &Session, start: Start) {
    match start {
        Start::Lobby => {}
        Start::Host(opts) => session.host(opts),
        Start::Open(room) => session.open_room(&room),
        Start::Join(addr) => session.join(addr),
    }
}
