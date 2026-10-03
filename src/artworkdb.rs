//! Reader for `iPod_Control/Artwork/ArtworkDB` and its `.ithmb` thumbnail files.
//!
//! ```text
//! mhfd
//! └─ mhsd type=1 → mhli → mhii …   one image entry per track (song_id = track dbid)
//!                          └─ mhod type=2 → mhni   one thumbnail per size
//!                                           └─ mhod type=3   ":F1028_1.ithmb"
//! ```
//! Thumbnails are raw RGB565 little-endian pixels at `offset` in the named file.

use crate::bytes::{Chunk, utf16le};
use anyhow::{Context, Result, bail};
use image::{DynamicImage, RgbImage};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Thumb {
    pub format_id: u32,
    pub file: String,
    pub offset: u32,
    pub size: u32,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Default)]
pub struct ArtworkDb {
    pub dir: PathBuf,
    /// Track dbid → available thumbnails.
    pub by_track: HashMap<u64, Vec<Thumb>>,
}

pub fn read(artwork_dir: &Path) -> Result<ArtworkDb> {
    let path = artwork_dir.join("ArtworkDB");
    let buf = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut db = parse(&buf)?;
    db.dir = artwork_dir.to_path_buf();
    Ok(db)
}

pub fn parse(buf: &[u8]) -> Result<ArtworkDb> {
    let mhfd = Chunk::at(buf, 0)?;
    mhfd.expect(b"mhfd")?;
    let mut db = ArtworkDb::default();

    let mut sd = mhfd.first_child()?;
    for _ in 0..mhfd.u32(0x14) {
        sd.expect(b"mhsd")?;
        if sd.u16(0x0C) == 1 {
            let mhli = sd.first_child()?;
            mhli.expect(b"mhli")?;
            let mut off = mhli.off + mhli.header_len();
            for _ in 0..mhli.total_len() {
                let ii = Chunk::at(buf, off)?;
                ii.expect(b"mhii")?;
                let thumbs = parse_image(ii)?;
                if !thumbs.is_empty() {
                    db.by_track.insert(ii.u64(0x14), thumbs);
                }
                off = ii.end();
            }
        }
        if sd.end() >= buf.len() {
            break;
        }
        sd = Chunk::at(buf, sd.end())?;
    }
    Ok(db)
}

fn parse_image(ii: Chunk) -> Result<Vec<Thumb>> {
    let mut thumbs = Vec::new();
    let mut off = ii.off + ii.header_len();
    for _ in 0..ii.u32(0x0C) {
        let od = Chunk::at(ii.buf, off)?;
        od.expect(b"mhod")?;
        if od.u16(0x0C) == 2 {
            let ni = od.first_child()?;
            ni.expect(b"mhni")?;
            let file = if ni.u32(0x0C) > 0 {
                let name = ni.first_child()?;
                name.expect(b"mhod")?;
                let len = name.u32(0x18) as usize;
                let raw = name.slice(0x24, len).unwrap_or_default();
                let s = match name.u32(0x1C) {
                    2 => utf16le(raw),
                    _ => String::from_utf8_lossy(raw).into_owned(),
                };
                s.trim_start_matches(':').to_string()
            } else {
                String::new()
            };
            thumbs.push(Thumb {
                format_id: ni.u32(0x10),
                file,
                offset: ni.u32(0x14),
                size: ni.u32(0x18),
                height: ni.u16(0x20),
                width: ni.u16(0x22),
            });
        }
        off = od.end();
    }
    Ok(thumbs)
}

impl ArtworkDb {
    /// The largest thumbnail stored for a track.
    pub fn best_thumb(&self, dbid: u64) -> Option<&Thumb> {
        self.by_track.get(&dbid)?.iter().max_by_key(|t| t.size)
    }

    pub fn load(&self, thumb: &Thumb) -> Result<DynamicImage> {
        let path = self.dir.join(&thumb.file);
        let mut f = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        f.seek(SeekFrom::Start(thumb.offset as u64))?;
        let mut raw = vec![0u8; thumb.size as usize];
        f.read_exact(&mut raw)?;
        decode_rgb565(&raw, thumb)
    }
}

/// The stored pixel buffer is the format's full size; `width`/`height` is the
/// visible image inside it.
fn decode_rgb565(raw: &[u8], thumb: &Thumb) -> Result<DynamicImage> {
    let (fw, fh) = format_dims(thumb.format_id)
        .unwrap_or((thumb.width as u32, thumb.height as u32));
    if fw == 0 || fh == 0 || raw.len() < (fw * fh * 2) as usize {
        bail!("thumbnail format {} has unexpected size", thumb.format_id);
    }
    let stride = raw.len() as u32 / fh / 2;
    let w = (thumb.width as u32).clamp(1, fw);
    let h = (thumb.height as u32).clamp(1, fh);
    let img = RgbImage::from_fn(w, h, |x, y| {
        let i = ((y * stride + x) * 2) as usize;
        let p = u16::from_le_bytes([raw[i], raw[i + 1]]);
        let r = ((p >> 11) & 0x1F) as u8;
        let g = ((p >> 5) & 0x3F) as u8;
        let b = (p & 0x1F) as u8;
        image::Rgb([(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)])
    });
    Ok(DynamicImage::ImageRgb8(img))
}

/// Pixel dimensions of known cover-art formats.
pub fn format_dims(format_id: u32) -> Option<(u32, u32)> {
    Some(match format_id {
        1028 => (100, 100), // iPod Video: list view
        1029 => (200, 200), // iPod Video: now playing
        1055 => (128, 128), // iPod Classic
        1060 => (320, 320), // iPod Classic
        1061 => (56, 56),   // iPod Classic
        _ => return None,
    })
}
