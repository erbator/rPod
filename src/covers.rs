//! Applying a chosen cover image to tracks on the iPod.

use crate::artwrite::{self, NewImage};
use crate::dbwrite::{self, TrackEdit};
use crate::edit::Report;
use crate::itunesdb::{self, Track};
use crate::{store, tags};
use anyhow::{Context, Result, bail};
use image::DynamicImage;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Longest side of the JPEG embedded into audio files. Plenty for any
/// player, without adding megabytes to every track.
const EMBED_SIZE: u32 = 1000;

/// One cover for a group of tracks (usually an album).
pub struct Assignment {
    pub tracks: Vec<Track>,
    /// Encoded image (JPEG/PNG/…) as downloaded or read from disk.
    pub image: Vec<u8>,
}

fn embed_jpeg(img: &DynamicImage) -> Result<Vec<u8>> {
    let img = if img.width().max(img.height()) > EMBED_SIZE { img.resize(EMBED_SIZE, EMBED_SIZE, image::imageops::FilterType::Lanczos3) } else { img.clone() };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img.to_rgb8())?;
    Ok(out)
}

/// Write covers to the iPod's artwork database, link the tracks to them, and
/// (optionally) embed them in the audio files.
pub fn apply(root: &Path, jobs: &[Assignment], write_files: bool) -> Result<Report> {
    let art_dir = store::artwork_dir(root);
    let art_path = art_dir.join("ArtworkDB");
    if !art_path.exists() {
        bail!("this iPod has no ArtworkDB yet; add a song with cover art first");
    }

    use rayon::prelude::*;
    let decoded: Vec<(Arc<DynamicImage>, Option<Vec<u8>>, u32)> = jobs
        .par_iter()
        .map(|j| {
            let img = image::load_from_memory(&j.image).context("decoding cover image")?;
            let jpeg = if write_files { Some(embed_jpeg(&img)?) } else { None };
            Ok((Arc::new(img.thumbnail(480, 480)), jpeg, j.image.len() as u32))
        })
        .collect::<Result<_>>()?;

    store::backup(root)?;

    let mut images = Vec::new();
    for (j, (img, _, size)) in jobs.iter().zip(&decoded) {
        for t in &j.tracks {
            images.push(NewImage { track_dbid: t.dbid, image: img.clone(), src_size: *size });
        }
    }
    let orig_art = std::fs::read(&art_path)?;
    let (art_db, ids) = artwrite::replace_images(&art_dir, &orig_art, &images)?;

    let edits: HashMap<u32, TrackEdit> = jobs
        .iter()
        .flat_map(|j| j.tracks.iter())
        .zip(ids.iter().zip(&images))
        .map(|(t, (&id, im))| (t.id, TrackEdit { artwork: Some((id, im.src_size)), ..Default::default() }))
        .collect();
    let db_path = store::itunesdb_path(root);
    let orig = std::fs::read(&db_path)?;
    let parsed = itunesdb::parse(&orig)?;
    let out = dbwrite::edit_tracks(&orig, &parsed, &edits)?;
    let check = itunesdb::parse(&out)?;
    if check.tracks.len() != parsed.tracks.len()
        || check.tracks.iter().filter(|t| edits.contains_key(&t.id)).any(|t| !t.has_artwork)
    {
        bail!("verification failed after linking covers");
    }

    // ArtworkDB first: until iTunesDB points at the new images they're unused.
    store::atomic_write(&art_path, &art_db)?;
    store::atomic_write(&db_path, &out)?;

    let mut file_errors = Vec::new();
    if write_files {
        for (j, (_, jpeg, _)) in jobs.iter().zip(&decoded) {
            let Some(jpeg) = jpeg else { continue };
            for t in &j.tracks {
                if let Err(e) = tags::embed_cover(&root.join(&t.location), jpeg) {
                    file_errors.push(format!("{}: {e:#}", t.title));
                }
            }
        }
    }
    Ok(Report { tracks: edits.len(), file_errors })
}

/// Flip and save the "embed covers in song files" setting.
pub fn toggle_embed_covers() -> bool {
    crate::import::Settings::update(|s| s.embed_covers = !s.embed_covers).embed_covers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artworkdb;

    #[test]
    fn apply_cover_to_album() {
        let Ok(src) = std::env::var("RPOD_TEST_IPOD") else { return };
        let root = std::env::temp_dir().join(format!("rpod-covers-{}", std::process::id()));
        for dir in ["iTunes", "Artwork"] {
            std::fs::create_dir_all(root.join("iPod_Control").join(dir)).unwrap();
            for f in std::fs::read_dir(format!("{src}/iPod_Control/{dir}")).unwrap() {
                let f = f.unwrap();
                std::fs::copy(f.path(), root.join("iPod_Control").join(dir).join(f.file_name())).unwrap();
            }
        }
        let _env = crate::store::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: tests that set XDG_DATA_HOME serialize on TEST_ENV_LOCK.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };

        let db = itunesdb::read(&store::itunesdb_path(&root)).unwrap();
        let art = artworkdb::read(&store::artwork_dir(&root)).unwrap();
        // Prefer tracks without art; fall back to replacing existing covers.
        let mut bare: Vec<Track> = db.tracks.iter().filter(|t| !art.by_track.contains_key(&t.dbid)).take(3).cloned().collect();
        if bare.is_empty() {
            bare = db.tracks.iter().take(3).cloned().collect();
        }
        let new_entries = bare.iter().filter(|t| !art.by_track.contains_key(&t.dbid)).count();
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1200, 1200, image::Rgb([0, 200, 0])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let report = apply(&root, &[Assignment { tracks: bare.clone(), image: png }], false).unwrap();
        assert_eq!(report.tracks, bare.len());

        let db2 = itunesdb::read(&store::itunesdb_path(&root)).unwrap();
        let art2 = artworkdb::read(&store::artwork_dir(&root)).unwrap();
        for t in &bare {
            assert!(db2.tracks.iter().find(|x| x.id == t.id).unwrap().has_artwork);
            let px = art2.load(art2.best_thumb(t.dbid).unwrap()).unwrap().to_rgb8();
            assert!(px.get_pixel(50, 50)[1] > 190);
        }
        assert_eq!(art2.by_track.len(), art.by_track.len() + new_entries);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
