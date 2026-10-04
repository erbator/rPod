//! Copying songs off the iPod into a music folder (`d` for a selection, `S`
//! for everything). Files get readable `Artist/Album/01 Title.ext` names and
//! the iPod's metadata written into their tags, since many files on an iPod
//! have none of their own. A record of what was copied, per iPod and folder,
//! lets later runs skip songs already there and only retag or rename the ones
//! edited on the iPod since.

use crate::artworkdb::{self, ArtworkDb};
use crate::itunesdb::Track;
use crate::{store, tags};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Not on the PC yet.
    Copy,
    /// On the PC, but edited on the iPod since: retag, and rename if needed.
    Update,
    /// On the PC and up to date.
    Have,
    /// Another file already sits where this song would go; left alone.
    Taken,
}

pub struct Planned {
    pub track: Track,
    /// Destination relative to the download folder.
    pub rel: PathBuf,
    pub step: Step,
    /// Where an `Update` currently lives, relative to the download folder.
    from: Option<PathBuf>,
}

pub enum Progress {
    Started(usize),
    /// Finished; a warning if the audio was copied but its tags couldn't be written.
    Done(usize, Option<String>),
    Failed(usize, String),
    Finished(Result<Summary, String>),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub copied: usize,
    pub updated: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// Videos stay behind: this is for music.
pub fn is_video(t: &Track) -> bool {
    let ext = Path::new(&t.location).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "m4v" | "mov") || (t.media_type & 0x02 != 0 && t.media_type & 0x01 == 0)
}

/// `Artist/Album/01 Title.ext`, with `1-01` style numbers on multi-disc albums.
pub fn target_path(t: &Track) -> PathBuf {
    let or = |s: &str, fallback: &str| if s.trim().is_empty() { fallback.to_string() } else { s.to_string() };
    let num = match (t.disc_no, t.track_no) {
        (_, 0) => String::new(),
        (d, n) if t.disc_total > 1 || d > 1 => format!("{d}-{n:02} "),
        (_, n) => format!("{n:02} "),
    };
    let ext = Path::new(&t.location).extension().and_then(|e| e.to_str()).unwrap_or("mp3").to_lowercase();
    PathBuf::from(clean_name(t.sort_artist()))
        .join(clean_name(&or(&t.album, "Unknown Album")))
        .join(format!("{}.{ext}", clean_name(&format!("{num}{}", or(&t.title, "Unknown")))))
}

/// A file or folder name that works on Linux, Windows and FAT/exFAT drives.
fn clean_name(s: &str) -> String {
    const BAD: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let mapped: String = s.chars().map(|c| if BAD.contains(&c) || c.is_control() { '_' } else { c }).collect();
    let mut name: String = mapped.trim().chars().take(100).collect();
    // Windows drops trailing dots and spaces, and a leading dot hides a file.
    name = name.trim_end_matches(['.', ' ']).to_string();
    if name.starts_with('.') {
        name.replace_range(..1, "_");
    }
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    if reserved.contains(&name.to_uppercase().as_str()) {
        name.push('_');
    }
    if name.is_empty() { "_".into() } else { name }
}

/// `name.ext` → `name (n).ext`, for two songs that would share a path.
fn numbered(rel: &Path, n: usize) -> PathBuf {
    let stem = rel.file_stem().unwrap_or_default().to_string_lossy();
    let ext = rel.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    rel.with_file_name(format!("{stem} ({n}){ext}"))
}

/// FNV-1a: stable across builds, unlike `DefaultHasher`, so records and
/// their file names survive a toolchain update.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Changes whenever anything written into the file's tags or name changes.
fn fingerprint(t: &Track) -> u64 {
    let text = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        t.title, t.artist, t.album, t.album_artist, t.genre, t.composer, t.comment,
        t.year, t.track_no, t.track_total, t.disc_no, t.disc_total, t.compilation
    );
    fnv(text.as_bytes())
}

#[derive(Serialize, Deserialize, Default)]
struct Record {
    dest: PathBuf,
    /// Track dbid → what was written for it.
    files: HashMap<u64, Entry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    path: PathBuf,
    meta: u64,
}

/// `~/.local/share/rpod/downloads/<FirewireGuid>/<folder hash>.json`
fn record_path(root: &Path, dest: &Path) -> PathBuf {
    let name = format!("{:016x}.json", fnv(dest.as_os_str().as_encoded_bytes()));
    store::data_dir().join("downloads").join(store::device_id(root)).join(name)
}

impl Record {
    fn load(root: &Path, dest: &Path) -> Self {
        std::fs::read(record_path(root, dest))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| Record { dest: dest.to_path_buf(), files: HashMap::new() })
    }

    fn save(&self, root: &Path) -> Result<()> {
        let path = record_path(root, &self.dest);
        std::fs::create_dir_all(path.parent().unwrap())?;
        store::atomic_write(&path, &serde_json::to_vec(self)?)
    }
}

/// Decide what to do with each track. Songs already downloaded keep their
/// paths, so a later run never shuffles `(2)` suffixes around.
pub fn plan(root: &Path, dest: &Path, tracks: &[Track]) -> Vec<Planned> {
    let record = Record::load(root, dest);
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<Option<Planned>> = tracks.iter().map(|_| None).collect();

    // Songs downloaded before claim their paths first.
    for (i, t) in tracks.iter().enumerate() {
        let Some(e) = record.files.get(&t.dbid).filter(|e| dest.join(&e.path).is_file()) else { continue };
        let rel = target_path(t);
        let same_place = e.path == rel || (1..100).any(|n| e.path == numbered(&rel, n));
        if same_place {
            taken.insert(e.path.clone());
            let step = if e.meta == fingerprint(t) { Step::Have } else { Step::Update };
            out[i] = Some(Planned { track: t.clone(), rel: e.path.clone(), step, from: Some(e.path.clone()) });
        }
    }
    // Every recorded file still on disk stays reserved, even if not in this selection.
    taken.extend(record.files.values().filter(|e| dest.join(&e.path).is_file()).map(|e| e.path.clone()));

    for (i, t) in tracks.iter().enumerate() {
        if out[i].is_some() {
            continue;
        }
        let mut from = record.files.get(&t.dbid).filter(|e| dest.join(&e.path).is_file()).map(|e| e.path.clone());
        let base = target_path(t);
        let mut step = if from.is_some() { Step::Update } else { Step::Copy };
        let mut rel = base.clone();
        let mut n = 1;
        loop {
            if !taken.contains(&rel) && !dest.join(&rel).exists() {
                break;
            }
            if !taken.contains(&rel) {
                if from.is_none() && same_song(&dest.join(&rel), t) {
                    // Downloaded before, but the record is gone (new PC,
                    // reinstall): adopt it and refresh its tags.
                    step = Step::Update;
                    from = Some(rel.clone());
                } else {
                    // Somebody else's file: don't overwrite it or pile a copy next to it.
                    step = Step::Taken;
                }
                break;
            }
            n += 1;
            rel = numbered(&base, n);
        }
        taken.insert(rel.clone());
        out[i] = Some(Planned { track: t.clone(), rel, step, from });
    }
    out.into_iter().flatten().collect()
}

/// Whether the file at `path` is tagged as `t`, as rPod's own downloads are.
fn same_song(path: &Path, t: &Track) -> bool {
    use lofty::prelude::*;
    let Ok(file) = lofty::read_from_path(path) else { return false };
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else { return false };
    let eq = |v: Option<std::borrow::Cow<str>>, want: &str| v.as_deref().unwrap_or("").trim() == want.trim();
    eq(tag.title(), &t.title) && eq(tag.artist(), &t.artist) && eq(tag.album(), &t.album)
}

/// Copy and retag everything planned, one file at a time (the iPod's disk is
/// slow at anything else). Stops between songs once `cancel` is set.
pub fn run(root: PathBuf, dest: PathBuf, plan: Arc<Vec<Planned>>, cancel: Arc<AtomicBool>, tx: Sender<Progress>) {
    let res = run_inner(&root, &dest, &plan, &cancel, &tx).map_err(|e| format!("{e:#}"));
    tx.send(Progress::Finished(res)).ok();
}

fn run_inner(root: &Path, dest: &Path, plan: &[Planned], cancel: &AtomicBool, tx: &Sender<Progress>) -> Result<Summary> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let art = artworkdb::read(&store::artwork_dir(root)).unwrap_or_default();
    let mut record = Record::load(root, dest);
    let mut sum = Summary::default();
    for (i, p) in plan.iter().enumerate() {
        if !matches!(p.step, Step::Copy | Step::Update) {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            sum.cancelled = true;
            break;
        }
        tx.send(Progress::Started(i)).ok();
        let cover = cover_jpeg(&art, p.track.dbid);
        let res = match p.step {
            Step::Copy => copy_one(root, dest, p, cover.as_deref()),
            _ => update_one(dest, p, cover.as_deref()),
        };
        match res {
            Ok(warning) => {
                if p.step == Step::Copy { sum.copied += 1 } else { sum.updated += 1 }
                record.files.insert(p.track.dbid, Entry { path: p.rel.clone(), meta: fingerprint(&p.track) });
                // Saved after every song, so an unplugged iPod loses nothing.
                record.save(root)?;
                tx.send(Progress::Done(i, warning)).ok();
            }
            Err(e) => {
                sum.failed += 1;
                tx.send(Progress::Failed(i, format!("{e:#}"))).ok();
            }
        }
    }
    Ok(sum)
}

/// The iPod's own thumbnail as a JPEG, for files with no cover of their own.
fn cover_jpeg(art: &ArtworkDb, dbid: u64) -> Option<Vec<u8>> {
    let img = art.load(art.best_thumb(dbid)?).ok()?;
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img.to_rgb8()).ok()?;
    Some(out)
}

/// Copy through a hidden temp file in the destination folder, so a cancelled
/// or interrupted copy never leaves a half-written song behind.
fn copy_one(root: &Path, dest: &Path, p: &Planned, cover: Option<&[u8]>) -> Result<Option<String>> {
    let src = root.join(&p.track.location);
    let to = dest.join(&p.rel);
    let dir = to.parent().unwrap();
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ext = p.rel.extension().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".rpod-{:016x}.{ext}", p.track.dbid));
    if let Err(e) = std::fs::copy(&src, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("copying {}", src.display()));
    }
    // A song is worth keeping even if its tags can't be written.
    let warning = tags::write_track(&tmp, &p.track, cover).err().map(|e| format!("copied, but tags failed: {e:#}"));
    std::fs::rename(&tmp, &to).with_context(|| format!("saving {}", to.display()))?;
    Ok(warning)
}

fn update_one(dest: &Path, p: &Planned, cover: Option<&[u8]>) -> Result<Option<String>> {
    let from = dest.join(p.from.as_ref().unwrap_or(&p.rel));
    let to = dest.join(&p.rel);
    if from != to {
        if to.exists() {
            bail!("{} already exists", to.display());
        }
        std::fs::create_dir_all(to.parent().unwrap())?;
        std::fs::rename(&from, &to).with_context(|| format!("renaming to {}", to.display()))?;
        // Tidy the old album/artist folders if that emptied them.
        for dir in from.ancestors().skip(1).take(2) {
            if dir == dest || std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }
    tags::write_track(&to, &p.track, cover)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, album: &str, title: &str, disc: (u32, u32), no: u32) -> Track {
        Track {
            dbid: fnv(title.as_bytes()),
            artist: artist.into(),
            album: album.into(),
            title: title.into(),
            disc_no: disc.0,
            disc_total: disc.1,
            track_no: no,
            location: "iPod_Control/Music/F00/ABCD.MP3".into(),
            ..Default::default()
        }
    }

    #[test]
    fn names_are_readable_and_safe() {
        let t = track("AC/DC", "Back in Black", "Hells Bells", (1, 1), 1);
        assert_eq!(target_path(&t), PathBuf::from("AC_DC/Back in Black/01 Hells Bells.mp3"));
        let t = track("Panchiko", "D>E>A>T>H>M>E>T>A>L", "Stabilisers", (2, 2), 7);
        assert_eq!(target_path(&t), PathBuf::from("Panchiko/D_E_A_T_H_M_E_T_A_L/2-07 Stabilisers.mp3"));
        let t = track("", "", "", (0, 0), 0);
        assert_eq!(target_path(&t), PathBuf::from("Unknown Artist/Unknown Album/Unknown.mp3"));
        assert_eq!(clean_name(".hidden..."), "_hidden");
        assert_eq!(clean_name("con"), "con_");
    }

    #[test]
    fn plans_copies_updates_and_clashes() {
        let _env = store::TEST_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("rpod-export-{}", std::process::id()));
        let (root, dest) = (tmp.join("ipod"), tmp.join("music"));
        unsafe { std::env::set_var("XDG_DATA_HOME", tmp.join("data")) };
        let a = track("A", "X", "One", (1, 1), 1);
        let b = track("A", "X", "One", (1, 1), 1); // same name, different song
        let mut b = b;
        b.dbid += 1;
        let c = track("A", "X", "Two", (1, 1), 2);

        let p = plan(&root, &dest, &[a.clone(), b.clone(), c.clone()]);
        assert_eq!(p.iter().map(|p| p.step).collect::<Vec<_>>(), [Step::Copy, Step::Copy, Step::Copy]);
        assert_eq!(p[1].rel, PathBuf::from("A/X/01 One (2).mp3"));

        // Pretend a and b were downloaded, and a stranger's file sits where c goes.
        let mut rec = Record { dest: dest.clone(), files: HashMap::new() };
        for p in &p[..2] {
            std::fs::create_dir_all(dest.join(&p.rel).parent().unwrap()).unwrap();
            std::fs::write(dest.join(&p.rel), b"x").unwrap();
            rec.files.insert(p.track.dbid, Entry { path: p.rel.clone(), meta: fingerprint(&p.track) });
        }
        rec.save(&root).unwrap();
        std::fs::write(dest.join(&p[2].rel), b"someone else's").unwrap();

        let mut a2 = a.clone();
        a2.genre = "Rock".into();
        let p = plan(&root, &dest, &[a2, b.clone(), c]);
        assert_eq!(p.iter().map(|p| p.step).collect::<Vec<_>>(), [Step::Update, Step::Have, Step::Taken]);
        assert_eq!(p[1].rel, PathBuf::from("A/X/01 One (2).mp3"));

        // Retitling moves the file.
        let mut b2 = b;
        b2.title = "Three".into();
        let p = plan(&root, &dest, &[b2]);
        assert_eq!((p[0].step, p[0].rel.clone()), (Step::Update, PathBuf::from("A/X/01 Three.mp3")));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// Downloads the first N songs of a real iPod (read-only on the iPod),
    /// optionally only from albums containing RPOD_TEST_ALBUM:
    /// `RPOD_TEST_IPOD=/run/media/… RPOD_TEST_DEST=/tmp/out RPOD_TEST_N=20 cargo test --release download_real -- --nocapture`
    #[test]
    fn download_real() {
        let (Ok(root), Ok(dest)) = (std::env::var("RPOD_TEST_IPOD"), std::env::var("RPOD_TEST_DEST")) else { return };
        let n: usize = std::env::var("RPOD_TEST_N").ok().and_then(|n| n.parse().ok()).unwrap_or(20);
        let (root, dest) = (PathBuf::from(root), PathBuf::from(dest));
        let ipod = crate::device::Ipod::open(&root).unwrap();
        let album = std::env::var("RPOD_TEST_ALBUM").unwrap_or_default();
        let tracks: Vec<Track> =
            ipod.db.tracks.iter().filter(|t| !is_video(t) && t.album.contains(&album)).take(n).cloned().collect();
        let p = Arc::new(plan(&root, &dest, &tracks));
        let bytes: u64 = p.iter().filter(|p| p.step == Step::Copy).map(|p| p.track.size as u64).sum();
        let (tx, rx) = std::sync::mpsc::channel();
        let t0 = std::time::Instant::now();
        run(root, dest, p.clone(), Arc::new(AtomicBool::new(false)), tx);
        for msg in rx {
            match msg {
                Progress::Done(i, w) => println!("ok   {}{}", p[i].rel.display(), w.map(|w| format!("  ({w})")).unwrap_or_default()),
                Progress::Failed(i, e) => println!("FAIL {}: {e}", p[i].rel.display()),
                Progress::Finished(r) => println!("{r:?}"),
                Progress::Started(_) => {}
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        println!("{:.1} MB in {secs:.1} s = {:.1} MB/s", bytes as f64 / 1e6, bytes as f64 / 1e6 / secs);
    }
}
