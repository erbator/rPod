//! Writing edits into the audio files' own tags, so files copied off the iPod
//! later carry the same metadata the iPod shows.

use crate::dbwrite::TrackEdit;
use crate::itunesdb::Track;
use anyhow::{Context, Result};
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{Tag, TagType, items::Timestamp};
use std::path::Path;

pub fn write(path: &Path, e: &TrackEdit) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("reading {}", path.display()))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).expect("tag inserted above");
    apply(tag, e);
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("writing tags to {}", path.display()))?;
    Ok(())
}

/// Write everything the iPod knows about `t` into a file copied off it, in
/// the tags PC players read best: ID3v2.3 for MP3 (Windows and older players
/// misread v2.4), iTunes atoms for M4A. Leftover ID3v1/APE tags go, so no
/// player shows stale values from them. `cover` (a JPEG) replaces the file's
/// own cover only when that is missing or smaller.
pub fn write_track(path: &Path, t: &Track, cover: Option<&[u8]>) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("reading {}", path.display()))?;
    let tag_type = file.primary_tag_type();
    let stale: Vec<TagType> =
        [TagType::Id3v1, TagType::Ape].into_iter().filter(|&tt| tt != tag_type && file.contains_tag_type(tt)).collect();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).expect("tag inserted above");
    apply(tag, &full_edit(t));
    if let Some(jpeg) = cover.filter(|jpeg| own_cover_smaller(tag, jpeg)) {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.remove_picture_type(PictureType::Other);
        tag.push_picture(Picture::unchecked(jpeg.to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Jpeg).build());
    }
    let options = WriteOptions::default().use_id3v23(true);
    tag.save_to_path(path, options).with_context(|| format!("writing tags to {}", path.display()))?;
    // `remove_from_path` opens the file read-only and fails, so hand it a writable one.
    for tt in stale {
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        tt.remove_from(&mut f, options).with_context(|| format!("removing old tags from {}", path.display()))?;
    }
    Ok(())
}

/// Whether the file's own front cover is missing or smaller than `jpeg`.
/// One that can't be measured counts as bigger, so it's kept.
fn own_cover_smaller(tag: &Tag, jpeg: &[u8]) -> bool {
    let side = |data: &[u8]| {
        let reader = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().ok()?;
        reader.into_dimensions().ok().map(|(w, h)| w.min(h))
    };
    let own = tag
        .pictures()
        .iter()
        // MP4 has no picture types; its covers read back as Other.
        .filter(|p| matches!(p.pic_type(), PictureType::CoverFront | PictureType::Other))
        .map(|p| side(p.data()).unwrap_or(u32::MAX))
        .max();
    own.is_none_or(|own| side(jpeg).is_some_and(|ours| own < ours))
}

/// Every tag field set from `t`; empty values clear the field.
fn full_edit(t: &Track) -> TrackEdit {
    TrackEdit {
        title: Some(t.title.clone()),
        artist: Some(t.artist.clone()),
        album: Some(t.album.clone()),
        album_artist: Some(t.album_artist.clone()),
        genre: Some(t.genre.clone()),
        composer: Some(t.composer.clone()),
        comment: Some(t.comment.clone()),
        year: Some(t.year),
        track_no: Some(t.track_no),
        track_total: Some(t.track_total),
        disc_no: Some(t.disc_no),
        disc_total: Some(t.disc_total),
        compilation: Some(t.compilation),
        ..Default::default()
    }
}

fn apply(tag: &mut Tag, e: &TrackEdit) {

    macro_rules! text {
        ($field:ident, $set:ident, $remove:ident) => {
            if let Some(v) = &e.$field {
                if v.is_empty() { tag.$remove() } else { tag.$set(v.clone()) }
            }
        };
    }
    text!(title, set_title, remove_title);
    text!(artist, set_artist, remove_artist);
    text!(album, set_album, remove_album);
    text!(genre, set_genre, remove_genre);
    text!(comment, set_comment, remove_comment);

    for (value, key) in [(&e.album_artist, ItemKey::AlbumArtist), (&e.composer, ItemKey::Composer)] {
        if let Some(v) = value {
            if v.is_empty() {
                tag.remove_key(key);
            } else {
                tag.insert_text(key, v.clone());
            }
        }
    }

    macro_rules! number {
        ($field:ident, $set:ident, $remove:ident) => {
            if let Some(v) = e.$field {
                if v == 0 { tag.$remove() } else { tag.$set(v) }
            }
        };
    }
    number!(track_no, set_track, remove_track);
    number!(track_total, set_track_total, remove_track_total);
    number!(disc_no, set_disk, remove_disk);
    number!(disc_total, set_disk_total, remove_disk_total);

    if let Some(year) = e.year {
        if year == 0 {
            tag.remove_date();
        } else {
            tag.set_date(Timestamp { year: year as u16, ..Default::default() });
        }
    }
    if let Some(c) = e.compilation {
        if c {
            tag.insert_text(ItemKey::FlagCompilation, "1".into());
        } else {
            tag.remove_key(ItemKey::FlagCompilation);
        }
    }
}

/// Replace the file's front cover with a JPEG.
pub fn embed_cover(path: &Path, jpeg: &[u8]) -> Result<()> {
    let mut file = lofty::read_from_path(path).with_context(|| format!("reading {}", path.display()))?;
    let tag_type = file.primary_tag_type();
    if file.tag(tag_type).is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.tag_mut(tag_type).expect("tag inserted above");
    tag.remove_picture_type(PictureType::CoverFront);
    // Formats without picture types (MP4) keep "Other"; drop those too.
    tag.remove_picture_type(PictureType::Other);
    tag.push_picture(Picture::unchecked(jpeg.to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Jpeg).build());
    tag.save_to_path(path, WriteOptions::default())
        .with_context(|| format!("writing cover to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs ffmpeg to make sample files; skipped without it.
    #[test]
    fn writes_mp3_and_m4a() {
        let dir = std::env::temp_dir().join(format!("rpod-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, codec) in [("a.mp3", "libmp3lame"), ("b.m4a", "aac"), ("c.m4a", "alac")] {
            let path = dir.join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-c:a", codec, "-metadata", "title=Old"])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return;
            }
            let e = TrackEdit {
                title: Some("Új cím".into()),
                album_artist: Some("The Band".into()),
                year: Some(1994),
                track_no: Some(3),
                track_total: Some(10),
                comment: Some(String::new()),
                ..Default::default()
            };
            write(&path, &e).unwrap();
            let f = lofty::read_from_path(&path).unwrap();
            let t = f.primary_tag().unwrap();
            assert_eq!(t.title().as_deref(), Some("Új cím"), "{name}");
            assert_eq!(t.get_string(ItemKey::AlbumArtist), Some("The Band"), "{name}");
            assert_eq!(t.date().map(|d| d.year), Some(1994), "{name}");
            assert_eq!((t.track(), t.track_total()), (Some(3), Some(10)), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn jpeg(side: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(side, side, image::Rgb([0, 128, 255])))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    /// Needs ffmpeg to make sample files; skipped without it.
    #[test]
    fn writes_downloaded_track() {
        let dir = std::env::temp_dir().join(format!("rpod-track-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Track {
            title: "Can I Kick It?".into(),
            artist: "A Tribe Called Quest".into(),
            album: "People's Instinctive Travels".into(),
            album_artist: "A Tribe Called Quest".into(),
            year: 1990,
            track_no: 9,
            track_total: 14,
            compilation: true,
            ..Default::default()
        };
        for (name, codec) in [("a.mp3", "libmp3lame"), ("b.m4a", "aac")] {
            let path = dir.join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-c:a", codec])
                .args(["-metadata", "title=Old", "-write_id3v1", "1"])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return;
            }
            embed_cover(&path, &jpeg(16)).unwrap();
            let had_v1 = lofty::read_from_path(&path).unwrap().contains_tag_type(TagType::Id3v1);
            assert_eq!(had_v1, name.ends_with("mp3"), "{name}");
            write_track(&path, &t, Some(&jpeg(64))).unwrap();
            let f = lofty::read_from_path(&path).unwrap();
            let tag = f.primary_tag().unwrap();
            assert_eq!(tag.title().as_deref(), Some("Can I Kick It?"), "{name}");
            assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("A Tribe Called Quest"), "{name}");
            assert_eq!((tag.track(), tag.track_total()), (Some(9), Some(14)), "{name}");
            assert!(tag.get_string(ItemKey::FlagCompilation).is_some(), "{name}");
            assert_eq!(tag.pictures().len(), 1, "{name}: the small cover is replaced");
            assert_eq!(tag.pictures()[0].data(), jpeg(64), "{name}");
            if name.ends_with("mp3") {
                assert!(!f.contains_tag_type(TagType::Id3v1), "old ID3v1 tag removed");
                assert_eq!(&std::fs::read(&path).unwrap()[..4], b"ID3\x03", "written as ID3v2.3");
            }
            // A smaller cover than the file's own leaves it alone.
            write_track(&path, &t, Some(&jpeg(32))).unwrap();
            let f = lofty::read_from_path(&path).unwrap();
            assert_eq!(f.primary_tag().unwrap().pictures()[0].data(), jpeg(64), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn embeds_cover() {
        let dir = std::env::temp_dir().join(format!("rpod-cover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 64, image::Rgb([0, 128, 255])))
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        for (name, codec) in [("a.mp3", "libmp3lame"), ("b.m4a", "aac")] {
            let path = dir.join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-c:a", codec])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return;
            }
            embed_cover(&path, &jpeg).unwrap();
            embed_cover(&path, &jpeg).unwrap(); // replacing, not stacking
            let f = lofty::read_from_path(&path).unwrap();
            let pics = f.primary_tag().unwrap().pictures();
            assert_eq!(pics.len(), 1, "{name}");
            assert_eq!(pics[0].data(), jpeg.as_slice(), "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
