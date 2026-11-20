//! Skeuomorphic surfaces for themes with a `[material]` table: bevelled
//! plates, sunken wells, glows, screws, engraved labels and textured canvases.
//!
//! egui fills shapes with one colour, so gradients are built here as meshes:
//! the shape is tessellated with egui's own anti-aliased path filler in
//! white, then each vertex is recoloured from its position (the feathered
//! edge keeps its fade). Textures are generated procedurally on first use and
//! tiled with `TextureWrapMode::Repeat`, which the CPU renderer in `soft.rs`
//! honours too, so headless screenshots match the window.
//!
//! Everything returns [`Shape`]s rather than painting, so callers can drop a
//! background behind content they have already laid out (`Painter::set`).

use egui::epaint::tessellator::{Path, path};
use egui::epaint::{Mesh, RoundedRect};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Color32, ColorImage, Context, CornerRadius, FontId, Galley, Id, Painter, Pos2, Rect, Shadow, Shape, Stroke, TextureHandle, TextureId,
    TextureOptions, TextureWrapMode, Vec2, pos2, vec2,
};
use std::sync::Arc;

use crate::theme::{Material, Palette, TextureKind, alpha, mix};

/// The theme's material, if it has one.
pub fn of(p: &Palette) -> Option<Material> {
    p.material
}

/// Lighter (`t` > 0, toward white) or darker (`t` < 0, toward black).
pub fn shade(c: Color32, t: f32) -> Color32 {
    if t >= 0.0 { mix(c, Color32::WHITE, t) } else { mix(c, Color32::BLACK, -t) }
}

// ----- meshes ------------------------------------------------------------------

/// Points of a rounded rectangle's outline.
fn outline(rect: Rect, radius: CornerRadius) -> Vec<Pos2> {
    let mut points = Vec::new();
    path::rounded_rectangle(&mut points, RoundedRect::new(rect, radius));
    points
}

/// Points of a circle's outline.
fn circle(center: Pos2, r: f32) -> Vec<Pos2> {
    let n = ((r * 2.0) as usize).clamp(12, 48);
    (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            center + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// An anti-aliased convex fill of `points`, coloured per vertex by `color`.
fn filled(painter: &Painter, points: &[Pos2], color: impl Fn(Pos2) -> Color32) -> Shape {
    if points.len() < 3 {
        return Shape::Noop;
    }
    let mut mesh = Mesh::default();
    let mut p = Path::default();
    p.add_line_loop(points);
    p.fill(1.0 / painter.pixels_per_point(), Color32::WHITE, &mut mesh);
    for v in &mut mesh.vertices {
        // White in, transparent on the outside of the feather: keep that fade.
        let coverage = v.color.a() as f32 / 255.0;
        v.color = color(v.pos).gamma_multiply(coverage);
    }
    Shape::mesh(mesh)
}

/// A rounded rectangle filled with a vertical gradient, `top` to `bottom`.
pub fn gradient(painter: &Painter, rect: Rect, radius: CornerRadius, top: Color32, bottom: Color32) -> Shape {
    let h = rect.height().max(1.0);
    filled(painter, &outline(rect, radius), |pos| mix(top, bottom, ((pos.y - rect.top()) / h).clamp(0.0, 1.0)))
}

/// A disc lit from the top left: `hi` there, `lo` at the bottom right.
pub fn dome(painter: &Painter, center: Pos2, r: f32, hi: Color32, lo: Color32) -> Shape {
    let from = center - Vec2::splat(r * 0.7);
    let span = r * 1.4 * std::f32::consts::SQRT_2 * 1.4;
    filled(painter, &circle(center, r), |pos| {
        let t = ((pos - from).dot(vec2(1.0, 1.0).normalized()) / span).clamp(0.0, 1.0);
        mix(hi, lo, t)
    })
}

/// A rounded rectangle tiled with `tex` (`tile` points per repeat), tinted.
pub fn textured(painter: &Painter, rect: Rect, radius: CornerRadius, tex: TextureId, tile: f32, tint: Color32) -> Shape {
    let points = outline(rect, radius);
    if points.len() < 3 || tile <= 0.0 {
        return Shape::Noop;
    }
    let mut mesh = Mesh::with_texture(tex);
    let mut p = Path::default();
    p.add_line_loop(&points);
    p.fill_with_uv(1.0 / painter.pixels_per_point(), tint, tex, |pos| pos2(pos.x / tile, pos.y / tile), &mut mesh);
    Shape::mesh(mesh)
}

// ----- surfaces ----------------------------------------------------------------

/// A raised plate: drop shadow, a dark rim, a body lit from above, a bright
/// top edge and a shaded bottom edge.
pub fn raised(painter: &Painter, rect: Rect, radius: CornerRadius, fill: Color32, p: &Palette, m: &Material) -> Vec<Shape> {
    let b = m.bevel;
    let r = radius.at_least(1);
    let mut out = vec![p.shadow.as_shape(rect, r).into()];
    out.push(Shape::rect_filled(rect.expand(1.0), grow(r, 1), mix(fill, Color32::BLACK, 0.65)));
    out.push(gradient(painter, rect, r, shade(fill, 0.07 * b), shade(fill, -0.12 * b)));
    out.extend(edges(rect, r, b));
    out
}

/// A small raised part (a button cap): like [`raised`] with a tighter shadow.
pub fn button(painter: &Painter, rect: Rect, radius: CornerRadius, fill: Color32, p: &Palette, m: &Material) -> Vec<Shape> {
    let b = m.bevel;
    let mut out = vec![p.shadow_small().as_shape(rect, radius).into()];
    out.push(Shape::rect_filled(rect.expand(1.0), grow(radius, 1), mix(fill, Color32::BLACK, 0.6)));
    out.push(gradient(painter, rect, radius, shade(fill, 0.16 * b), shade(fill, -0.14 * b)));
    out.extend(edges(rect, radius, b));
    out
}

/// The lit top edge and shaded bottom edge of a plate.
fn edges(rect: Rect, radius: CornerRadius, b: f32) -> Vec<Shape> {
    let inset = |c: u8| (c as f32 * 0.6).max(2.0);
    let top = rect.top() + 0.5;
    let bottom = rect.bottom() - 0.5;
    vec![
        Shape::hline(
            rect.left() + inset(radius.nw)..=rect.right() - inset(radius.ne),
            top,
            Stroke::new(1.0, alpha(Color32::WHITE, 0.13 * b)),
        ),
        Shape::hline(
            rect.left() + inset(radius.sw)..=rect.right() - inset(radius.se),
            bottom,
            Stroke::new(1.0, alpha(Color32::BLACK, 0.4 * b)),
        ),
    ]
}

/// A sunken well: a light lip below, a dark rim, a body that darkens toward
/// the top and a shadow falling in from the top edge.
pub fn sunken(painter: &Painter, rect: Rect, radius: CornerRadius, fill: Color32, m: &Material) -> Vec<Shape> {
    let (b, d) = (m.bevel, m.inset);
    let r = radius;
    let mut out = vec![
        // Light catching the lower lip, just outside the well.
        Shape::hline(
            rect.left() + (r.sw as f32 * 0.6).max(2.0)..=rect.right() - (r.se as f32 * 0.6).max(2.0),
            rect.bottom() + 1.5,
            Stroke::new(1.0, alpha(Color32::WHITE, 0.07 * b)),
        ),
        Shape::rect_filled(rect.expand(1.0), grow(r, 1), mix(fill, Color32::BLACK, 0.7)),
        gradient(painter, rect, r, shade(fill, -0.25 * d), fill),
    ];
    // The shadow falls in from the top; at least the corner radius deep so its
    // square bottom corners sit on the straight part of the sides.
    let depth = (6.0 + 8.0 * d).max(r.nw.max(r.ne) as f32).min(rect.height());
    let band = Rect::from_min_size(rect.min, vec2(rect.width(), depth));
    let band_r = CornerRadius { nw: r.nw, ne: r.ne, sw: 0, se: 0 };
    out.push(gradient(painter, band, band_r, alpha(Color32::BLACK, 0.55 * d), Color32::TRANSPARENT));
    out
}

/// A soft coloured glow behind `rect`, like light from a backlit part.
pub fn glow(rect: Rect, radius: CornerRadius, color: Color32, strength: f32) -> Shape {
    if strength <= 0.0 {
        return Shape::Noop;
    }
    let blur = (6.0 + 10.0 * strength) as u8;
    Shadow { offset: [0, 0], blur, spread: 0, color: alpha(color, 0.5 * strength) }.as_shape(rect, radius).into()
}

/// A screw head: a domed disc with a slot.
pub fn screw(painter: &Painter, center: Pos2, r: f32, metal: Color32) -> Vec<Shape> {
    let slot = vec2(r * 0.62, -r * 0.62);
    vec![
        Shape::circle_filled(center + vec2(0.0, 0.6), r + 0.6, alpha(Color32::WHITE, 0.07)),
        Shape::circle_filled(center, r + 0.6, alpha(Color32::BLACK, 0.7)),
        dome(painter, center, r, shade(metal, 0.45), shade(metal, -0.35)),
        Shape::line_segment([center - slot, center + slot], Stroke::new((r * 0.35).max(0.8), alpha(Color32::BLACK, 0.65))),
    ]
}

/// Screws in the four corners of a plate.
pub fn screws(painter: &Painter, rect: Rect, metal: Color32) -> Vec<Shape> {
    let i = 6.5;
    [
        pos2(rect.left() + i, rect.top() + i),
        pos2(rect.right() - i, rect.top() + i),
        pos2(rect.left() + i, rect.bottom() - i),
        pos2(rect.right() - i, rect.bottom() - i),
    ]
    .into_iter()
    .flat_map(|c| screw(painter, c, 2.6, metal))
    .collect()
}

fn grow(r: CornerRadius, by: u8) -> CornerRadius {
    CornerRadius { nw: r.nw.saturating_add(by), ne: r.ne.saturating_add(by), sw: r.sw.saturating_add(by), se: r.se.saturating_add(by) }
}

// ----- text ----------------------------------------------------------------------

/// One line of tracked text (letter spacing in points), cut with `…` at `max_width`.
pub fn tracked(painter: &Painter, text: &str, font: FontId, color: Color32, spacing: f32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    job.append(text, 0.0, TextFormat { font_id: font, color, extra_letter_spacing: spacing, ..Default::default() });
    job.wrap = TextWrapping { max_width: max_width.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    painter.ctx().fonts_mut(|f| f.layout_job(job))
}

/// Text cut into the surface: a dark edge above and a lit edge below.
pub fn engraved(painter: &Painter, pos: Pos2, galley: &Arc<Galley>, color: Color32, m: &Material) {
    painter.galley_with_override_text_color(pos - vec2(0.0, 1.0), galley.clone(), alpha(Color32::BLACK, 0.75));
    painter.galley_with_override_text_color(pos + vec2(0.0, 1.0), galley.clone(), alpha(Color32::WHITE, 0.06 * m.bevel));
    painter.galley_with_override_text_color(pos, galley.clone(), color);
}

// ----- textures --------------------------------------------------------------------

/// Texels per tile edge.
const TILE: usize = 256;

/// The canvas behind the cards: its colour, then the theme's texture.
pub fn canvas(painter: &Painter, rect: Rect, fill: Color32, m: &Material) -> Vec<Shape> {
    let mut out = vec![Shape::rect_filled(rect, CornerRadius::ZERO, fill)];
    out.extend(texture_over(painter, rect, CornerRadius::ZERO, m.texture, m.texture_strength));
    // A faint pool of light from above, so the surface isn't dead flat.
    out.push(gradient(painter, rect, CornerRadius::ZERO, alpha(Color32::WHITE, 0.035 * m.bevel), alpha(Color32::BLACK, 0.12 * m.bevel)));
    out
}

/// `kind` laid over `rect` at `strength` (0..1), if it's a texture at all.
pub fn texture_over(painter: &Painter, rect: Rect, radius: CornerRadius, kind: TextureKind, strength: f32) -> Option<Shape> {
    let tex = texture(painter.ctx(), kind)?;
    // One texel per physical pixel, so the streaks stay crisp at any zoom.
    let tile = TILE as f32 / painter.pixels_per_point();
    Some(textured(painter, rect, radius, tex, tile, Color32::WHITE.gamma_multiply(strength.clamp(0.0, 1.0))))
}

/// The texture for `kind`, made once per context.
fn texture(ctx: &Context, kind: TextureKind) -> Option<TextureId> {
    if kind == TextureKind::None {
        return None;
    }
    let key = Id::unique(("den-material-texture", kind));
    if let Some(h) = ctx.data(|d| d.get_temp::<TextureHandle>(key)) {
        return Some(h.id());
    }
    let options = TextureOptions { wrap_mode: TextureWrapMode::Repeat, ..TextureOptions::LINEAR };
    let handle = ctx.load_texture(format!("material-{kind:?}"), generate(kind, TILE), options);
    let id = handle.id();
    ctx.data_mut(|d| d.insert_temp(key, handle));
    Some(id)
}

/// A small deterministic PRNG (xorshift64*), so textures look the same every run.
struct Rng(u64);

impl Rng {
    /// Uniform in -1..1.
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        ((v >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// A `size`×`size` tile that repeats seamlessly: each texel lightens (white)
/// or darkens (black) what's under it, premultiplied, at most ~14 % opacity.
pub fn generate(kind: TextureKind, size: usize) -> ColorImage {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut v = vec![0.0f32; size * size];
    match kind {
        TextureKind::None => {}
        TextureKind::Grain => {
            for x in &mut v {
                *x = rng.next() * 0.6;
            }
        }
        TextureKind::Brushed => {
            // Long horizontal streaks: noise along each row, box-blurred
            // around the tile (so it wraps), plus a brightness per row,
            // softened against its neighbours (wrapping too).
            let rows: Vec<f32> = (0..size).map(|_| rng.next()).collect();
            let k = (size / 10).max(1);
            let norm = ((2 * k + 1) as f32).sqrt();
            for y in 0..size {
                let base = (rows[(y + size - 1) % size] + 2.0 * rows[y] + rows[(y + 1) % size]) / 4.0;
                let raw: Vec<f32> = (0..size).map(|_| rng.next()).collect();
                let mut sum: f32 = (0..=2 * k).map(|i| raw[(i + size - k) % size]).sum();
                for x in 0..size {
                    let streak = sum / (2 * k + 1) as f32 * norm;
                    v[y * size + x] = 0.55 * base + 0.35 * streak + 0.12 * rng.next();
                    sum += raw[(x + k + 1) % size] - raw[(x + size - k) % size];
                }
            }
        }
    }
    let pixels = v
        .into_iter()
        .map(|x| {
            let a = (x.abs().min(1.0) * 0.14 * 255.0).round() as u8;
            if x >= 0.0 { Color32::from_rgba_premultiplied(a, a, a, a) } else { Color32::from_rgba_premultiplied(0, 0, 0, a) }
        })
        .collect();
    ColorImage::new([size, size], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textures_are_deterministic_and_faint() {
        for kind in [TextureKind::Brushed, TextureKind::Grain] {
            let a = generate(kind, 64);
            assert_eq!(a.pixels, generate(kind, 64).pixels, "{kind:?}");
            assert!(a.pixels.iter().all(|c| c.a() <= 36), "{kind:?} too strong");
            assert!(a.pixels.iter().any(|c| c.a() > 4), "{kind:?} invisible");
            // Premultiplied: never brighter than opaque.
            assert!(a.pixels.iter().all(|c| c.r() <= c.a()));
        }
        assert!(generate(TextureKind::None, 8).pixels.iter().all(|c| *c == Color32::TRANSPARENT));
    }

    #[test]
    fn brushed_runs_along_rows() {
        // Neighbours along a row are far more alike than neighbours down a column.
        let img = generate(TextureKind::Brushed, 128);
        let at = |x: usize, y: usize| {
            let c = img.pixels[y * 128 + x];
            if c.r() > 0 { c.a() as f32 } else { -(c.a() as f32) }
        };
        let (mut along, mut across) = (0.0, 0.0);
        for y in 0..127 {
            for x in 0..127 {
                along += (at(x, y) - at(x + 1, y)).abs();
                across += (at(x, y) - at(x, y + 1)).abs();
            }
        }
        assert!(along * 1.5 < across, "along {along} across {across}");
    }

    #[test]
    fn gradient_keeps_the_feathered_edge() {
        let ctx = Context::default();
        let painter = Painter::new(ctx, egui::LayerId::background(), Rect::EVERYTHING);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 20.0));
        let Shape::Mesh(mesh) = gradient(&painter, rect, CornerRadius::same(4), Color32::WHITE, Color32::BLACK) else {
            panic!("not a mesh")
        };
        // The feather's outer ring stays transparent; inside runs white to black.
        assert!(mesh.vertices.iter().any(|v| v.color == Color32::TRANSPARENT));
        let top = mesh.vertices.iter().filter(|v| v.pos.y < 2.0 && v.color.a() == 255).map(|v| v.color.r()).min();
        let bottom = mesh.vertices.iter().filter(|v| v.pos.y > 18.0 && v.color.a() == 255).map(|v| v.color.r()).max();
        assert!(top.unwrap_or(0) > 200 && bottom.unwrap_or(255) < 55, "top {top:?} bottom {bottom:?}");
    }
}
