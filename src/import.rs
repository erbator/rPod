//! Adding music: scanning sources, choosing conversions, transcoding with
//! ffmpeg, copying onto the iPod, and committing the databases.

use crate::artwrite::{self, NewImage};
use crate::dbwrite::{self, NewTrack, rand_u64};
use crate::itunesdb::{self, Track};
use crate::store;
use anyhow::{Context, Result, bail};
use image::DynamicImage;
use lofty::config::ParseOptions;
use lofty::picture::PictureType;
use lofty::prelude::*;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- settings

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Alac,
    Aac(u32),
    Mp3(u32),
    Mp3V0,
}

impl Target {
    pub fn label(self) -> String {
        match self {
            Target::Alac => "ALAC (lossless)".into(),
            Target::Aac(k) => format!("AAC {k} kbps"),
            Target::Mp3(k) => format!("MP3 {k} kbps"),
            Target::Mp3V0 => "MP3 V0 (~245 kbps VBR)".into(),
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Target::Alac | Target::Aac(_) => "m4a",
            Target::Mp3(_) | Target::Mp3V0 => "mp3",
        }
    }

    /// Rough output bytes per second, for size estimates.
    fn bytes_per_sec(self) -> Option<u64> {
        match self {
            Target::Alac => None,
            Target::Aac(k) | Target::Mp3(k) => Some(k as u64 * 125),
            Target::Mp3V0 => Some(245 * 125),
        }
    }
}

pub const LOSSLESS_TARGETS: &[Target] =
    &[Target::Alac, Target::Aac(320), Target::Aac(256), Target::Aac(192), Target::Mp3(320), Target::Mp3V0];
pub const LOSSY_TARGETS: &[Target] =
    &[Target::Aac(256), Target::Aac(192), Target::Aac(128), Target::Mp3(320), Target::Mp3V0];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// What FLAC / WAV / AIFF / APE / WavPack become.
    pub lossless: Target,
    /// What Ogg Vorbis / Opus / Musepack become (the iPod can't play them).
    pub lossy: Target,
    /// Re-encode MP3/AAC above this bitrate to the lossy target (0 = never).
    pub shrink_above: u32,
    pub jobs: usize,
    pub skip_duplicates: bool,
    /// Use cover.jpg / folder.jpg when a file has no embedded art.
    pub folder_art: bool,
    /// Also write metadata edits into the audio files' tags.
    pub write_tags: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lossless: Target::Alac,
            lossy: Target::Aac(256),
            shrink_above: 0,
            jobs: std::thread::available_parallelism().map_or(4, |n| n.get()),
            skip_duplicates: true,
            folder_art: true,
            write_tags: true,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rpod/settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = config_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap_or_default());
        }
    }
}

// ---------------------------------------------------------------- sources

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec {
    Mp3,
    Aac,
    Alac,
    Lossless,
    Lossy,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Copy,
    Convert(Target),
    Skip(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Pending,
    Working(&'static str),
    Done,
    Failed(String),
}

#[derive(Clone)]
pub struct Item {
    pub src: PathBuf,
    pub meta: Track,
    pub codec: Codec,
    /// e.g. "FLAC 24/96", "MP3 320"
    pub format: String,
    pub bit_depth: u8,
    pub channels: u8,
    pub size: u64,
    /// Embedded cover art.
    pub has_art: bool,
    /// A cover.jpg-style image next to the file.
    pub folder_art: bool,
    /// A cover picked or found online; wins over embedded and folder art.
    pub cover: Option<Arc<Vec<u8>>>,
    pub duplicate: bool,
    pub enabled: bool,
    pub status: Status,
}

impl Item {
    pub fn will_have_art(&self, s: &Settings) -> bool {
        self.cover.is_some() || self.has_art || (s.folder_art && self.folder_art)
    }

    /// Songs from one album share this, so a cover applies to all of them.
    pub fn album_key(&self) -> (String, String) {
        (self.meta.sort_artist().to_lowercase(), self.meta.album.to_lowercase())
    }

    /// The iPod Video plays MP3, AAC and ALAC up to 48 kHz / 16-bit.
    fn ipod_safe(&self) -> bool {
        self.meta.sample_rate <= 48_000 && self.bit_depth <= 16 && self.channels <= 2
    }

    pub fn action(&self, s: &Settings) -> Action {
        if !self.enabled {
            return Action::Skip("unchecked");
        }
        if self.duplicate && s.skip_duplicates {
            return Action::Skip("already on iPod");
        }
        match self.codec {
            Codec::Mp3 | Codec::Aac => {
                if s.shrink_above > 0 && self.meta.bitrate > s.shrink_above {
                    Action::Convert(s.lossy)
                } else if self.meta.sample_rate > 48_000 {
                    Action::Convert(s.lossy)
                } else {
                    Action::Copy
                }
            }
            Codec::Alac if self.ipod_safe() && s.lossless == Target::Alac => Action::Copy,
            Codec::Alac | Codec::Lossless => Action::Convert(s.lossless),
            Codec::Lossy => Action::Convert(s.lossy),
        }
    }

    pub fn estimated_size(&self, s: &Settings) -> u64 {
        let secs = self.meta.length_ms as u64 / 1000;
        match self.action(s) {
            Action::Skip(_) => 0,
            Action::Copy => self.size,
            Action::Convert(Target::Alac) => {
                // ALAC ≈ FLAC size, scaled down when hi-res is reduced to 16/44.1.
                let rate = (self.meta.sample_rate.max(1) as f64).min(48_000.0) / self.meta.sample_rate.max(1) as f64;
                let depth = 16.0 / (self.bit_depth.max(16) as f64);
                let base = if self.format.starts_with("WAV") || self.format.starts_with("AIFF") {
                    self.size as f64 * 0.6
                } else {
                    self.size as f64 * 1.05
                };
                (base * rate * depth) as u64
            }
            Action::Convert(t) => t.bytes_per_sec().unwrap_or(0) * secs,
        }
    }
}

const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "m4b", "mp4", "aac", "flac", "wav", "aif", "aiff", "ogg", "oga", "opus", "ape", "wv", "mpc",
];

pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.contains(&e.to_lowercase().as_str()))
}

/// Expand files and folders into audio files.
pub fn expand(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(p)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && is_audio(e.path()))
                .map(|e| e.into_path())
                .collect();
            files.sort();
            out.extend(files);
        } else if is_audio(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Read tags and properties of each file in parallel.
pub fn scan(files: &[PathBuf], on_ipod: &HashSet<(String, String, String)>) -> Vec<Item> {
    // Look for cover.jpg once per folder, not once per file.
    let dirs: HashSet<&Path> = files.iter().filter_map(|f| f.parent()).collect();
    let with_cover: HashSet<&Path> = dirs.into_par_iter().filter(|d| find_folder_art(d).is_some()).collect();
    let mut items: Vec<Item> = files
        .par_iter()
        .filter_map(|p| probe(p, on_ipod, p.parent().is_some_and(|d| with_cover.contains(d))).ok())
        .collect();
    items.sort_by_cached_key(|i| {
        let m = &i.meta;
        (m.sort_artist().to_lowercase(), m.album.to_lowercase(), m.disc_no, m.track_no, i.src.clone())
    });
    items
}

pub fn dup_key(t: &Track) -> (String, String, String) {
    (t.artist.trim().to_lowercase(), t.album.trim().to_lowercase(), t.title.trim().to_lowercase())
}

fn probe(path: &Path, on_ipod: &HashSet<(String, String, String)>, dir_has_cover: bool) -> Result<Item> {
    let probe = lofty::probe::Probe::open(path)?.guess_file_type()?;
    // MP4 needs its concrete type to tell AAC from ALAC; parse it only once.
    let (tagged, mp4_codec) = if probe.file_type() == Some(lofty::file::FileType::Mp4) {
        let mut f = std::fs::File::open(path)?;
        let mp4 = lofty::mp4::Mp4File::read_from(&mut f, ParseOptions::new())?;
        let codec = mp4.properties().codec();
        (lofty::file::TaggedFile::from(mp4), codec)
    } else {
        (probe.read()?, None)
    };
    let props = tagged.properties();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

    let codec = match tagged.file_type() {
        lofty::file::FileType::Mpeg => Codec::Mp3,
        lofty::file::FileType::Aac => Codec::Aac,
        lofty::file::FileType::Mp4 => match mp4_codec {
            Some(lofty::mp4::Mp4Codec::ALAC) => Codec::Alac,
            Some(lofty::mp4::Mp4Codec::MP3) => Codec::Mp3,
            Some(lofty::mp4::Mp4Codec::FLAC) => Codec::Lossless,
            _ => Codec::Aac,
        },
        lofty::file::FileType::Flac
        | lofty::file::FileType::Wav
        | lofty::file::FileType::Aiff
        | lofty::file::FileType::Ape
        | lofty::file::FileType::WavPack => Codec::Lossless,
        _ => Codec::Lossy,
    };

    let sample_rate = props.sample_rate().unwrap_or(44_100);
    let bit_depth = props.bit_depth().unwrap_or(16);
    let bitrate = props.audio_bitrate().or(props.overall_bitrate()).unwrap_or(0);
    let format = match codec {
        Codec::Mp3 | Codec::Aac | Codec::Lossy => {
            format!("{} {bitrate}", if codec == Codec::Mp3 { "MP3".to_string() } else { ext.to_uppercase() })
        }
        _ => format!(
            "{} {}/{}",
            if codec == Codec::Alac { "ALAC".to_string() } else { ext.to_uppercase() },
            bit_depth,
            (sample_rate as f32 / 1000.0).to_string().trim_end_matches(".0")
        ),
    };

    let mut meta = Track {
        length_ms: props.duration().as_millis() as u32,
        sample_rate,
        bitrate,
        ..Default::default()
    };
    let mut has_art = false;
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let s = |v: Option<std::borrow::Cow<str>>| v.map(|c| c.trim().to_string()).unwrap_or_default();
        meta.title = s(tag.title());
        meta.artist = s(tag.artist());
        meta.album = s(tag.album());
        meta.genre = s(tag.genre());
        meta.comment = s(tag.comment());
        meta.album_artist = tag.get_string(ItemKey::AlbumArtist).unwrap_or("").trim().to_string();
        meta.composer = tag.get_string(ItemKey::Composer).unwrap_or("").trim().to_string();
        meta.compilation = matches!(tag.get_string(ItemKey::FlagCompilation), Some("1" | "true"));
        meta.track_no = tag.track().unwrap_or(0);
        meta.track_total = tag.track_total().unwrap_or(0);
        meta.disc_no = tag.disk().unwrap_or(0);
        meta.disc_total = tag.disk_total().unwrap_or(0);
        meta.year = tag.date().map_or(0, |d| d.year as u32);
        has_art = !tag.pictures().is_empty();
    }
    if meta.title.is_empty() {
        meta.title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }

    let duplicate = on_ipod.contains(&dup_key(&meta));
    Ok(Item {
        src: path.to_path_buf(),
        meta,
        codec,
        format,
        bit_depth,
        channels: props.channels().unwrap_or(2),
        size: std::fs::metadata(path)?.len(),
        has_art,
        folder_art: !has_art && dir_has_cover,
        cover: None,
        duplicate,
        enabled: true,
        status: Status::Pending,
    })
}

// ---------------------------------------------------------------- dropped paths

/// Turn pasted / drag-and-dropped text into existing paths. Handles
/// newline-separated lists, shell quoting and escaping, and file:// URIs.
pub fn parse_dropped(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let whole = to_path(line);
        if whole.exists() {
            out.push(whole);
            continue;
        }
        out.extend(shell_split(line).iter().map(|t| to_path(t)).filter(|p| p.exists()));
    }
    out
}

fn to_path(s: &str) -> PathBuf {
    match s.strip_prefix("file://") {
        Some(rest) => PathBuf::from(percent_decode(rest.strip_prefix("localhost").unwrap_or(rest))),
        None => PathBuf::from(s),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    let mut in_token = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_token = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_token = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- running

pub enum Progress {
    Item(usize, Status),
    Phase(String),
    Finished(Result<usize, String>),
}

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn ffmpeg_args(item: &Item, t: Target, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.push(item.src.to_string_lossy().into_owned());
    a.extend(["-map", "0:a:0", "-vn", "-map_metadata", "0"].map(String::from));
    if item.meta.sample_rate > 48_000 {
        a.extend(["-ar", "44100"].map(String::from));
    }
    if item.channels > 2 {
        a.extend(["-ac", "2"].map(String::from));
    }
    match t {
        Target::Alac => a.extend(["-c:a", "alac", "-sample_fmt", "s16p"].map(String::from)),
        Target::Aac(k) => a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), format!("{k}k")]),
        Target::Mp3(k) => a.extend(["-c:a".into(), "libmp3lame".into(), "-b:a".into(), format!("{k}k"), "-id3v2_version".into(), "3".into()]),
        Target::Mp3V0 => a.extend(["-c:a", "libmp3lame", "-q:a", "0", "-id3v2_version", "3"].map(String::from)),
    }
    a.push(out.to_string_lossy().into_owned());
    a
}

/// Pick a free `iPod_Control/Music/Fxx/ABCD.ext` path.
fn ipod_dest(root: &Path, ext: &str) -> Result<PathBuf> {
    let music = root.join("iPod_Control/Music");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&music)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('F')))
        .collect();
    if dirs.is_empty() {
        let d = music.join("F00");
        std::fs::create_dir_all(&d)?;
        dirs.push(d);
    }
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    loop {
        let r = rand_u64();
        let dir = &dirs[(r % dirs.len() as u64) as usize];
        let name: String = (0..4).map(|i| ALPHA[((r >> (8 + i * 6)) % 36) as usize] as char).collect();
        let p = dir.join(format!("{name}.{ext}"));
        if !p.exists() {
            return Ok(p);
        }
    }
}

const COVER_NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart", "albumartsmall"];

/// Raw bytes of the embedded front cover, else of a cover image next to the file.
fn find_art(src: &Path, folder_art: bool) -> Option<Vec<u8>> {
    if let Ok(tagged) = lofty::read_from_path(src) {
        for tag in tagged.tags() {
            let pics = tag.pictures();
            if let Some(p) = pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or(pics.first()) {
                return Some(p.data().to_vec());
            }
        }
    }
    if !folder_art {
        return None;
    }
    std::fs::read(find_folder_art(src.parent()?)?).ok()
}

/// A decoded cover shared by every track that uses the same image bytes.
pub type Cover = Arc<(u32, Arc<DynamicImage>)>;

/// Decoded covers by content hash. Each entry is decoded at most once; other
/// threads wanting the same cover wait on its OnceLock instead of decoding.
type ArtCache = Mutex<HashMap<u64, Arc<std::sync::OnceLock<Option<Cover>>>>>;

fn decode_cover(bytes: Vec<u8>, cache: &ArtCache) -> Option<Cover> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let slot = cache.lock().unwrap().entry(h.finish()).or_default().clone();
    slot.get_or_init(|| {
        let img = image::load_from_memory(&bytes).ok()?;
        Some(Arc::new((bytes.len() as u32, Arc::new(img.thumbnail(480, 480)))))
    })
    .clone()
}

fn find_folder_art(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            matches!(ext.as_str(), "jpg" | "jpeg" | "png") && COVER_NAMES.iter().any(|n| stem.starts_with(n))
        })
        .collect();
    candidates.sort_by_key(|p| {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        COVER_NAMES.iter().position(|n| stem.starts_with(n)).unwrap_or(99)
    });
    candidates.into_iter().next()
}

fn kind_of(path: &Path, codec_alac: bool) -> (String, [u8; 4]) {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")) {
        ("MPEG audio file".into(), *b"MP3 ")
    } else if codec_alac {
        ("Apple Lossless audio file".into(), *b"M4A ")
    } else {
        ("AAC audio file".into(), *b"M4A ")
    }
}

struct Prepared {
    track: NewTrack,
    file: PathBuf,
    art: Option<Cover>,
}

/// Transcode/copy one item onto the iPod.
fn prepare(
    root: &Path,
    item: &Item,
    action: &Action,
    settings: &Settings,
    tmp: &Path,
    idx: usize,
    usb: &Mutex<()>,
    art_cache: &ArtCache,
    tx: &Sender<Progress>,
) -> Result<Prepared> {
    let (src, ext) = match action {
        Action::Convert(t) => {
            tx.send(Progress::Item(idx, Status::Working("converting"))).ok();
            let out = tmp.join(format!("{idx}.{}", t.ext()));
            let res = Command::new("ffmpeg").args(ffmpeg_args(item, *t, &out)).output().context("running ffmpeg")?;
            if !res.status.success() {
                let err = String::from_utf8_lossy(&res.stderr);
                bail!("ffmpeg: {}", err.lines().last().unwrap_or("failed"));
            }
            (out, t.ext().to_string())
        }
        _ => {
            let ext = item.src.extension().and_then(|e| e.to_str()).unwrap_or("mp3").to_lowercase();
            let ext = if ext == "mp4" || ext == "m4b" { "m4a".into() } else { ext };
            (item.src.clone(), ext)
        }
    };

    // Final properties come from the file that lands on the iPod.
    let tagged = lofty::read_from_path(&src)?;
    let props = tagged.properties();
    let is_alac = matches!(action, Action::Convert(Target::Alac)) || (item.codec == Codec::Alac && *action == Action::Copy);

    let dest = ipod_dest(root, &ext)?;
    {
        let _lock = usb.lock().unwrap(); // the iPod's hard disk hates parallel writes
        tx.send(Progress::Item(idx, Status::Working("copying"))).ok();
        std::fs::copy(&src, &dest).with_context(|| format!("copying to {}", dest.display()))?;
    }
    if src != item.src {
        let _ = std::fs::remove_file(&src);
    }

    let art = item
        .cover
        .as_ref()
        .map(|b| b.to_vec())
        .or_else(|| find_art(&item.src, settings.folder_art))
        .and_then(|bytes| decode_cover(bytes, art_cache));

    let (kind, filetype) = kind_of(&dest, is_alac);
    let rel = dest.strip_prefix(root)?.to_string_lossy().into_owned();
    let mut meta = item.meta.clone();
    meta.location = rel;
    meta.kind = kind;
    meta.size = std::fs::metadata(&dest)?.len() as u32;
    meta.length_ms = props.duration().as_millis() as u32;
    meta.bitrate = props.audio_bitrate().or(props.overall_bitrate()).unwrap_or(meta.bitrate);
    meta.sample_rate = props.sample_rate().unwrap_or(meta.sample_rate);
    meta.dbid = rand_u64();
    let vbr = matches!(action, Action::Convert(Target::Mp3V0));
    Ok(Prepared { track: NewTrack { meta, filetype, vbr, artwork: None }, file: dest, art })
}

/// Run the whole import on the calling thread, reporting through `tx`.
pub fn run(root: PathBuf, items: Vec<Item>, settings: Settings, tx: Sender<Progress>) {
    let res = run_inner(&root, &items, &settings, &tx).map_err(|e| format!("{e:#}"));
    tx.send(Progress::Finished(res)).ok();
}

fn run_inner(root: &Path, items: &[Item], settings: &Settings, tx: &Sender<Progress>) -> Result<usize> {
    let work: Vec<(usize, Action)> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (i, it.action(settings)))
        .filter(|(_, a)| !matches!(a, Action::Skip(_)))
        .collect();
    if work.is_empty() {
        return Ok(0);
    }
    if work.iter().any(|(_, a)| matches!(a, Action::Convert(_))) && !ffmpeg_available() {
        bail!("ffmpeg is needed to convert some of these files but wasn't found");
    }

    let tmp = std::env::temp_dir().join(format!("rpod-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let usb = Mutex::new(());
    let art_cache: ArtCache = Mutex::new(HashMap::new());
    let pool = rayon::ThreadPoolBuilder::new().num_threads(settings.jobs.max(1)).build()?;
    let results: Vec<(usize, Result<Prepared>)> = pool.install(|| {
        work.par_iter()
            .map(|(i, action)| {
                let r = prepare(root, &items[*i], action, settings, &tmp, *i, &usb, &art_cache, tx);
                let status = match &r {
                    Ok(_) => Status::Working("waiting for database"),
                    Err(e) => Status::Failed(format!("{e:#}")),
                };
                tx.send(Progress::Item(*i, status)).ok();
                (*i, r)
            })
            .collect()
    });
    let _ = std::fs::remove_dir_all(&tmp);

    let mut ok: Vec<(usize, Prepared)> = results.into_iter().filter_map(|(i, r)| r.ok().map(|p| (i, p))).collect();
    if ok.is_empty() {
        bail!("no files could be added");
    }

    match commit(root, &mut ok, tx) {
        Ok(()) => {
            for (i, _) in &ok {
                tx.send(Progress::Item(*i, Status::Done)).ok();
            }
            Ok(ok.len())
        }
        Err(e) => {
            // Leave the iPod as it was: remove the copied audio files.
            for (_, p) in &ok {
                let _ = std::fs::remove_file(&p.file);
            }
            Err(e)
        }
    }
}

fn commit(root: &Path, ok: &mut [(usize, Prepared)], tx: &Sender<Progress>) -> Result<()> {
    let db_path = store::itunesdb_path(root);
    let art_dir = store::artwork_dir(root);
    let art_path = art_dir.join("ArtworkDB");

    tx.send(Progress::Phase("Backing up databases…".into())).ok();
    store::backup(root)?;

    if art_path.exists() {
        tx.send(Progress::Phase("Writing artwork…".into())).ok();
        let with_art: Vec<usize> = (0..ok.len()).filter(|&i| ok[i].1.art.is_some()).collect();
        let images: Vec<NewImage> = with_art
            .iter()
            .map(|&i| {
                let p = &ok[i].1;
                let art = p.art.as_ref().unwrap();
                NewImage { track_dbid: p.track.meta.dbid, image: art.1.clone(), src_size: art.0 }
            })
            .collect();
        if !images.is_empty() {
            let orig = std::fs::read(&art_path)?;
            let (db, ids) = artwrite::add_images(&art_dir, &orig, &images)?;
            store::atomic_write(&art_path, &db)?;
            for ((&i, id), im) in with_art.iter().zip(ids).zip(&images) {
                ok[i].1.track.artwork = Some((id, im.src_size));
            }
        }
    }

    tx.send(Progress::Phase("Writing iTunesDB…".into())).ok();
    let orig = std::fs::read(&db_path)?;
    let parsed = itunesdb::parse(&orig)?;
    let mut tracks: Vec<NewTrack> = ok.iter().map(|(_, p)| p.track.clone()).collect();
    let out = dbwrite::add_tracks(&orig, &parsed, &mut tracks)?;
    // Verify before replacing the original.
    let check = itunesdb::parse(&out)?;
    if check.tracks.len() != parsed.tracks.len() + tracks.len() {
        bail!("verification failed: track count mismatch");
    }
    store::atomic_write(&db_path, &out)?;
    Ok(())
}

/// Free bytes on the filesystem holding `path`.
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_paths() {
        let dir = std::env::temp_dir().join("rpod drop test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a song.flac");
        std::fs::write(&f, b"").unwrap();
        let s = f.to_string_lossy().to_string();
        assert_eq!(parse_dropped(&s), vec![f.clone()]);
        assert_eq!(parse_dropped(&format!("'{s}'")), vec![f.clone()]);
        assert_eq!(parse_dropped(&s.replace(' ', "\\ ")), vec![f.clone()]);
        assert_eq!(parse_dropped(&format!("file://{}", s.replace(' ', "%20"))), vec![f.clone()]);
        assert_eq!(parse_dropped(&format!("'{s}' '{}'", dir.display())), vec![f.clone(), dir.clone()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn picked_cover_is_used_on_import() {
        let Ok(src) = std::env::var("RPOD_TEST_IPOD") else { return };
        let root = std::env::temp_dir().join(format!("rpod-import-cover-{}", std::process::id()));
        for dir in ["iTunes", "Artwork", "Device"] {
            std::fs::create_dir_all(root.join("iPod_Control").join(dir)).unwrap();
            for f in std::fs::read_dir(format!("{src}/iPod_Control/{dir}")).unwrap() {
                let f = f.unwrap();
                std::fs::copy(f.path(), root.join("iPod_Control").join(dir).join(f.file_name())).unwrap();
            }
        }
        std::fs::create_dir_all(root.join("iPod_Control/Music/F00")).unwrap();
        let songs = root.join("songs");
        std::fs::create_dir_all(&songs).unwrap();
        let mp3 = songs.join("bare.mp3");
        let ok = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-c:a", "libmp3lame", "-metadata", "title=Bare Song", "-metadata", "album=No Art Album"])
            .arg(&mp3)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
        let _env = crate::store::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: tests that set XDG_DATA_HOME serialize on TEST_ENV_LOCK.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };

        let mut items = scan(&[mp3], &HashSet::new());
        assert!(!items[0].will_have_art(&Settings::default()));
        let mut png = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(300, 300, image::Rgb([250, 0, 250])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        items[0].cover = Some(Arc::new(png));

        let (tx, rx) = std::sync::mpsc::channel();
        run(root.clone(), items, Settings::default(), tx);
        let finished = rx.iter().find_map(|p| match p {
            Progress::Finished(r) => Some(r),
            _ => None,
        });
        assert_eq!(finished, Some(Ok(1)));

        let ipod = crate::device::Ipod::open(&root).unwrap();
        let t = ipod.db.tracks.iter().find(|t| t.title == "Bare Song").unwrap();
        let px = ipod.art.load(ipod.art.best_thumb(t.dbid).unwrap()).unwrap().to_rgb8();
        assert!(px.get_pixel(50, 50)[0] > 240 && px.get_pixel(50, 50)[1] < 10, "picked cover used");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
