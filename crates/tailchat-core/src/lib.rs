//! UI-agnostic engine for tailchat.
//!
//! Frontends (the egui shell, the terminal client, the headless server
//! console, or anything an agent writes later) only talk to the types in this
//! crate. Nothing in here knows how the interface looks.
//!
//! * [`server`] — the room server. Runs inside the app when a peer hosts, or
//!   headless via `tailchat serve`. Same code either way.
//! * [`session`] — one user's view of a room: connection, state, commands,
//!   slash commands, voice link. Frontends render [`session::SessionState`]
//!   and call [`session::Session`] methods.
//! * [`server_node`] — the headless server wrapper (server + mods + API).
//! * [`api`] — the local control API (JSON lines over loopback TCP) that mods,
//!   scripts and agents use. It exposes the same surface as `Session`.
//! * [`mods`] — manifest-declared extensions: slash commands, event hooks,
//!   panels, startup daemons, themes and layouts.
//! * [`ui`] — the declarative widget store mod panels write into.
//! * [`tailscale`] — tailnet discovery and identity via the local `tailscale` CLI.

pub mod api;
pub mod config;
pub mod events;
pub mod mods;
pub mod net;
pub mod paths;
pub mod server;
pub mod server_node;
pub mod session;
pub mod tailscale;
pub mod ui;
#[cfg(feature = "voice")]
pub mod voice;

#[cfg(feature = "voice")]
pub use tailchat_audio as audio;
pub use tailchat_proto as proto;

/// Version string reported to peers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether this build can do voice.
pub const fn voice_supported() -> bool {
    cfg!(feature = "voice")
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
