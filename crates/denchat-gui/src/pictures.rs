//! Pictures on screen: profile and room pictures as textures, and choosing a
//! new one.
//!
//! The session holds the files ([`Session::picture`]); this keeps a texture
//! per picture id. A picture that hasn't arrived yet is tried again when the
//! session's state changes (it bumps when a file arrives).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use denchat_core::session::Session;
use egui::{Color32, ColorImage, TextureHandle, TextureId, TextureOptions};

enum Entry {
    Ready(TextureHandle),
    /// Not here yet: look again once the state has changed (and a moment has passed).
    Missing {
        revision: u64,
        at: Instant,
    },
    /// The file isn't an image we can show.
    Broken,
}

/// Textures for picture ids, made on first use.
#[derive(Default)]
pub struct Pictures {
    map: RefCell<HashMap<String, Entry>>,
}

impl Pictures {
    /// The texture for picture `id`, if we have the file.
    pub fn texture(&self, ctx: &egui::Context, session: &Session, revision: u64, id: Option<&str>) -> Option<TextureId> {
        let id = id?;
        let mut map = self.map.borrow_mut();
        match map.get(id) {
            Some(Entry::Ready(t)) => return Some(t.id()),
            Some(Entry::Broken) => return None,
            Some(Entry::Missing { revision: r, at }) if *r == revision || at.elapsed() < Duration::from_millis(300) => return None,
            _ => {}
        }
        let entry = match session.picture(id) {
            None => Entry::Missing { revision, at: Instant::now() },
            Some(bytes) => match denchat_core::pictures::decode(&bytes) {
                Some((w, h, rgba)) => {
                    let image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let options = TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear));
                    Entry::Ready(ctx.load_texture(format!("picture-{id}"), image, options))
                }
                None => {
                    tracing::warn!("picture {id} isn't an image Den can show");
                    Entry::Broken
                }
            },
        };
        let tex = match &entry {
            Entry::Ready(t) => Some(t.id()),
            _ => None,
        };
        map.insert(id.to_owned(), entry);
        tex
    }
}

/// Paint a picture as a circle of diameter `size` centred at `center`.
pub fn paint_round(painter: &egui::Painter, center: egui::Pos2, size: f32, tex: TextureId) {
    paint_rounded(painter, egui::Rect::from_center_size(center, egui::Vec2::splat(size)), size / 2.0, tex);
}

/// Paint a picture filling `rect` with corners of `radius`.
pub fn paint_rounded(painter: &egui::Painter, rect: egui::Rect, radius: f32, tex: TextureId) {
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    let corner = egui::CornerRadius::same(radius.round().clamp(0.0, 255.0) as u8);
    painter.add(egui::epaint::RectShape::filled(rect, corner, Color32::WHITE).with_texture(tex, uv));
}

// ----- choosing one ------------------------------------------------------------

/// Whose picture is being chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureFor {
    /// Your profile picture.
    Me,
    /// The room you're in.
    Room,
}

impl PictureFor {
    pub fn title(self) -> &'static str {
        match self {
            PictureFor::Me => "Choose your picture",
            PictureFor::Room => "Choose the room's picture",
        }
    }

    /// Use the image at `path` (`None`: remove the picture).
    pub fn apply(self, session: &Session, path: Option<PathBuf>) {
        match self {
            PictureFor::Me => session.set_picture_file(path),
            PictureFor::Room => session.set_room_picture_file(path),
        }
    }
}

/// Image files the picker offers.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Ask for an image with the system's file dialog and use it; cancelling
/// changes nothing. Call on the UI thread: the dialog is made here (macOS
/// wants that) and waited for on the session's runtime, so the window keeps
/// drawing meanwhile.
pub fn pick_with_dialog(session: &Session, target: PictureFor) {
    let dialog = rfd::AsyncFileDialog::new().set_title(target.title()).add_filter("Images", IMAGE_EXTENSIONS).pick_file();
    let session = session.clone();
    session.runtime().clone().spawn(async move {
        if let Some(file) = dialog.await {
            target.apply(&session, Some(file.path().to_path_buf()));
        }
    });
}

/// The in-app picker (no window: sandboxes, agents): type a path.
#[derive(Default)]
pub struct PathPrompt {
    pub open: Option<PictureFor>,
    pub path: String,
}

impl PathPrompt {
    /// Draw it while open. Returns the choice once made.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<(PictureFor, PathBuf)> {
        let target = self.open?;
        let mut chosen = None;
        let mut close = false;
        egui::Modal::new(egui::Id::unique("den-picture-path")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading(target.title());
            let label = ui.label("Image file (PNG, JPEG, GIF, WebP or BMP)");
            let hint = if cfg!(windows) { "C:\\pictures\\me.png" } else { "~/pictures/me.png" };
            let edit = egui::TextEdit::singleline(&mut self.path).hint_text(hint).desired_width(f32::INFINITY);
            let edit = ui.add(edit).labelled_by(label.id);
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if !edit.has_focus() && self.path.is_empty() {
                edit.request_focus();
            }
            ui.horizontal(|ui| {
                let ok = !self.path.trim().is_empty();
                if ui.add_enabled(ok, egui::Button::new("Use picture")).clicked() || (enter && ok) {
                    chosen = Some((target, PathBuf::from(self.path.trim().trim_matches('"'))));
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        });
        if chosen.is_some() || close {
            self.open = None;
            self.path.clear();
        }
        chosen
    }
}
