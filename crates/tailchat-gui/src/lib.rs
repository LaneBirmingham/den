//! tailchat's desktop shell (egui).
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
//! [`tailchat_core::session::Session`] and never needs this crate.

pub mod app;
pub mod assets;
pub mod catalog;
pub mod icons;
pub mod kit;
pub mod layout;
pub mod panels;
pub mod theme;

use tailchat_core::session::Session;

pub use panels::{Panel, PanelCx, Registry};

/// Open the window with the built-in panels.
pub fn run(session: Session) -> anyhow::Result<()> {
    run_with(session, Registry::with_builtins())
}

/// Open the window with a custom panel registry (add, replace or remove panels).
pub fn run_with(session: Session, registry: Registry) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("tailchat")
            .with_app_id("tailchat")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([480.0, 360.0]),
        // Window size, floating panels and hidden panels live with the rest of
        // this instance's data, so `--home` instances don't share them.
        persistence_path: Some(session.paths().data.join("gui")),
        ..Default::default()
    };
    eframe::run_native("tailchat", options, Box::new(move |cc| Ok(Box::new(app::TailchatApp::new(cc, session, registry)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
