//! Small drawing kit shared by the built-in panels: type scale, avatars,
//! icon buttons, pills, badges and cards. Everything takes colours from the
//! [`Palette`], so every theme restyles it.

use std::sync::{Arc, LazyLock};

use egui::accesskit::Role;
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Galley, Margin, Painter, Pos2, Rect, Response, Sense, Shadow, Stroke, Ui,
    Vec2, pos2, vec2,
};

use crate::theme::{Palette, alpha, mix};

/// Inter Medium / SemiBold, registered by the theme (egui can't fake bold).
pub static MEDIUM: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("tc-medium".into()));
pub static SEMIBOLD: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("tc-semibold".into()));

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum W {
    Regular,
    Medium,
    Semibold,
}

pub fn font(size: f32, w: W) -> FontId {
    let family = match w {
        W::Regular => FontFamily::Proportional,
        W::Medium => MEDIUM.clone(),
        W::Semibold => SEMIBOLD.clone(),
    };
    FontId::new(size, family)
}

// ----- colour for people ---------------------------------------------------

fn nick_hue(nick: &str) -> f32 {
    let mut h = nick.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    // FNV's low bits barely move for short nicks; finalise so `kai`/`amy`/`bob` get distinct hues.
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    (h % 360) as f32
}

/// HSV in sRGB space (hue in degrees), like CSS.
fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let f = |n: f32| {
        let k = (n + h / 60.0) % 6.0;
        let x = v - v * s * k.min(4.0 - k).clamp(0.0, 1.0);
        (x * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(5.0), f(3.0), f(1.0))
}

/// Saturation and value of a colour.
fn sat_val(c: Color32) -> (f32, f32) {
    let [r, g, b] = [c.r(), c.g(), c.b()].map(|x| x as f32 / 255.0);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    (if mx > 0.0 { (mx - mn) / mx } else { 0.0 }, mx)
}

/// A nick's colour: hashed hue, half the theme accent's saturation/brightness,
/// pulled toward the text colour so it sits in the palette.
pub fn nick_color(nick: &str, p: &Palette) -> Color32 {
    let (s, v) = sat_val(p.accent);
    let h = nick_hue(nick);
    if p.dark { mix(hsv(h, (0.5 + s) / 2.0, (0.92 + v) / 2.0), p.text, 0.2) } else { mix(hsv(h, (0.7 + s) / 2.0, 0.55), p.text, 0.18) }
}

pub fn avatar_fill(nick: &str, p: &Palette) -> Color32 {
    let (s, v) = sat_val(p.accent);
    let h = nick_hue(nick);
    if p.dark { mix(hsv(h, (0.45 + s) / 2.0, (0.85 + v) / 2.0), p.text, 0.2) } else { mix(hsv(h, (0.4 + s) / 2.0, 0.86), p.bg, 0.15) }
}

fn avatar_ink(p: &Palette) -> Color32 {
    if p.dark { mix(p.panel, Color32::BLACK, 0.35) } else { Color32::from_rgb(0x1d, 0x1b, 0x26) }
}

pub fn initial(nick: &str) -> String {
    nick.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

// ----- avatars ---------------------------------------------------------------

/// How to draw one avatar.
#[derive(Clone, Copy)]
pub struct Face<'a> {
    pub nick: &'a str,
    /// Diameter.
    pub size: f32,
    /// Colour behind the avatar (for the gap around rings and dots).
    pub behind: Color32,
    /// Presence dot colour, bottom right.
    pub presence: Option<Color32>,
    /// Talking, and how loud (0..1, see `denchat_core::loudness::intensity`).
    pub talking: Option<f32>,
    /// Their picture (else a coloured circle with their initial).
    pub picture: Option<egui::TextureId>,
}

impl<'a> Face<'a> {
    pub fn new(nick: &'a str, size: f32, behind: Color32) -> Face<'a> {
        Face { nick, size, behind, presence: None, talking: None, picture: None }
    }

    /// Show this picture (see `PanelCx::picture`) instead of the initial.
    pub fn picture(mut self, tex: Option<egui::TextureId>) -> Self {
        self.picture = tex;
        self
    }

    pub fn presence(mut self, c: Color32) -> Self {
        self.presence = Some(c);
        self
    }

    /// Talking at the quietest level (a plain ring).
    pub fn speaking(mut self, on: bool) -> Self {
        self.talking = on.then_some(0.0);
        self
    }

    /// Talking at `level` (0..1): the ring thickens and a glow grows with it.
    pub fn talking(mut self, level: Option<f32>) -> Self {
        self.talking = level.map(|l| l.clamp(0.0, 1.0));
        self
    }

    /// How far past the avatar's edge the talking ring and glow can reach.
    pub fn reach(size: f32) -> f32 {
        4.0 + size * 0.3
    }

    pub fn paint(&self, painter: &Painter, center: Pos2, p: &Palette, _time: f64) {
        let r = self.size / 2.0;
        if let Some(level) = self.talking {
            talk_ring(painter, center, r, level, p);
        }
        match self.picture {
            Some(tex) => crate::pictures::paint_round(painter, center, self.size, tex),
            None => {
                painter.circle_filled(center, r, avatar_fill(self.nick, p));
                let ink = avatar_ink(p);
                painter.text(center, Align2::CENTER_CENTER, initial(self.nick), font((self.size * 0.42).max(8.0), W::Semibold), ink);
            }
        }
        if let Some(c) = self.presence {
            let dr = (self.size * 0.15).clamp(3.0, 6.0);
            let at = center + vec2(r * 0.72, r * 0.72);
            painter.circle_filled(at, dr + 2.0, self.behind);
            painter.circle_filled(at, dr, c);
        }
    }

    /// Allocate and draw; hover shows `tip` if given.
    pub fn show(&self, ui: &mut Ui, p: &Palette) -> Response {
        let pad = if self.talking.is_some() { Face::reach(self.size) } else { 0.0 };
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(self.size + pad * 2.0), Sense::hover());
        self.paint(ui.painter(), rect.center(), p, ui.input(|i| i.time));
        resp
    }
}

/// The talking indicator around an avatar of radius `r`: a ring that is
/// always clearly visible while someone talks (so you can tell at a glance
/// who is), and a soft glow plus a thicker ring that grow with `level`.
fn talk_ring(painter: &Painter, center: Pos2, r: f32, level: f32, p: &Palette) {
    let l = level.clamp(0.0, 1.0);
    // Ease out so quiet speech already moves the ring noticeably.
    let e = 1.0 - (1.0 - l) * (1.0 - l);
    let size = r * 2.0;
    let width = (size * 0.06).clamp(2.0, 3.0) * (1.0 + 0.7 * e);
    let gap = (size * 0.05).clamp(1.5, 3.0);
    let ring = r + gap + width / 2.0;
    let glow = (Face::reach(size) - gap - width) * e;
    if glow > 0.3 {
        // A halo outside the ring; two layers fake a soft falloff. Strokes,
        // not fills, so the gap inside the ring shows whatever is behind.
        let edge = ring + width / 2.0;
        painter.circle_stroke(center, edge + glow / 2.0, Stroke::new(glow, alpha(p.speaking, 0.07 + 0.12 * e)));
        painter.circle_stroke(center, edge + glow / 4.0, Stroke::new(glow / 2.0, alpha(p.speaking, 0.08 + 0.14 * e)));
    }
    painter.circle_stroke(center, ring, Stroke::new(width, alpha(p.speaking, 0.85 + 0.15 * e)));
}

/// Width of [`paint_stack`] for `n` faces.
pub fn stack_width(n: usize, size: f32, max: usize) -> f32 {
    let total = n.min(max) + usize::from(n > max);
    if total == 0 { 0.0 } else { size + size * 0.72 * (total - 1) as f32 }
}

/// One face in [`paint_stack`]: nick, talking level, picture.
pub type StackFace = (String, Option<f32>, Option<egui::TextureId>);

/// Overlapping avatars, `+N` after `max`.
pub fn paint_stack(ui: &Ui, left_center: Pos2, faces: &[StackFace], (size, max): (f32, usize), behind: Color32, p: &Palette) -> f32 {
    let painter = ui.painter();
    let time = ui.input(|i| i.time);
    let step = size * 0.72;
    let shown = faces.len().min(max);
    let x = left_center.x + size / 2.0;
    // Draw right to left so the first sits on top.
    let extra = faces.len().saturating_sub(max);
    let total = shown + usize::from(extra > 0);
    let width = if total == 0 { 0.0 } else { size + step * (total - 1) as f32 };
    let centers: Vec<Pos2> = (0..total).map(|i| pos2(x + step * i as f32, left_center.y)).collect();
    if extra > 0 {
        let c = centers[total - 1];
        painter.circle_filled(c, size / 2.0 + 2.0, behind);
        painter.circle_filled(c, size / 2.0, p.surface);
        painter.text(c, Align2::CENTER_CENTER, format!("+{extra}"), font(size * 0.4, W::Semibold), p.muted);
    }
    for i in (0..shown).rev() {
        let c = centers[i];
        painter.circle_filled(c, size / 2.0 + 2.0, behind);
        Face { nick: &faces[i].0, size, behind, presence: None, talking: faces[i].1, picture: faces[i].2 }.paint(painter, c, p, time);
    }
    width
}

// ----- text ----------------------------------------------------------------

/// One line, cut with `…` at `max_width`.
pub fn line(ui: &Ui, text: impl Into<String>, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.into(), font, color);
    job.wrap = TextWrapping { max_width: max_width.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.ctx().fonts_mut(|f| f.layout_job(job))
}

/// Paint a galley with its left edge at `x`, vertically centred on `y`.
pub fn put(painter: &Painter, g: &Arc<Galley>, x: f32, y: f32, fallback: Color32) {
    painter.galley(pos2(x, y - g.size().y / 2.0), g.clone(), fallback);
}

/// Paint an icon glyph centred at `c`.
pub fn icon(painter: &Painter, c: Pos2, glyph: &str, size: f32, color: Color32) {
    painter.text(c, Align2::CENTER_CENTER, glyph, FontId::proportional(size), color);
}

// ----- naming ----------------------------------------------------------------

/// Give a hand-painted widget a role and a name, so screen readers and
/// `den ui tree` / `den ui click` see it like a stock egui widget.
pub fn name(resp: &Response, role: Role, label: impl Into<String>) {
    let label = label.into();
    let enabled = resp.enabled();
    resp.widget_info(|| egui::WidgetInfo::labeled(role, enabled, &label));
}

/// [`name`] for something that can be the current one (a channel row).
pub fn name_selected(resp: &Response, role: Role, selected: bool, label: impl Into<String>) {
    let label = label.into();
    let enabled = resp.enabled();
    resp.widget_info(|| egui::WidgetInfo::selected(role, enabled, selected, &label));
}

// ----- controls --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Muted icon, brighter on hover.
    Plain,
    /// Toggled on (text colour).
    On,
    /// Something is switched off that is normally on (mic, sound): red.
    Off,
    /// A destructive action (hang up): red on a soft red tile.
    Danger,
}

/// A square icon button with a hover tile.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, tone: Tone, size: f32, p: &Palette) -> Response {
    let (_, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    paint_icon_button(ui, &resp, glyph, tone, p);
    name_icon_button(&resp, "icon", tip, tone);
    if !tip.is_empty() { resp.on_hover_text(tip) } else { resp }
}

/// [`icon_button`] at a rect the caller laid out.
pub fn icon_button_at(ui: &mut Ui, rect: Rect, salt: &str, glyph: &str, tip: &str, tone: Tone, p: &Palette) -> Response {
    let resp = ui.interact(rect, ui.scope_id().with(salt), Sense::click());
    paint_icon_button(ui, &resp, glyph, tone, p);
    name_icon_button(&resp, salt, tip, tone);
    if !tip.is_empty() { resp.on_hover_text(tip) } else { resp }
}

/// How lit a hover highlight is (0..1). It comes on in the same frame the
/// pointer arrives, since any fade-in reads as lag behind the cursor, and
/// fades briefly as the pointer leaves so sweeping down a list stays calm.
pub fn hover_fade(ui: &Ui, id: egui::Id, hovered: bool) -> f32 {
    ui.ctx().animate_bool_with_time(id, hovered, if hovered { 0.0 } else { 0.1 })
}

/// The tooltip is the name (it says what a click does: `Mute`, `Unmute`).
fn name_icon_button(resp: &Response, fallback: &str, tip: &str, _tone: Tone) {
    name(resp, Role::Button, if tip.is_empty() { fallback } else { tip });
}

fn paint_icon_button(ui: &Ui, resp: &Response, glyph: &str, tone: Tone, p: &Palette) {
    let rect = resp.rect;
    let t = hover_fade(ui, resp.id, resp.hovered());
    let radius = CornerRadius::same(p.row_radius.min((rect.height() / 2.0) as u8));
    let (bg, fg) = match tone {
        Tone::Plain => (alpha(p.text, 0.07 * t), mix(p.muted, p.text, t)),
        Tone::On => (alpha(p.text, 0.05 + 0.05 * t), p.text),
        Tone::Off | Tone::Danger => (mix(p.danger_soft, alpha(p.danger, 0.26), t), p.danger),
    };
    ui.painter().rect_filled(rect, radius, bg);
    icon(ui.painter(), rect.center(), glyph, (rect.height() * 0.56).round(), fg);
}

/// A small rounded text button (`Join`, `d20`).
pub fn pill_button(ui: &mut Ui, text: &str, p: &Palette) -> Response {
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font(12.5, W::Semibold), p.text));
    let size = vec2(g.size().x + 20.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let t = hover_fade(ui, resp.id, resp.hovered());
    ui.painter().rect_filled(rect, CornerRadius::same(12), mix(p.surface, p.surface_hover, t));
    put(ui.painter(), &g, rect.left() + 10.0, rect.center().y, p.text);
    name(&resp, Role::Button, text);
    resp
}

/// The main action of a form (Join, Host): accent fill.
pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool, p: &Palette) -> Response {
    let fg = if enabled { p.accent_text } else { p.muted };
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font(13.5, W::Semibold), fg));
    let size = vec2((g.size().x + 32.0).max(72.0), 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let t = hover_fade(ui, resp.id, resp.hovered() && enabled);
    let bg = if enabled { mix(p.accent, p.text, 0.12 * t) } else { p.surface };
    ui.painter().rect_filled(rect, CornerRadius::same(p.row_radius.saturating_add(2).min(12)), bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp.widget_info(|| egui::WidgetInfo::labeled(Role::Button, enabled, text));
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// A count pill (`3`, `@`) right-aligned at `right`; returns its width.
pub fn badge(painter: &Painter, ui: &Ui, right: Pos2, text: &str, bg: Color32, fg: Color32) -> f32 {
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font(11.0, W::Semibold), fg));
    let w = (g.size().x + 12.0).max(18.0);
    let rect = Rect::from_min_size(pos2(right.x - w, right.y - 9.0), vec2(w, 18.0));
    painter.rect_filled(rect, CornerRadius::same(9), bg);
    painter.galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, fg);
    w
}

/// Label for a group of rows ("Channels", "Voice"): sentence case, muted.
pub fn section(ui: &mut Ui, text: &str, p: &Palette) -> Response {
    ui.add_space(10.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    let g = line(ui, text, font(12.5, W::Semibold), p.muted, w - 8.0);
    put(ui.painter(), &g, rect.left() + 8.0, rect.center().y, p.muted);
    name(&resp, Role::Heading, text);
    resp
}

/// A row that highlights on hover and can be selected. Returns the click response.
pub fn row(ui: &mut Ui, height: f32, selected: bool, raised: bool, p: &Palette) -> (Rect, Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let t = hover_fade(ui, resp.id, resp.hovered());
    let radius = CornerRadius::same(p.row_radius);
    if selected {
        if raised {
            ui.painter().add(p.shadow_small().as_shape(rect, radius));
            ui.painter().rect_filled(rect, radius, p.panel);
        } else {
            ui.painter().rect_filled(rect, radius, p.row_active);
        }
    } else if t > 0.0 {
        ui.painter().rect_filled(rect, radius, alpha(p.text, 0.045 * t));
    }
    (rect, resp)
}

/// A card: rounded, filled, with the theme's soft shadow when `raised`.
pub fn card(p: &Palette, fill: Color32, raised: bool) -> Frame {
    Frame::new().fill(fill).corner_radius(CornerRadius::same(p.card_radius.min(16))).inner_margin(Margin::same(10)).shadow(if raised {
        p.shadow_small()
    } else {
        Shadow::NONE
    })
}

/// Three connection-strength bars (like a phone's), `n` lit out of 3.
pub fn signal(painter: &Painter, left_bottom: Pos2, n: u8, color: Color32, off: Color32) {
    for i in 0..3u8 {
        let h = [4.0, 7.0, 10.0][i as usize];
        let x = left_bottom.x + i as f32 * 4.0;
        let r = Rect::from_min_max(pos2(x, left_bottom.y - h), pos2(x + 2.5, left_bottom.y));
        painter.rect_filled(r, CornerRadius::same(1), if i < n { color } else { off });
    }
}

/// Animated level bars for someone talking, taller the louder they are
/// (`level` 0..1); never flat, so talking always shows.
pub fn eq(painter: &Painter, left_bottom: Pos2, color: Color32, time: f64, level: f32) {
    let l = level.clamp(0.0, 1.0);
    for i in 0..3 {
        let phase = time * (6.0 + 4.0 * l as f64) + i as f64 * 1.9;
        let h = 3.0 + (2.5 + 8.5 * l) * (0.55 + 0.45 * phase.sin() as f32);
        let x = left_bottom.x + i as f32 * 4.5;
        painter.rect_filled(Rect::from_min_max(pos2(x, left_bottom.y - h), pos2(x + 3.0, left_bottom.y)), CornerRadius::same(1), color);
    }
}

/// Connection quality for the signal bars: (bars, colour).
pub fn signal_level(latency_ms: Option<u32>, p: &Palette) -> (u8, Color32) {
    match latency_ms {
        Some(ms) if ms < 90 => (3, p.ok),
        Some(ms) if ms < 220 => (2, p.warn),
        Some(_) => (1, p.danger),
        None => (0, p.muted),
    }
}

/// `Today`, `Yesterday`, or a date, for day dividers.
pub fn day_label(ts: i64) -> String {
    use chrono::{Datelike, Local};
    let Some(t) = chrono::DateTime::from_timestamp_millis(ts).map(|t| t.with_timezone(&Local)) else { return String::new() };
    let today = Local::now().date_naive();
    let d = t.date_naive();
    if d == today {
        "Today".into()
    } else if Some(d) == today.pred_opt() {
        "Yesterday".into()
    } else if d.year() == today.year() {
        t.format("%A %-d %B").to_string()
    } else {
        t.format("%-d %B %Y").to_string()
    }
}

pub fn local_day(ts: i64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp_millis(ts).map(|t| t.with_timezone(&chrono::Local).date_naive())
}

/// A centred pill on a hairline ("Today").
pub fn day_divider(ui: &mut Ui, text: &str, p: &Palette) {
    ui.add_space(8.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    name(&resp, Role::Label, text);
    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font(12.0, W::Semibold), p.muted));
    let pill = Rect::from_center_size(rect.center(), vec2(g.size().x + 24.0, 22.0));
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.line));
    ui.painter().rect_filled(pill, CornerRadius::same(11), p.surface);
    ui.painter().galley(pill.center() - g.size() / 2.0, g, p.muted);
}

/// The red "new messages" line. `label` sits at the right (flat) or centre (bubbles).
pub fn new_divider(ui: &mut Ui, label: &str, centred: bool, p: &Palette) {
    ui.add_space(10.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
    name(&resp, Role::Label, label);
    let y = rect.center().y;
    let stroke = Stroke::new(1.0, alpha(p.danger, 0.75));
    if centred {
        let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.to_owned(), font(12.0, W::Semibold), p.danger));
        let half = g.size().x / 2.0 + 10.0;
        ui.painter().hline(rect.left() + 40.0..=rect.center().x - half, y, stroke);
        ui.painter().hline(rect.center().x + half..=rect.right() - 40.0, y, stroke);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, p.danger);
    } else {
        let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(label.to_uppercase(), font(10.5, W::Semibold), p.bg));
        let tag = Rect::from_min_size(pos2(rect.right() - g.size().x - 12.0, y - 8.0), vec2(g.size().x + 12.0, 16.0));
        ui.painter().hline(rect.left()..=tag.left() - 4.0, y, stroke);
        ui.painter().rect_filled(tag, CornerRadius::same(4), p.danger);
        ui.painter().galley(tag.center() - g.size() / 2.0, g, p.bg);
    }
}

impl Palette {
    /// The card shadow, a little tighter for small things (rows, pills).
    pub fn shadow_small(&self) -> Shadow {
        Shadow { offset: [0, 1], blur: 8, spread: 0, color: self.shadow.color.gamma_multiply(0.8) }
    }
}
