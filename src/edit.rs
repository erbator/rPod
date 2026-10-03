//! Metadata editing: the editable fields, turning form input into per-track
//! edits, and saving them to the iPod.

use crate::dbwrite::{self, TrackEdit};
use crate::itunesdb::{self, Track};
use crate::{store, tags};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Year,
    TrackNo,
    TrackTotal,
    DiscNo,
    DiscTotal,
    Composer,
    Comment,
    Compilation,
    Rating,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Toggle,
    Stars,
}

pub const FIELDS: [Field; 14] = [
    Field::Title,
    Field::Artist,
    Field::Album,
    Field::AlbumArtist,
    Field::Genre,
    Field::Year,
    Field::TrackNo,
    Field::TrackTotal,
    Field::DiscNo,
    Field::DiscTotal,
    Field::Composer,
    Field::Comment,
    Field::Compilation,
    Field::Rating,
];

const TEXT_FIELDS: [Field; 7] =
    [Field::Title, Field::Artist, Field::Album, Field::AlbumArtist, Field::Genre, Field::Composer, Field::Comment];

impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Title => "Title",
            Field::Artist => "Artist",
            Field::Album => "Album",
            Field::AlbumArtist => "Album artist",
            Field::Genre => "Genre",
            Field::Year => "Year",
            Field::TrackNo => "Track #",
            Field::TrackTotal => "Tracks total",
            Field::DiscNo => "Disc #",
            Field::DiscTotal => "Discs total",
            Field::Composer => "Composer",
            Field::Comment => "Comment",
            Field::Compilation => "Compilation",
            Field::Rating => "Rating",
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Field::Year | Field::TrackNo | Field::TrackTotal | Field::DiscNo | Field::DiscTotal => Kind::Number,
            Field::Compilation => Kind::Toggle,
            Field::Rating => Kind::Stars,
            _ => Kind::Text,
        }
    }

    /// The field's value as the form shows it. Numbers of 0 are blank,
    /// toggles are "yes"/"no", ratings are a star count.
    pub fn get(self, t: &Track) -> String {
        let num = |n: u32| if n == 0 { String::new() } else { n.to_string() };
        match self {
            Field::Title => t.title.clone(),
            Field::Artist => t.artist.clone(),
            Field::Album => t.album.clone(),
            Field::AlbumArtist => t.album_artist.clone(),
            Field::Genre => t.genre.clone(),
            Field::Composer => t.composer.clone(),
            Field::Comment => t.comment.clone(),
            Field::Year => num(t.year),
            Field::TrackNo => num(t.track_no),
            Field::TrackTotal => num(t.track_total),
            Field::DiscNo => num(t.disc_no),
            Field::DiscTotal => num(t.disc_total),
            Field::Compilation => if t.compilation { "yes" } else { "no" }.into(),
            Field::Rating => (t.rating / 20).min(5).to_string(),
        }
    }

    /// Store a form value into an edit.
    fn set(self, e: &mut TrackEdit, v: &str) -> Result<(), String> {
        let number = || -> Result<u32, String> {
            if v.trim().is_empty() {
                return Ok(0);
            }
            v.trim().parse::<u32>().map_err(|_| format!("{} must be a number", self.label()))
        };
        match self {
            Field::Title => e.title = Some(v.to_string()),
            Field::Artist => e.artist = Some(v.to_string()),
            Field::Album => e.album = Some(v.to_string()),
            Field::AlbumArtist => e.album_artist = Some(v.to_string()),
            Field::Genre => e.genre = Some(v.to_string()),
            Field::Composer => e.composer = Some(v.to_string()),
            Field::Comment => e.comment = Some(v.to_string()),
            Field::Year => e.year = Some(number()?),
            Field::TrackNo => e.track_no = Some(number()?),
            Field::TrackTotal => e.track_total = Some(number()?),
            Field::DiscNo => e.disc_no = Some(number()?),
            Field::DiscTotal => e.disc_total = Some(number()?),
            Field::Compilation => e.compilation = Some(v == "yes"),
            Field::Rating => e.rating = Some(v.parse::<u8>().unwrap_or(0).min(5) * 20),
        }
        Ok(())
    }
}

/// Trim and collapse runs of whitespace.
pub fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Form state shared by every track in the editor.
#[derive(Default)]
pub struct Form {
    /// Field → typed value.
    pub values: HashMap<Field, String>,
    /// Number the tracks 1…n in order (and set the total).
    pub auto_number: bool,
    /// Tidy whitespace in every text field.
    pub clean: bool,
}

impl Form {
    /// Per-track edits (by track id), skipping tracks with nothing to change.
    pub fn build(&self, tracks: &[Track]) -> Result<HashMap<u32, TrackEdit>, String> {
        let n = tracks.len() as u32;
        let mut out = HashMap::new();
        for (i, t) in tracks.iter().enumerate() {
            let mut e = TrackEdit::default();
            for (&field, v) in &self.values {
                if field.get(t) != *v {
                    field.set(&mut e, v)?;
                }
            }
            if self.auto_number {
                if t.track_no != i as u32 + 1 {
                    e.track_no = Some(i as u32 + 1);
                }
                if !self.values.contains_key(&Field::TrackTotal) && t.track_total != n {
                    e.track_total = Some(n);
                }
            }
            if self.clean {
                for f in TEXT_FIELDS {
                    let current = self.values.get(&f).cloned().unwrap_or_else(|| f.get(t));
                    let tidy = clean(&current);
                    if tidy != f.get(t) {
                        f.set(&mut e, &tidy)?;
                    }
                }
            }
            if !e.is_empty() {
                out.insert(t.id, e);
            }
        }
        Ok(out)
    }
}

pub struct Report {
    pub tracks: usize,
    pub file_errors: Vec<String>,
}

/// Back up, write and verify the iPod database, then (optionally) the files' tags.
pub fn save(root: &Path, edits: &HashMap<u32, TrackEdit>, write_files: bool) -> Result<Report> {
    if edits.is_empty() {
        return Ok(Report { tracks: 0, file_errors: Vec::new() });
    }
    store::backup(root)?;
    let db_path = store::itunesdb_path(root);
    let orig = std::fs::read(&db_path)?;
    let parsed = itunesdb::parse(&orig)?;
    let out = dbwrite::edit_tracks(&orig, &parsed, edits)?;

    // Verify: re-read the new database and check every edit landed.
    let check = itunesdb::parse(&out)?;
    if check.tracks.len() != parsed.tracks.len() {
        bail!("verification failed: track count changed");
    }
    for (before, after) in parsed.tracks.iter().zip(&check.tracks) {
        let mut expected = before.clone();
        if let Some(e) = edits.get(&before.id) {
            e.apply(&mut expected);
        }
        if FIELDS.iter().any(|f| f.get(&expected) != f.get(after)) {
            bail!("verification failed for \"{}\"", before.title);
        }
    }
    store::atomic_write(&db_path, &out)?;

    let mut file_errors = Vec::new();
    if write_files {
        for t in parsed.tracks.iter().filter(|t| edits.contains_key(&t.id)) {
            let path: PathBuf = root.join(&t.location);
            if let Err(e) = tags::write(&path, &edits[&t.id]) {
                file_errors.push(format!("{}: {e:#}", t.title));
            }
        }
    }
    Ok(Report { tracks: edits.len(), file_errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copy of the test iPod's database in a temp dir, for writing.
    fn scratch_root() -> Option<PathBuf> {
        let src = std::env::var("RPOD_TEST_IPOD").ok()?;
        let root = std::env::temp_dir().join(format!("rpod-edit-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(root.join("iPod_Control/iTunes")).unwrap();
        std::fs::copy(format!("{src}/iPod_Control/iTunes/iTunesDB"), root.join("iPod_Control/iTunes/iTunesDB")).unwrap();
        Some(root)
    }

    #[test]
    fn batch_edit_album_saves_and_verifies() {
        let Some(root) = scratch_root() else { return };
        // SAFETY: tests in this module don't read XDG_DATA_HOME concurrently.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
        let db = itunesdb::read(&store::itunesdb_path(&root)).unwrap();
        let album = db.tracks[0].album.clone();
        let mut tracks: Vec<Track> = db.tracks.iter().filter(|t| t.album == album).cloned().collect();
        tracks.sort_by_key(|t| (t.disc_no, t.track_no));

        let mut form = Form::default();
        form.values.insert(Field::Genre, "Edited Genre".into());
        form.values.insert(Field::Year, "1987".into());
        form.values.insert(Field::Rating, "4".into());
        form.auto_number = true;
        let edits = form.build(&tracks).unwrap();
        let report = save(&root, &edits, false).unwrap();
        assert_eq!(report.tracks, edits.len());

        let after = itunesdb::read(&store::itunesdb_path(&root)).unwrap();
        for (i, t) in tracks.iter().enumerate() {
            let a = after.tracks.iter().find(|x| x.id == t.id).unwrap();
            assert_eq!(a.genre, "Edited Genre");
            assert_eq!((a.year, a.rating), (1987, 80));
            assert_eq!((a.track_no, a.track_total), (i as u32 + 1, tracks.len() as u32));
            assert_eq!(a.title, t.title);
        }
        assert!(root.join("data/rpod/backups").exists(), "backup made");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bad_numbers_are_rejected() {
        let t = Track { id: 1, ..Default::default() };
        let mut form = Form::default();
        form.values.insert(Field::Year, "nineteen".into());
        assert!(form.build(&[t]).unwrap_err().contains("Year"));
    }

    #[test]
    fn clean_collapses_whitespace() {
        assert_eq!(clean("  Daft   Punk "), "Daft Punk");
    }
}
