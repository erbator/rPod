//! Adding cover art to ArtworkDB and its `.ithmb` thumbnail files.

use crate::artworkdb::format_dims;
use crate::bytes::{Chunk, utf16le};
use anyhow::{Context, Result};
use image::DynamicImage;
use image::imageops::FilterType;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub struct NewImage {
    pub track_dbid: u64,
    /// Shared between tracks of an album, so each image is encoded once.
    pub image: Arc<DynamicImage>,
    /// Byte size of the source image, stored for the iPod's bookkeeping.
    pub src_size: u32,
}

/// A thumbnail format this iPod uses, from the ArtworkDB file list (mhlf).
struct Format {
    id: u32,
    width: u32,
    height: u32,
    /// The ithmb file new thumbnails are appended to.
    file: String,
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn set32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn header(tag: &[u8; 4], len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    b[..4].copy_from_slice(tag);
    set32(&mut b, 4, len as u32);
    b
}

pub fn encode_rgb565(img: &DynamicImage, w: u32, h: u32) -> Vec<u8> {
    let rgb = img.resize_to_fill(w, h, FilterType::Lanczos3).to_rgb8();
    let mut out = Vec::with_capacity((w * h * 2) as usize);
    for p in rgb.pixels() {
        let [r, g, b] = p.0;
        let v = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn mhni(fmt: &Format, offset: u32, size: u32) -> Vec<u8> {
    let name: Vec<u8> = format!(":{}", fmt.file).encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut s = header(b"mhod", 0x18);
    s[0x0C..0x0E].copy_from_slice(&3u16.to_le_bytes());
    put32(&mut s, name.len() as u32);
    put32(&mut s, 2); // UTF-16
    put32(&mut s, 0);
    s.extend_from_slice(&name);
    while s.len() % 4 != 0 {
        s.push(0);
    }
    let total = s.len() as u32;
    set32(&mut s, 8, total);

    let mut ni = header(b"mhni", 0x4C);
    set32(&mut ni, 0x0C, 1);
    set32(&mut ni, 0x10, fmt.id);
    set32(&mut ni, 0x14, offset);
    set32(&mut ni, 0x18, size);
    ni[0x20..0x22].copy_from_slice(&(fmt.height as u16).to_le_bytes());
    ni[0x22..0x24].copy_from_slice(&(fmt.width as u16).to_le_bytes());
    ni.extend(s);
    let total = ni.len() as u32;
    set32(&mut ni, 8, total);

    let mut od = header(b"mhod", 0x18);
    od[0x0C..0x0E].copy_from_slice(&2u16.to_le_bytes());
    od.extend(ni);
    let total = od.len() as u32;
    set32(&mut od, 8, total);
    od
}

fn mhii(id: u32, dbid: u64, src_size: u32, thumbs: &[Vec<u8>]) -> Vec<u8> {
    let mut ii = header(b"mhii", 0x98);
    set32(&mut ii, 0x0C, thumbs.len() as u32);
    set32(&mut ii, 0x10, id);
    ii[0x14..0x1C].copy_from_slice(&dbid.to_le_bytes());
    set32(&mut ii, 0x30, src_size);
    for t in thumbs {
        ii.extend_from_slice(t);
    }
    let total = ii.len() as u32;
    set32(&mut ii, 8, total);
    ii
}

/// Scan the existing database for formats, the newest ithmb file per format,
/// and the highest image id.
fn survey(buf: &[u8]) -> Result<(Vec<Format>, u32)> {
    let mhfd = Chunk::at(buf, 0)?;
    mhfd.expect(b"mhfd")?;
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    let mut latest: HashMap<u32, (u32, String)> = HashMap::new();
    let mut max_id = 0;

    let mut sd = mhfd.first_child()?;
    for i in 0..mhfd.u32(0x14) {
        sd.expect(b"mhsd")?;
        let list = sd.first_child()?;
        let mut off = list.off + list.header_len();
        match sd.u16(0x0C) {
            1 => {
                for _ in 0..list.total_len() {
                    let ii = Chunk::at(buf, off)?;
                    max_id = max_id.max(ii.u32(0x10));
                    let mut o = ii.off + ii.header_len();
                    for _ in 0..ii.u32(0x0C) {
                        let od = Chunk::at(buf, o)?;
                        if od.u16(0x0C) == 2 {
                            let ni = od.first_child()?;
                            let name = ni.first_child()?;
                            let raw = name.slice(0x24, name.u32(0x18) as usize).unwrap_or_default();
                            let file = utf16le(raw).trim_start_matches(':').to_string();
                            let n = file_number(&file);
                            let e = latest.entry(ni.u32(0x10)).or_insert((0, String::new()));
                            if n >= e.0 {
                                *e = (n, file);
                            }
                        }
                        o = od.end();
                    }
                    off = ii.end();
                }
            }
            3 => {
                for _ in 0..list.total_len() {
                    let f = Chunk::at(buf, off)?;
                    f.expect(b"mhif")?;
                    sizes.push((f.u32(0x10), f.u32(0x14)));
                    off = f.end();
                }
            }
            _ => {}
        }
        if i + 1 < mhfd.u32(0x14) {
            sd = Chunk::at(buf, sd.end())?;
        }
    }

    let formats = sizes
        .into_iter()
        .filter_map(|(id, size)| {
            let (w, h) = format_dims(id)?;
            // Formats with row padding (some Classic sizes) aren't written yet.
            (w * h * 2 == size).then(|| Format {
                id,
                width: w,
                height: h,
                file: latest.get(&id).map(|(_, f)| f.clone()).unwrap_or_else(|| format!("F{id}_1.ithmb")),
            })
        })
        .collect();
    Ok((formats, max_id.max(mhfd.u32(0x1C).saturating_sub(1))))
}

/// "F1029_2.ithmb" → 2
fn file_number(name: &str) -> u32 {
    name.rsplit_once('_')
        .and_then(|(_, r)| r.split('.').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Append thumbnails to the ithmb files in `art_dir` and return the new
/// ArtworkDB bytes plus the image id assigned to each input (same order).
/// The caller writes the returned database; until it does, the appended
/// pixels are unreferenced and harmless.
pub fn add_images(art_dir: &Path, orig: &[u8], images: &[NewImage]) -> Result<(Vec<u8>, Vec<u32>)> {
    let (formats, max_id) = survey(orig)?;
    let mut ids = Vec::with_capacity(images.len());
    let mut entries = Vec::new();

    // Encode each distinct image once (in parallel), then append per file
    // sequentially. Tracks of one album share an Arc'd image.
    use rayon::prelude::*;
    let mut unique: Vec<&Arc<DynamicImage>> = Vec::new();
    let slot: Vec<usize> = images
        .iter()
        .map(|im| match unique.iter().position(|u| Arc::ptr_eq(u, &im.image)) {
            Some(i) => i,
            None => {
                unique.push(&im.image);
                unique.len() - 1
            }
        })
        .collect();
    let distinct: Vec<Vec<Vec<u8>>> = unique
        .par_iter()
        .map(|img| formats.iter().map(|f| encode_rgb565(img, f.width, f.height)).collect())
        .collect();
    let encoded: Vec<&Vec<Vec<u8>>> = slot.iter().map(|&i| &distinct[i]).collect();

    let mut files = Vec::new();
    for f in &formats {
        let path = art_dir.join(&f.file);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len() as u32;
        files.push((file, len));
    }

    for (i, (im, thumbs)) in images.iter().zip(encoded).enumerate() {
        let id = max_id + 1 + i as u32;
        let mut mhnis = Vec::new();
        for ((f, px), (file, len)) in formats.iter().zip(thumbs).zip(files.iter_mut()) {
            file.write_all(px)?;
            mhnis.push(mhni(f, *len, px.len() as u32));
            *len += px.len() as u32;
        }
        entries.extend(mhii(id, im.track_dbid, im.src_size, &mhnis));
        ids.push(id);
    }
    for (file, _) in &files {
        file.sync_all()?;
    }

    Ok((splice(orig, &entries, images.len(), max_id + 1 + images.len() as u32)?, ids))
}

/// Insert new mhii entries at the end of the image list.
fn splice(orig: &[u8], entries: &[u8], count: usize, next_id: u32) -> Result<Vec<u8>> {
    let mhfd = Chunk::at(orig, 0)?;
    let mut sd = mhfd.first_child()?;
    for i in 0..mhfd.u32(0x14) {
        if sd.u16(0x0C) == 1 {
            let mhli = sd.first_child()?;
            let mut out = Vec::with_capacity(orig.len() + entries.len());
            out.extend_from_slice(&orig[..sd.end()]);
            out.extend_from_slice(entries);
            out.extend_from_slice(&orig[sd.end()..]);
            let grow = entries.len() as u32;
            set32(&mut out, 8, mhfd.u32(8) + grow);
            set32(&mut out, 0x1C, next_id);
            set32(&mut out, sd.off + 8, sd.u32(8) + grow);
            set32(&mut out, mhli.off + 8, mhli.u32(8) + count as u32);
            return Ok(out);
        }
        if i + 1 < mhfd.u32(0x14) {
            sd = Chunk::at(orig, sd.end())?;
        }
    }
    anyhow::bail!("ArtworkDB has no image list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artworkdb;

    #[test]
    fn add_cover_round_trips() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let tmp = std::env::temp_dir().join(format!("rpod-art-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        for f in std::fs::read_dir(format!("{root}/iPod_Control/Artwork")).unwrap() {
            let f = f.unwrap();
            std::fs::copy(f.path(), tmp.join(f.file_name())).unwrap();
        }
        let orig = std::fs::read(tmp.join("ArtworkDB")).unwrap();
        let before = artworkdb::parse(&orig).unwrap();

        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(300, 300, |x, _| image::Rgb([x as u8, 0, 255])));
        let (db, ids) =
            add_images(&tmp, &orig, &[NewImage { track_dbid: 0xABCDEF, image: Arc::new(img), src_size: 999 }]).unwrap();
        std::fs::write(tmp.join("ArtworkDB"), &db).unwrap();

        let after = artworkdb::read(&tmp).unwrap();
        assert_eq!(after.by_track.len(), before.by_track.len() + 1);
        assert!(ids[0] > 1385);
        let thumb = after.best_thumb(0xABCDEF).unwrap();
        assert_eq!((thumb.width, thumb.height), (200, 200));
        let px = after.load(thumb).unwrap().to_rgb8();
        assert!(px.get_pixel(100, 100)[2] > 240, "blue channel survives RGB565");
        // Existing art still decodes.
        let (dbid, _) = before.by_track.iter().next().unwrap();
        after.load(after.best_thumb(*dbid).unwrap()).unwrap();
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
