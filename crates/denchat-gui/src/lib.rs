//! Den's desktop shell (egui).
//!
//! The shell is deliberately thin: it reads a **layout** (which panels go in
//! which slot), a **theme** (palette + shape + fonts) and renders registered
//! **panels**. All three are swappable without touching Rust:
//!
//! * themes and layouts are TOML files (built-in, from mods, or in the user's
//!   config dir) and hot-reload on save;
//! * every built-in panel (`core.*`) can be moved or removed by the layout;
//! * mods add panels by pushing declarative widgets through the control API.
//!
//! A different frontend (TUI, web, an agent-written one) talks to the same
//! [`denchat_core::session::Session`] and never needs this crate.

pub mod agent;
pub mod app;
pub mod assets;
pub mod catalog;
pub mod headless;
pub mod icons;
pub mod kit;
pub mod layout;
pub mod panels;
pub mod pictures;
pub mod soft;
pub mod theme;

use denchat_core::session::Session;

pub use panels::{Panel, PanelCx, Registry};

/// How to show the app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// A normal window.
    #[default]
    Window,
    /// No window at all: for agents and tests (`DEN_WINDOW=headless`).
    Headless,
}

impl Mode {
    /// `DEN_WINDOW`: `headless` (or `hidden`, `none`) runs without a window.
    pub fn from_env() -> Mode {
        match std::env::var("DEN_WINDOW").unwrap_or_default().to_lowercase().as_str() {
            "headless" | "hidden" | "none" | "off" => Mode::Headless,
            _ => Mode::Window,
        }
    }
}

const DEFAULT_SIZE: [f32; 2] = [1100.0, 720.0];

/// `DEN_SIZE=WxH`: the window's starting size (in points).
pub fn size_from_env() -> Option<[f32; 2]> {
    let v = std::env::var("DEN_SIZE").ok()?;
    let (w, h) = v.split_once(['x', 'X', ','])?;
    let (w, h): (f32, f32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w >= 200.0 && h >= 150.0).then_some([w, h])
}

/// Open the window with the built-in panels.
pub fn run(session: Session) -> anyhow::Result<()> {
    run_with(session, Registry::with_builtins())
}

/// Open the window with a custom panel registry (add, replace or remove panels).
pub fn run_with(session: Session, registry: Registry) -> anyhow::Result<()> {
    run_mode(session, registry, Mode::from_env())
}

/// Run the app in a window or headless.
pub fn run_mode(session: Session, registry: Registry, mode: Mode) -> anyhow::Result<()> {
    let size = size_from_env();
    if mode == Mode::Headless {
        return headless::run(session, registry, size.unwrap_or(DEFAULT_SIZE).into());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Den")
            .with_app_id("den")
            .with_inner_size(size.unwrap_or(DEFAULT_SIZE))
            .with_min_inner_size([480.0, 360.0]),
        // Window size, floating panels and hidden panels live with the rest of
        // this instance's data, so `--home` instances don't share them.
        persistence_path: Some(session.paths().data.join("gui")),
        // An explicit size wins over the one remembered from last time.
        persist_window: size.is_none(),
        // Hover and clicks should land on the very next refresh: vsync with a
        // one-frame queue (eframe's default, spelled out so it stays that way).
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig::LOW_LATENCY),
        ..Default::default()
    };
    eframe::run_native("den", options, Box::new(move |cc| Ok(Box::new(app::DenApp::new(cc, session, registry)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
