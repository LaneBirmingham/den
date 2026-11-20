//! A small software rasterizer for egui's output.
//!
//! Screenshots normally come from the GPU, which only draws a window the OS
//! shows. Drawing the same triangles on the CPU gives a screenshot with no
//! GPU and no visible window: a headless run (`DEN_WINDOW=headless`), or an
//! app whose window is minimised or covered. egui's tessellator already
//! anti-aliases edges with feathered vertices, so plain triangle filling with
//! a top-left rule and premultiplied blending matches the GPU closely.

use std::collections::HashMap;
use std::path::Path;

use egui::epaint::textures::TextureFilter;
use egui::epaint::textures::{TextureWrapMode, TexturesDelta};
use egui::epaint::{ClippedPrimitive, ImageData, Primitive, TextureId, Vertex};
use egui::{Color32, ColorImage, Rect};

struct Texture {
    size: [usize; 2],
    pixels: Vec<Color32>,
    linear: bool,
    /// Tiles (`TextureWrapMode::Repeat`, for material textures) rather than clamping at the edges.
    repeat: bool,
}

/// A CPU copy of every texture egui has uploaded (the font atlas, pictures, material textures).
#[derive(Default)]
pub struct Textures {
    map: HashMap<TextureId, Texture>,
}

impl Textures {
    /// Apply a frame's texture changes, as a GPU painter would.
    pub fn apply(&mut self, delta: &TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let ImageData::Color(img) = &d.image;
                let linear = d.options.magnification == TextureFilter::Linear;
                let repeat = d.options.wrap_mode == TextureWrapMode::Repeat;
                match d.pos {
                    None => {
                        self.map.insert(*id, Texture { size: img.size, pixels: img.pixels.clone(), linear, repeat });
                    }
                    Some([x0, y0]) => {
                        let Some(t) = self.map.get_mut(id) else { continue };
                        let [w, h] = img.size;
                        for y in 0..h.min(t.size[1].saturating_sub(y0)) {
                            let dst = (y0 + y) * t.size[0] + x0;
                            let n = w.min(t.size[0].saturating_sub(x0));
                            t.pixels[dst..dst + n].copy_from_slice(&img.pixels[y * w..y * w + n]);
                        }
                    }
                }
            }
        }
        for id in &delta.free {
            self.map.remove(id);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Premultiplied RGBA, 0..1.
type Px = [f32; 4];

fn px(c: Color32) -> Px {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0]
}

impl Texture {
    fn sample(&self, u: f32, v: f32) -> Px {
        let [w, h] = self.size;
        if w == 0 || h == 0 {
            return [1.0; 4];
        }
        let (u, v) = if self.repeat { (u.rem_euclid(1.0), v.rem_euclid(1.0)) } else { (u, v) };
        let at = |x: usize, y: usize| {
            let (x, y) = if self.repeat { (x % w, y % h) } else { (x.min(w - 1), y.min(h - 1)) };
            px(self.pixels[y * w + x])
        };
        if !self.linear {
            let x = ((u * w as f32) as isize).clamp(0, w as isize - 1) as usize;
            let y = ((v * h as f32) as isize).clamp(0, h as isize - 1) as usize;
            return at(x, y);
        }
        let x = (u * w as f32 - 0.5).max(0.0);
        let y = (v * h as f32 - 0.5).max(0.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        let mut out = [0.0; 4];
        for i in 0..4 {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            out[i] = top + (bottom - top) * fy;
        }
        out
    }
}

/// Paint `primitives` (from `ctx.tessellate`) into a `size` pixel image.
pub fn render(primitives: &[ClippedPrimitive], textures: &Textures, size: [usize; 2], pixels_per_point: f32, clear: Color32) -> ColorImage {
    let [w, h] = size;
    let mut canvas = Canvas { w, buf: vec![px(clear); w * h] };
    for p in primitives {
        let Primitive::Mesh(mesh) = &p.primitive else { continue };
        let clip = p.clip_rect;
        let clip = [
            (clip.min.x * pixels_per_point).round().clamp(0.0, w as f32) as usize,
            (clip.min.y * pixels_per_point).round().clamp(0.0, h as f32) as usize,
            (clip.max.x * pixels_per_point).round().clamp(0.0, w as f32) as usize,
            (clip.max.y * pixels_per_point).round().clamp(0.0, h as f32) as usize,
        ];
        if clip[0] >= clip[2] || clip[1] >= clip[3] {
            continue;
        }
        let tex = textures.map.get(&mesh.texture_id);
        for [i, j, k] in mesh.indices.as_chunks::<3>().0 {
            let v = |i: u32| mesh.vertices.get(i as usize);
            if let (Some(a), Some(b), Some(c)) = (v(*i), v(*j), v(*k)) {
                canvas.triangle(clip, [a, b, c], pixels_per_point, tex);
            }
        }
    }
    let pixels = canvas
        .buf
        .iter()
        .map(|p| {
            let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
            Color32::from_rgba_premultiplied(q(p[0]), q(p[1]), q(p[2]), q(p[3]))
        })
        .collect();
    ColorImage::new(size, pixels)
}

struct Canvas {
    w: usize,
    buf: Vec<Px>,
}

fn orient(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Top-left fill rule (y down, clockwise on screen): pixels exactly on an
/// edge belong to one of the two triangles sharing it, never both.
fn top_left(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    (dy == 0.0 && dx > 0.0) || dy < 0.0
}

impl Canvas {
    fn triangle(&mut self, clip: [usize; 4], v: [&Vertex; 3], ppp: f32, tex: Option<&Texture>) {
        let p = |v: &Vertex| (v.pos.x * ppp, v.pos.y * ppp);
        let [a, mut b, mut c] = v;
        let mut area = orient(p(a), p(b), p(c));
        if area < 0.0 {
            std::mem::swap(&mut b, &mut c);
            area = -area;
        }
        if area < 1e-6 {
            return;
        }
        let (pa, pb, pc) = (p(a), p(b), p(c));
        let x0 = pa.0.min(pb.0).min(pc.0).floor().max(clip[0] as f32) as usize;
        let y0 = pa.1.min(pb.1).min(pc.1).floor().max(clip[1] as f32) as usize;
        let x1 = (pa.0.max(pb.0).max(pc.0).ceil() as usize).min(clip[2]);
        let y1 = (pa.1.max(pb.1).max(pc.1).ceil() as usize).min(clip[3]);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (ca, cb, cc) = (px(a.color), px(b.color), px(c.color));
        let same_uv = a.uv == b.uv && b.uv == c.uv;
        let flat = match tex {
            None => [1.0; 4],
            Some(t) if same_uv => t.sample(a.uv.x, a.uv.y),
            Some(_) => [1.0; 4],
        };
        let (tl_a, tl_b, tl_c) = (top_left(pb, pc), top_left(pc, pa), top_left(pa, pb));
        let inv = 1.0 / area;
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            let row = y * self.w;
            for x in x0..x1 {
                let q = (x as f32 + 0.5, py);
                let (ea, eb, ec) = (orient(pb, pc, q), orient(pc, pa, q), orient(pa, pb, q));
                let inside = |e: f32, tl: bool| e > 0.0 || (e == 0.0 && tl);
                if !(inside(ea, tl_a) && inside(eb, tl_b) && inside(ec, tl_c)) {
                    continue;
                }
                let (wa, wb, wc) = (ea * inv, eb * inv, ec * inv);
                let texel = match tex {
                    Some(t) if !same_uv => {
                        let u = a.uv.x * wa + b.uv.x * wb + c.uv.x * wc;
                        let v = a.uv.y * wa + b.uv.y * wb + c.uv.y * wc;
                        t.sample(u, v)
                    }
                    _ => flat,
                };
                let dst = &mut self.buf[row + x];
                let src_a = (ca[3] * wa + cb[3] * wb + cc[3] * wc) * texel[3];
                for i in 0..3 {
                    let s = (ca[i] * wa + cb[i] * wb + cc[i] * wc) * texel[i];
                    dst[i] = s + dst[i] * (1.0 - src_a);
                }
                dst[3] = src_a + dst[3] * (1.0 - src_a);
            }
        }
    }
}

/// The part of `img` inside `rect` (pixels).
pub fn crop(img: &ColorImage, rect: Rect) -> ColorImage {
    let [w, h] = img.size;
    let x0 = (rect.min.x.floor().max(0.0) as usize).min(w);
    let y0 = (rect.min.y.floor().max(0.0) as usize).min(h);
    let x1 = (rect.max.x.ceil().max(0.0) as usize).clamp(x0, w);
    let y1 = (rect.max.y.ceil().max(0.0) as usize).clamp(y0, h);
    let mut pixels = Vec::with_capacity((x1 - x0) * (y1 - y0));
    for y in y0..y1 {
        pixels.extend_from_slice(&img.pixels[y * w + x0..y * w + x1]);
    }
    ColorImage::new([x1 - x0, y1 - y0], pixels)
}

/// Shrink by `factor` (0 < factor < 1) with a box filter; smaller images cost
/// an agent fewer tokens to look at.
pub fn downscale(img: &ColorImage, factor: f32) -> ColorImage {
    if !(factor > 0.0 && factor < 1.0) {
        return img.clone();
    }
    let [w, h] = img.size;
    let (nw, nh) = (((w as f32 * factor).round() as usize).max(1), ((h as f32 * factor).round() as usize).max(1));
    let mut pixels = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        let (sy0, sy1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1).min(h));
        for x in 0..nw {
            let (sx0, sx1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1).min(w));
            let mut acc = [0u32; 4];
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let c = img.pixels[sy * w + sx];
                    for (i, v) in [c.r(), c.g(), c.b(), c.a()].into_iter().enumerate() {
                        acc[i] += v as u32;
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)).max(1) as u32;
            pixels.push(Color32::from_rgba_premultiplied((acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8, (acc[3] / n) as u8));
        }
    }
    ColorImage::new([nw, nh], pixels)
}

/// Write `img` as a PNG, creating parent folders.
pub fn save_png(path: &Path, img: &ColorImage) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    w.write_image_data(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::Mesh;
    use egui::{Pos2, pos2};

    fn quad(rect: Rect, color: Color32) -> ClippedPrimitive {
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(rect, color);
        ClippedPrimitive { clip_rect: Rect::EVERYTHING, primitive: Primitive::Mesh(mesh) }
    }

    #[test]
    fn fills_rects_without_seams() {
        let half = Color32::from_rgba_premultiplied(0, 0, 100, 128);
        let prims = [quad(Rect::from_min_max(Pos2::ZERO, pos2(8.0, 8.0)), half)];
        let img = render(&prims, &Textures::default(), [10, 10], 1.0, Color32::BLACK);
        // Every pixel of the translucent square blended exactly once (no
        // darker/brighter diagonal where its two triangles meet).
        let inside: Vec<Color32> = (0..8).flat_map(|y| (0..8).map(move |x| (x, y))).map(|(x, y)| img.pixels[y * 10 + x]).collect();
        assert!(inside.iter().all(|c| *c == inside[0]), "{inside:?}");
        assert_eq!(inside[0].b(), 100);
        assert_eq!(img.pixels[9 * 10 + 9], Color32::BLACK);
    }

    #[test]
    fn repeating_textures_tile() {
        let strip = |repeat| Texture { size: [2, 1], pixels: vec![Color32::BLACK, Color32::WHITE], linear: false, repeat };
        // Past the right edge: a repeating texture starts over, a clamped one holds its edge.
        assert_eq!(strip(true).sample(1.25, 0.5), px(Color32::BLACK));
        assert_eq!(strip(true).sample(-0.25, 0.5), px(Color32::WHITE));
        assert_eq!(strip(false).sample(1.25, 0.5), px(Color32::WHITE));
        assert_eq!(strip(false).sample(-0.25, 0.5), px(Color32::BLACK));
    }

    #[test]
    fn respects_clip_and_scale() {
        let mut p = quad(Rect::from_min_max(Pos2::ZERO, pos2(10.0, 10.0)), Color32::WHITE);
        p.clip_rect = Rect::from_min_max(Pos2::ZERO, pos2(2.0, 10.0));
        let img = render(&[p], &Textures::default(), [20, 20], 2.0, Color32::BLACK);
        assert_eq!(img.pixels[0], Color32::WHITE);
        assert_eq!(img.pixels[3], Color32::WHITE);
        assert_eq!(img.pixels[4], Color32::BLACK, "clipped at x = 2 points = 4 px");
        let small = downscale(&img, 0.5);
        assert_eq!(small.size, [10, 10]);
        let part = crop(&img, Rect::from_min_max(pos2(2.0, 2.0), pos2(6.0, 5.0)));
        assert_eq!(part.size, [4, 3]);
    }
}
