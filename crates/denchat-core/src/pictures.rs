//! Pictures: profile pictures and room pictures.
//!
//! A picture is a small image file named by its hash ([`id_of`], see
//! [`denchat_proto::valid_picture_id`]), so the same id always means the
//! same bytes and anyone can check what they were sent. Rooms refer to
//! pictures by id (a member's `picture`, a profile's, the room's own); the
//! files travel separately (`put_picture`, `get_pictures`, `picture`) and
//! every member keeps the ones its room uses in its copy of the room
//! (`rooms/<id>/pictures/`), so whoever hosts next can serve them.
//!
//! Your own profile picture lives in `<data>/picture.<ext>` and goes with you
//! to every room.
//!
//! With the `images` feature, [`prepare`] turns any common image into a
//! square [`SIZE`] px PNG or JPEG and [`decode`] reads pictures for display.
//! Without it, pictures pass through as they are (if small enough).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use denchat_proto::{MAX_PICTURE, valid_picture_id};

/// Side of a prepared picture, in pixels.
pub const SIZE: u32 = 256;

/// A picture's id: the BLAKE3 hash of its bytes, in hex.
pub fn id_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// What kind of image `bytes` is, from its first bytes: `png`, `jpeg`, `gif`
/// or `webp`. Only these travel.
pub fn kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// File extension for a picture's kind.
fn ext(bytes: &[u8]) -> &'static str {
    match kind(bytes) {
        Some("jpeg") => "jpg",
        Some(k) => k,
        None => "bin",
    }
}

/// Whether `bytes` may travel as a picture: a known image kind, not too big.
pub fn check(bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.len() > MAX_PICTURE {
        bail!("pictures are limited to {} KB (this one is {} KB)", MAX_PICTURE / 1024, bytes.len().div_ceil(1024));
    }
    if kind(bytes).is_none() {
        bail!("not a PNG, JPEG, GIF or WebP image");
    }
    Ok(())
}

/// Bytes from the wire: base64 that must hash to `id` and be a picture.
pub fn from_wire(id: &str, data: &str) -> anyhow::Result<Vec<u8>> {
    if !valid_picture_id(id) {
        bail!("bad picture id");
    }
    if data.len() > MAX_PICTURE.div_ceil(3) * 4 + 4 {
        bail!("picture too large");
    }
    let bytes = data_encoding::BASE64.decode(data.as_bytes())?;
    check(&bytes)?;
    if id_of(&bytes) != id {
        bail!("picture doesn't match its id");
    }
    Ok(bytes)
}

pub fn to_wire(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

/// Make a picture from an image someone chose: cropped to a centred square,
/// scaled to [`SIZE`] px, saved as PNG when it has see-through parts and as
/// JPEG otherwise. Photos from phones are turned upright first.
#[cfg(feature = "images")]
pub fn prepare(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Cursor;

    use anyhow::Context;
    use image::imageops::FilterType;
    use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

    let mut reader = ImageReader::new(Cursor::new(raw)).with_guessed_format().context("reading the image")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(1 << 30);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("not an image Den can read (PNG, JPEG, GIF, WebP, BMP)")?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).context("decoding the image")?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let side = img.width().min(img.height()).min(SIZE);
    if side == 0 {
        bail!("the image is empty");
    }
    let img = img.resize_to_fill(side, side, FilterType::Lanczos3);
    let see_through = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p[3] < 255);
    let mut out = Vec::new();
    if see_through {
        img.to_rgba8().write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)?;
    }
    if !see_through || out.len() > MAX_PICTURE {
        out.clear();
        let mut quality = 88;
        loop {
            out.clear();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&img.to_rgb8())?;
            if out.len() <= MAX_PICTURE || quality <= 40 {
                break;
            }
            quality -= 12;
        }
    }
    check(&out)?;
    Ok(out)
}

/// Without image support, pictures must already be small images.
#[cfg(not(feature = "images"))]
pub fn prepare(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    check(raw)?;
    Ok(raw.to_vec())
}

/// Decode a picture for display: `(width, height, RGBA pixels)`. Refuses
/// anything larger than 2048 px a side (pictures are [`SIZE`] px; this is
/// for files from the network).
#[cfg(feature = "images")]
pub fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use image::{ImageReader, Limits};
    kind(bytes)?;
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(64 << 20);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    Some((img.width(), img.height(), img.into_raw()))
}

/// Pictures on disk (one folder, files named `<id>.<ext>`) with the recently
/// used ones in memory.
pub struct Store {
    dir: Option<PathBuf>,
    mem: HashMap<String, Arc<[u8]>>,
}

/// Pictures a store keeps in memory.
const MEM: usize = 128;

impl Store {
    /// A store in `dir` (created on first write), or memory-only with `None`.
    pub fn new(dir: Option<PathBuf>) -> Store {
        Store { dir, mem: HashMap::new() }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn get(&mut self, id: &str) -> Option<Arc<[u8]>> {
        if let Some(b) = self.mem.get(id) {
            return Some(b.clone());
        }
        let bytes: Arc<[u8]> = read(self.dir.as_deref()?, id)?.into();
        self.remember(id, bytes.clone());
        Some(bytes)
    }

    pub fn has(&mut self, id: &str) -> bool {
        self.mem.contains_key(id) || self.dir.as_deref().is_some_and(|d| find(d, id).is_some())
    }

    /// Keep `bytes` (already checked) under their id. Returns the id.
    pub fn put(&mut self, bytes: &[u8]) -> String {
        let id = id_of(bytes);
        if !self.mem.contains_key(&id)
            && let Some(dir) = &self.dir
            && find(dir, &id).is_none()
            && let Err(e) = write(dir, &id, bytes)
        {
            tracing::warn!("saving picture {id} in {}: {e:#}", dir.display());
        }
        self.remember(&id, bytes.into());
        id
    }

    fn remember(&mut self, id: &str, bytes: Arc<[u8]>) {
        if self.mem.len() >= MEM
            && let Some(k) = self.mem.keys().next().cloned()
        {
            self.mem.remove(&k);
        }
        self.mem.insert(id.to_owned(), bytes);
    }

    /// Delete files on disk the room no longer uses.
    pub fn prune(&mut self, keep: &HashSet<String>) {
        self.mem.retain(|k, _| keep.contains(k));
        let Some(dir) = &self.dir else { return };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(stem) = Path::new(&name).file_stem().and_then(|s| s.to_str()) else { continue };
            if valid_picture_id(stem) && !keep.contains(stem) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// The file holding picture `id` in `dir`, if any.
fn find(dir: &Path, id: &str) -> Option<PathBuf> {
    if !valid_picture_id(id) {
        return None;
    }
    ["png", "jpg", "gif", "webp"].iter().map(|x| dir.join(format!("{id}.{x}"))).find(|p| p.is_file())
}

/// Picture `id` from `dir`, if it's there and intact.
pub fn read(dir: &Path, id: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(find(dir, id)?).ok()?;
    (id_of(&bytes) == id).then_some(bytes)
}

fn write(dir: &Path, id: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.{}", ext(bytes)));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ----- your own picture ------------------------------------------------------

const OWN: &str = "picture";

/// Your profile picture, from `<data>/picture.<ext>`.
pub fn load_own(data: &Path) -> Option<Vec<u8>> {
    ["png", "jpg", "gif", "webp"].iter().find_map(|x| std::fs::read(data.join(format!("{OWN}.{x}"))).ok()).filter(|b| check(b).is_ok())
}

/// Save (or with `None`, remove) your profile picture.
pub fn save_own(data: &Path, bytes: Option<&[u8]>) -> anyhow::Result<()> {
    for x in ["png", "jpg", "gif", "webp"] {
        let p = data.join(format!("{OWN}.{x}"));
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    if let Some(b) = bytes {
        std::fs::create_dir_all(data)?;
        std::fs::write(data.join(format!("{OWN}.{}", ext(b))), b)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 PNG.
    pub(crate) const DOT: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63,
        0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
        0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn ids_kinds_and_wire() {
        let id = id_of(DOT);
        assert!(denchat_proto::valid_picture_id(&id));
        assert_eq!(kind(DOT), Some("png"));
        assert_eq!(kind(b"hello"), None);
        assert_eq!(from_wire(&id, &to_wire(DOT)).unwrap(), DOT);
        // Wrong id, not an image, too big.
        assert!(from_wire(&"0".repeat(64), &to_wire(DOT)).is_err());
        assert!(from_wire(&id_of(b"hello"), &to_wire(b"hello")).is_err());
        let mut big = DOT.to_vec();
        big.resize(MAX_PICTURE + 1, 0);
        assert!(check(&big).is_err());
    }

    #[test]
    fn store_keeps_files_by_id_and_prunes() {
        let dir = std::env::temp_dir().join(format!("den-pictures-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::new(Some(dir.clone()));
        let id = s.put(DOT);
        assert!(dir.join(format!("{id}.png")).is_file());
        // A fresh store (another process, the next host) finds it on disk.
        let mut again = Store::new(Some(dir.clone()));
        assert!(again.has(&id));
        assert_eq!(&*again.get(&id).unwrap(), DOT);
        again.prune(&HashSet::new());
        assert!(!Store::new(Some(dir.clone())).has(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "images")]
    #[test]
    fn prepare_makes_small_squares() {
        let img = image::RgbImage::from_fn(900, 500, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let out = prepare(&png).unwrap();
        // Opaque: a JPEG.
        assert_eq!(kind(&out), Some("jpeg"));
        let (w, h, px) = decode(&out).unwrap();
        assert_eq!((w, h, px.len()), (SIZE, SIZE, (SIZE * SIZE * 4) as usize));
        // See-through stays PNG; small images aren't scaled up.
        let clear = image::RgbaImage::from_fn(40, 60, |x, _| image::Rgba([200, 10, 10, if x < 20 { 0 } else { 255 }]));
        let mut png = Vec::new();
        clear.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let out = prepare(&png).unwrap();
        assert_eq!(kind(&out), Some("png"));
        assert_eq!(decode(&out).unwrap().0, 40);
        assert!(prepare(b"not an image").is_err());
    }
}
