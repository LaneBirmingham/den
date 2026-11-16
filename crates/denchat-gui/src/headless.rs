//! The app with no window (`DEN_WINDOW=headless`).
//!
//! Runs the same [`DenApp`] frames on a plain egui context, as fast as egui
//! asks for repaints, and never opens a window, touches the GPU or takes
//! focus. Everything else is real: the session, mods, the control API.
//! Agents drive it with `den ui …` (the [`crate::agent`] plugin answers), and
//! screenshots are drawn on the CPU. It works on machines with no display.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use denchat_core::session::Session;
use egui::{Pos2, Rect, ViewportCommand, ViewportId, ViewportInfo, vec2};

use crate::agent::Kind;
use crate::app::DenApp;
use crate::panels::Registry;

/// Frames never come faster than this, whatever egui asks for.
const MIN_FRAME: Duration = Duration::from_millis(16);
/// …nor slower: timers in the app (polling for file changes) need a tick.
const MAX_IDLE: Duration = Duration::from_secs(1);

pub fn run(session: Session, registry: Registry, size: egui::Vec2) -> anyhow::Result<()> {
    let ctx = egui::Context::default();
    // Same fallback fonts as the windowed app, so screenshots match it.
    ctx.add_font_provider(Arc::new(egui_system_font_provider::SystemFontProvider::new()));
    let (wake_tx, wake) = mpsc::channel::<()>();
    ctx.set_request_repaint_callback(move |_| {
        let _ = wake_tx.send(());
    });
    let mut app = DenApp::create(&ctx, None, session.clone(), registry, Kind::Headless);
    let started = Instant::now();
    let mut window = Window { size, info: ViewportInfo::default() };
    window.sync();
    tracing::info!("running headless ({}x{}); drive it with `den ui …`", size.x, size.y);
    let mut next = Instant::now();
    loop {
        let now = Instant::now();
        if next > now {
            match wake.recv_timeout(next - now) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        while wake.try_recv().is_ok() {}
        let frame_start = Instant::now();
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, window.size)),
            max_texture_side: Some(8192),
            time: Some(started.elapsed().as_secs_f64()),
            focused: true,
            ..Default::default()
        };
        raw.viewports.insert(ViewportId::ROOT, window.info.clone());
        let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
        // Nothing to upload them to: the agent plugin kept its own copy.
        out.textures_delta.clear();
        let mut close = false;
        let mut delay = MAX_IDLE;
        if let Some(vo) = out.viewport_output.remove(&ViewportId::ROOT) {
            delay = vo.repaint_delay;
            for cmd in vo.commands {
                close |= window.apply(cmd);
            }
        }
        if close {
            break;
        }
        next = frame_start + delay.clamp(MIN_FRAME, MAX_IDLE);
    }
    ctx.on_exit();
    Ok(())
}

/// What a real window would report, kept up to date with the commands the app sends.
struct Window {
    size: egui::Vec2,
    info: ViewportInfo,
}

impl Window {
    fn sync(&mut self) {
        let rect = Rect::from_min_size(self.info.outer_rect.map_or(Pos2::ZERO, |r| r.min), self.size);
        let i = &mut self.info;
        i.inner_rect = Some(rect);
        i.outer_rect = Some(rect);
        i.native_pixels_per_point = Some(1.0);
        i.monitor_size = Some(vec2(1920.0, 1080.0));
        i.focused.get_or_insert(true);
        i.minimized.get_or_insert(false);
        i.maximized.get_or_insert(false);
        i.fullscreen.get_or_insert(false);
        i.occluded = Some(false);
        i.title.get_or_insert_with(|| "Den".into());
    }

    /// Returns true when the app asked to close.
    fn apply(&mut self, cmd: ViewportCommand) -> bool {
        match cmd {
            ViewportCommand::Close => return true,
            ViewportCommand::InnerSize(s) => self.size = s.max(vec2(200.0, 150.0)),
            ViewportCommand::OuterPosition(p) => self.info.outer_rect = Some(Rect::from_min_size(p, self.size)),
            ViewportCommand::Title(t) => self.info.title = Some(t),
            ViewportCommand::Minimized(b) => self.info.minimized = Some(b),
            ViewportCommand::Maximized(b) => self.info.maximized = Some(b),
            ViewportCommand::Fullscreen(b) => self.info.fullscreen = Some(b),
            ViewportCommand::Focus => self.info.focused = Some(true),
            _ => {}
        }
        self.sync();
        false
    }
}
