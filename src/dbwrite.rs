//! Writing new tracks into an existing iTunesDB.
//!
//! The writer is conservative: every chunk it doesn't need to change is copied
//! byte-for-byte from the original file, so fields rPod doesn't understand
//! survive untouched. It appends to or removes from the track list, the
//! playlists, the album list (mhla) and the artist list (mhli), and regenerates the master
//! playlist's sort indexes (mhod 52) and letter jump tables (mhod 53), which
//! the iPod's Music menus are built from.

use crate::bytes::{Chunk, utf16le};
use crate::itunesdb::{ITunesDb, Track};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

/// A track to add. `id` is assigned by the writer.
#[derive(Debug, Clone, Default)]
pub struct NewTrack {
    pub meta: Track,
    /// `b"MP3 "` or `b"M4A "`, stored reversed (little-endian) in the file.
    pub filetype: [u8; 4],
    pub vbr: bool,
    /// Artwork image id in ArtworkDB and source image byte size.
    pub artwork: Option<(u32, u32)>,
}

/// Seconds since 1904-01-01, the iPod epoch.
pub fn mac_now() -> u32 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (unix + 2_082_844_800) as u32
}

/// Process-unique random u64 (std's SipHash keys are randomly seeded).
pub fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

// ---------------------------------------------------------------- builders

/// A chunk under construction: a zeroed header of fixed length plus children.
struct Builder {
    buf: Vec<u8>,
}

impl Builder {
    fn new(tag: &[u8; 4], header_len: usize) -> Self {
        let mut buf = vec![0u8; header_len];
        buf[..4].copy_from_slice(tag);
        buf[4..8].copy_from_slice(&(header_len as u32).to_le_bytes());
        Self { buf }
    }

    /// Start from an existing chunk's header bytes.
    fn from_header(c: Chunk) -> Self {
        Self { buf: c.buf[c.off..c.off + c.header_len()].to_vec() }
    }

    /// Setters skip fields beyond the header, so older, shorter header
    /// variants are left alone rather than corrupted.
    fn put(&mut self, off: usize, bytes: &[u8]) -> &mut Self {
        if let Some(dst) = self.buf.get_mut(off..off + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
        self
    }
    fn u8(&mut self, off: usize, v: u8) -> &mut Self {
        self.put(off, &[v])
    }
    fn u16(&mut self, off: usize, v: u16) -> &mut Self {
        self.put(off, &v.to_le_bytes())
    }
    fn u32(&mut self, off: usize, v: u32) -> &mut Self {
        self.put(off, &v.to_le_bytes())
    }
    fn u64(&mut self, off: usize, v: u64) -> &mut Self {
        self.put(off, &v.to_le_bytes())
    }
    fn f32(&mut self, off: usize, v: f32) -> &mut Self {
        self.put(off, &v.to_le_bytes())
    }

    /// Append children and set the total length (third header word).
    fn finish(mut self, children: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(children);
        let total = self.buf.len() as u32;
        self.buf[8..12].copy_from_slice(&total.to_le_bytes());
        self.buf
    }

    /// For list chunks (mhlt/mhlp/mhla/mhli) the third word is an item count.
    fn finish_list(mut self, count: usize, children: &[u8]) -> Vec<u8> {
        self.buf[8..12].copy_from_slice(&(count as u32).to_le_bytes());
        self.buf.extend_from_slice(children);
        self.buf
    }
}

fn string_mhod(ty: u32, s: &str) -> Vec<u8> {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut b = Builder::new(b"mhod", 0x18);
    b.u32(0x0C, ty);
    let mut body = Vec::with_capacity(16 + utf16.len());
    body.extend_from_slice(&1u32.to_le_bytes()); // position / UTF-16 marker
    body.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&utf16);
    b.finish(&body)
}

fn mhit(t: &NewTrack, id: u32, header_len: usize, album_id: u32, artist_id: u32) -> Vec<u8> {
    let m = &t.meta;
    let now = mac_now();
    let mut h = Builder::new(b"mhit", header_len.max(0x270));
    h.u32(0x10, id)
        .u32(0x14, 1)
        .u32(0x18, u32::from_le_bytes(t.filetype).swap_bytes())
        .u8(0x1C, t.vbr as u8)
        .u8(0x1D, (&t.filetype == b"MP3 ") as u8)
        .u8(0x1E, m.compilation as u8)
        .u8(0x1F, m.rating)
        .u32(0x20, now)
        .u32(0x24, m.size)
        .u32(0x28, m.length_ms)
        .u32(0x2C, m.track_no)
        .u32(0x30, m.track_total)
        .u32(0x34, m.year)
        .u32(0x38, m.bitrate)
        .u32(0x3C, m.sample_rate << 16)
        .u32(0x5C, m.disc_no)
        .u32(0x60, m.disc_total)
        .u32(0x68, now)
        .u64(0x70, m.dbid)
        .u16(0x7E, 0xFFFF)
        .f32(0x88, m.sample_rate as f32)
        .u64(0xA8, m.dbid)
        .u8(0xB2, 2) // not played yet
        .u64(0xBC, m.sample_rate as u64 * m.length_ms as u64 / 1000)
        .u32(0xD0, 1) // media type: audio
        .u32(0x120, album_id)
        .u32(0x12C, m.size)
        .u32(0x168, 1)
        .u8(0x197, 1)
        .u32(0x1E0, artist_id)
        .u32(0x1EC, 1)
        .u32(0x20C, 2);
    match t.artwork {
        Some((image_id, src_size)) => {
            h.u16(0x7C, 1).u32(0x80, src_size).u8(0xA4, 1).u32(0x160, image_id);
        }
        None => {
            h.u8(0xA4, 2);
        }
    }

    let strings: [(u32, &str); 9] = [
        (1, &m.title),
        (4, &m.artist),
        (3, &m.album),
        (5, &m.genre),
        (6, &m.kind),
        (2, &format!(":{}", m.location.replace('/', ":"))),
        (22, &m.album_artist),
        (12, &m.composer),
        (8, &m.comment),
    ];
    let mut kids = Vec::new();
    let mut n = 0;
    for (ty, s) in strings {
        if !s.is_empty() {
            kids.extend(string_mhod(ty, s));
            n += 1;
        }
    }
    h.u32(0x0C, n);
    h.finish(&kids)
}

fn mhip(track_id: u32) -> Vec<u8> {
    let mut od = Builder::new(b"mhod", 0x18);
    od.u32(0x0C, 100);
    let od = od.finish(&[0u8; 0x14]);
    let mut h = Builder::new(b"mhip", 0x4C);
    h.u32(0x0C, 1).u32(0x18, track_id);
    h.finish(&od)
}

// ---------------------------------------------------------------- sort indexes

/// Sort types used by mhod 52/53 in the master playlist.
const SORT_TITLE: u32 = 3;
const SORT_ALBUM: u32 = 4;
const SORT_ARTIST: u32 = 5;
const SORT_GENRE: u32 = 7;
const SORT_COMPOSER: u32 = 18;

/// Case-insensitive key that files "The Strokes" under S and "A Tribe
/// Called Quest" under T, as iTunes does.
fn sort_key(s: &str) -> String {
    let f = fold(s);
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = f.strip_prefix(article) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    f
}

/// iTunes' stored "sort as" form: "The Strokes" → "Strokes, The".
pub fn sort_form(s: &str) -> String {
    for article in ["the ", "a ", "an "] {
        if s.len() > article.len() && s.is_char_boundary(article.len()) && s[..article.len()].eq_ignore_ascii_case(article) {
            return format!("{}, {}", &s[article.len()..], s[..article.len()].trim_end());
        }
    }
    s.to_string()
}

fn jump_letter(key: &str) -> u16 {
    match key.chars().next() {
        Some(c) if c.is_alphabetic() => c.to_uppercase().next().map_or('0' as u16, |u| {
            let mut buf = [0u16; 2];
            u.encode_utf16(&mut buf)[0]
        }),
        _ => '0' as u16,
    }
}

/// Track order (positions in the track list) for one sort type, plus the
/// primary key of each, for the jump table.
fn sorted_positions(tracks: &[&Track], sort: u32) -> Vec<(usize, String)> {
    let keyed: Vec<(usize, Vec<String>, (u32, u32))> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (artist, album, title) = (sort_key(&t.artist), sort_key(&t.album), sort_key(&t.title));
            let primary = match sort {
                SORT_TITLE => vec![title],
                SORT_ALBUM => vec![album, title],
                SORT_ARTIST => vec![artist, album, title],
                SORT_GENRE => vec![sort_key(&t.genre), artist, album, title],
                _ => vec![sort_key(&t.composer), title],
            };
            (i, primary, (t.disc_no, t.track_no))
        })
        .collect();
    let mut keyed = keyed;
    keyed.sort_by(|a, b| {
        // Album-based sorts keep disc/track order inside the album.
        let disc_track_matters = matches!(sort, SORT_ALBUM | SORT_ARTIST | SORT_GENRE);
        let n = a.1.len() - 1;
        a.1[..n]
            .cmp(&b.1[..n])
            .then_with(|| if disc_track_matters { a.2.cmp(&b.2) } else { std::cmp::Ordering::Equal })
            .then_with(|| a.1[n].cmp(&b.1[n]))
    });
    keyed.into_iter().map(|(i, k, _)| (i, k.into_iter().next().unwrap_or_default())).collect()
}

fn index_mhod(sort: u32, order: &[(usize, String)]) -> Vec<u8> {
    let mut b = Builder::new(b"mhod", 0x18);
    b.u32(0x0C, 52);
    let mut body = Vec::with_capacity(48 + order.len() * 4);
    body.extend_from_slice(&sort.to_le_bytes());
    body.extend_from_slice(&(order.len() as u32).to_le_bytes());
    body.extend_from_slice(&[0u8; 40]);
    for (pos, _) in order {
        body.extend_from_slice(&(*pos as u32).to_le_bytes());
    }
    b.finish(&body)
}

fn jump_mhod(sort: u32, order: &[(usize, String)]) -> Vec<u8> {
    let mut runs: Vec<(u16, u32, u32)> = Vec::new();
    for (i, (_, key)) in order.iter().enumerate() {
        let letter = jump_letter(key);
        match runs.last_mut() {
            Some(r) if r.0 == letter => r.2 += 1,
            _ => runs.push((letter, i as u32, 1)),
        }
    }
    let mut b = Builder::new(b"mhod", 0x18);
    b.u32(0x0C, 53);
    let mut body = Vec::with_capacity(16 + runs.len() * 12);
    body.extend_from_slice(&sort.to_le_bytes());
    body.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    body.extend_from_slice(&[0u8; 8]);
    for (letter, start, count) in runs {
        body.extend_from_slice(&letter.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body.extend_from_slice(&start.to_le_bytes());
        body.extend_from_slice(&count.to_le_bytes());
    }
    b.finish(&body)
}

// ---------------------------------------------------------------- album / artist lists

fn read_mhod_string(od: Chunk) -> String {
    let len = od.u32(0x1C) as usize;
    od.slice(0x28, len).map(utf16le).unwrap_or_default()
}

/// Existing (album, album artist) → album id, read from the mhla.
fn album_ids(mhla: Chunk) -> Result<HashMap<(String, String), u32>> {
    let mut map = HashMap::new();
    let mut off = mhla.off + mhla.header_len();
    for _ in 0..mhla.total_len() {
        let ia = Chunk::at(mhla.buf, off)?;
        ia.expect(b"mhia")?;
        let (mut album, mut artist, mut album_artist) = (String::new(), String::new(), String::new());
        let mut o = ia.off + ia.header_len();
        for _ in 0..ia.u32(0x0C) {
            let od = Chunk::at(ia.buf, o)?;
            match od.u32(0x0C) {
                200 => album = read_mhod_string(od),
                201 => artist = read_mhod_string(od),
                202 => album_artist = read_mhod_string(od),
                _ => {}
            }
            o = od.end();
        }
        let who = if album_artist.is_empty() { artist } else { album_artist };
        map.insert((fold(&album), fold(&who)), ia.u32(0x10));
        off = ia.end();
    }
    Ok(map)
}

fn artist_ids(mhli: Chunk) -> Result<HashMap<String, u32>> {
    let mut map = HashMap::new();
    let mut off = mhli.off + mhli.header_len();
    for _ in 0..mhli.total_len() {
        let ii = Chunk::at(mhli.buf, off)?;
        ii.expect(b"mhii")?;
        let od = ii.first_child()?;
        if ii.u32(0x0C) > 0 && od.u32(0x0C) == 300 {
            map.insert(fold(&read_mhod_string(od)), ii.u32(0x10));
        }
        off = ii.end();
    }
    Ok(map)
}

fn album_key(t: &Track) -> (String, String) {
    let who = if t.album_artist.is_empty() { &t.artist } else { &t.album_artist };
    (fold(&t.album), fold(who))
}

fn mhia(id: u32, t: &Track) -> Vec<u8> {
    let mut h = Builder::new(b"mhia", 0x58);
    h.u32(0x0C, 3).u32(0x10, id).u64(0x14, rand_u64()).u16(0x1C, 2);
    let mut kids = string_mhod(200, &t.album);
    kids.extend(string_mhod(201, &t.artist));
    kids.extend(string_mhod(202, if t.album_artist.is_empty() { &t.artist } else { &t.album_artist }));
    h.finish(&kids)
}

fn mhii_artist(id: u32, name: &str) -> Vec<u8> {
    let mut h = Builder::new(b"mhii", 0x50);
    h.u32(0x0C, 1).u32(0x10, id).u64(0x14, rand_u64()).u32(0x1C, 2);
    h.finish(&string_mhod(300, name))
}

// ---------------------------------------------------------------- edits

/// Changes to an existing track. `None` leaves a field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub comment: Option<String>,
    pub year: Option<u32>,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_no: Option<u32>,
    pub disc_total: Option<u32>,
    pub compilation: Option<bool>,
    /// 0–100, 20 per star.
    pub rating: Option<u8>,
    /// New cover: (ArtworkDB image id, source image byte size).
    pub artwork: Option<(u32, u32)>,
}

impl TrackEdit {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn apply(&self, t: &mut Track) {
        fn set<T: Clone>(dst: &mut T, v: &Option<T>) {
            if let Some(v) = v {
                *dst = v.clone();
            }
        }
        set(&mut t.title, &self.title);
        set(&mut t.artist, &self.artist);
        set(&mut t.album, &self.album);
        set(&mut t.album_artist, &self.album_artist);
        set(&mut t.genre, &self.genre);
        set(&mut t.composer, &self.composer);
        set(&mut t.comment, &self.comment);
        set(&mut t.year, &self.year);
        set(&mut t.track_no, &self.track_no);
        set(&mut t.track_total, &self.track_total);
        set(&mut t.disc_no, &self.disc_no);
        set(&mut t.disc_total, &self.disc_total);
        set(&mut t.compilation, &self.compilation);
        set(&mut t.rating, &self.rating);
        if self.artwork.is_some() {
            t.has_artwork = true;
        }
    }

    /// Whether the track may need to move to another album/artist entry.
    fn relinks(&self) -> bool {
        self.artist.is_some() || self.album.is_some() || self.album_artist.is_some()
    }

    /// (string mhod type, its "sort as" mhod type, new value) per edited text field.
    fn strings(&self) -> Vec<(u32, Option<u32>, &str)> {
        [
            (1, Some(27), &self.title),
            (4, Some(23), &self.artist),
            (3, Some(28), &self.album),
            (22, Some(29), &self.album_artist),
            (12, Some(30), &self.composer),
            (5, None, &self.genre),
            (8, None, &self.comment),
        ]
        .into_iter()
        .filter_map(|(ty, sort, v)| v.as_deref().map(|v| (ty, sort, v)))
        .collect()
    }
}

/// Rewrite one existing mhit with `e` applied. Untouched fields and string
/// records are copied verbatim; an edited field's "sort as" record is
/// regenerated so the iPod doesn't keep sorting by the old value.
fn rewrite_mhit(it: Chunk, e: &TrackEdit, links: Option<(u32, u32)>) -> Result<Vec<u8>> {
    let mut h = Builder::from_header(it);
    let opt32 = |h: &mut Builder, off: usize, v: Option<u32>| {
        if let Some(v) = v {
            h.u32(off, v);
        }
    };
    if let Some(v) = e.compilation {
        h.u8(0x1E, v as u8);
    }
    if let Some(v) = e.rating {
        h.u8(0x1F, v);
    }
    opt32(&mut h, 0x2C, e.track_no);
    opt32(&mut h, 0x30, e.track_total);
    opt32(&mut h, 0x34, e.year);
    opt32(&mut h, 0x5C, e.disc_no);
    opt32(&mut h, 0x60, e.disc_total);
    h.u32(0x20, mac_now());
    if let Some((album, artist)) = links {
        h.u32(0x120, album).u32(0x1E0, artist);
    }
    if let Some((image_id, src_size)) = e.artwork {
        h.u16(0x7C, 1).u32(0x80, src_size).u8(0xA4, 1).u32(0x160, image_id);
    }

    let strings = e.strings();
    let mut kids = Vec::new();
    let mut count = 0u32;
    let mut replaced = Vec::new();
    let mut off = it.off + it.header_len();
    for _ in 0..it.u32(0x0C) {
        let od = Chunk::at(it.buf, off)?;
        od.expect(b"mhod")?;
        let ty = od.u32(0x0C);
        if let Some(&(_, _, v)) = strings.iter().find(|(t, _, _)| *t == ty) {
            replaced.push(ty);
            if !v.is_empty() {
                kids.extend(string_mhod(ty, v));
                count += 1;
            }
        } else if let Some(&(_, _, v)) = strings.iter().find(|(_, sort, _)| *sort == Some(ty)) {
            if !v.is_empty() {
                kids.extend(string_mhod(ty, &sort_form(v)));
                count += 1;
            }
        } else {
            kids.extend_from_slice(&it.buf[od.off..od.end()]);
            count += 1;
        }
        off = od.end();
    }
    // Fields the track didn't have before.
    for &(ty, _, v) in &strings {
        if !replaced.contains(&ty) && !v.is_empty() {
            kids.extend(string_mhod(ty, v));
            count += 1;
        }
    }
    h.u32(0x0C, count);
    Ok(h.finish(&kids))
}

/// Finds or creates the album (mhia) and artist (mhii) list entries tracks link to.
struct Linker {
    albums: HashMap<(String, String), u32>,
    artists: HashMap<String, u32>,
    next_album: u32,
    next_artist: u32,
    new_albums: Vec<Vec<u8>>,
    new_artists: Vec<Vec<u8>>,
}

impl Linker {
    fn read(mhbd: Chunk) -> Result<Self> {
        let mut albums = HashMap::new();
        let mut artists = HashMap::new();
        for_each_mhsd(mhbd, |sd| {
            match sd.u32(0x0C) {
                4 => albums = album_ids(sd.first_child()?)?,
                8 => artists = artist_ids(sd.first_child()?)?,
                _ => {}
            }
            Ok(())
        })?;
        Ok(Self {
            next_album: albums.values().max().copied().unwrap_or(0) + 1,
            next_artist: artists.values().max().copied().unwrap_or(0) + 1,
            albums,
            artists,
            new_albums: Vec::new(),
            new_artists: Vec::new(),
        })
    }

    fn link(&mut self, t: &Track) -> (u32, u32) {
        let album = *self.albums.entry(album_key(t)).or_insert_with(|| {
            self.new_albums.push(mhia(self.next_album, t));
            self.next_album += 1;
            self.next_album - 1
        });
        let artist = *self.artists.entry(fold(&t.artist)).or_insert_with(|| {
            self.new_artists.push(mhii_artist(self.next_artist, &t.artist));
            self.next_artist += 1;
            self.next_artist - 1
        });
        (album, artist)
    }
}

// ---------------------------------------------------------------- top level

/// Produce a new iTunesDB with `new` appended. Assigns `meta.id` on each.
pub fn add_tracks(orig: &[u8], existing: &ITunesDb, new: &mut [NewTrack]) -> Result<Vec<u8>> {
    write(orig, existing, new, &HashMap::new(), &HashSet::new())
}

/// Apply `edits` (keyed by track id) to existing tracks.
pub fn edit_tracks(orig: &[u8], existing: &ITunesDb, edits: &HashMap<u32, TrackEdit>) -> Result<Vec<u8>> {
    write(orig, existing, &mut [], edits, &HashSet::new())
}

/// Remove tracks (by id) from the track list and every playlist. Album and
/// artist entries no remaining track uses go too.
pub fn remove_tracks(orig: &[u8], existing: &ITunesDb, remove: &HashSet<u32>) -> Result<Vec<u8>> {
    write(orig, existing, &mut [], &HashMap::new(), remove)
}

/// The general writer: edit, remove and append tracks in one pass.
pub fn write(
    orig: &[u8],
    existing: &ITunesDb,
    new: &mut [NewTrack],
    edits: &HashMap<u32, TrackEdit>,
    remove: &HashSet<u32>,
) -> Result<Vec<u8>> {
    let mhbd = Chunk::at(orig, 0)?;
    mhbd.expect(b"mhbd")?;

    let mut next_id = existing.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    for t in new.iter_mut() {
        t.meta.id = next_id;
        next_id += 1;
    }

    // Each existing track's current (album id, artist id), read from its mhit.
    let mut current_links: Vec<(u32, u32)> = Vec::with_capacity(existing.tracks.len());
    for_each_mhsd(mhbd, |sd| {
        if sd.u32(0x0C) == 1 {
            let mhlt = sd.first_child()?;
            let mut off = mhlt.off + mhlt.header_len();
            for _ in 0..mhlt.total_len() {
                let it = Chunk::at(orig, off)?;
                current_links.push((it.u32(0x120), it.u32(0x1E0)));
                off = it.end();
            }
        }
        Ok(())
    })?;

    // Tracks as they'll be after editing, for relinking and sort indexes.
    let mut current: Vec<Track> = existing.tracks.clone();
    let mut linker = Linker::read(mhbd)?;
    let mut relinked: HashMap<u32, (u32, u32)> = HashMap::new();
    for t in current.iter_mut() {
        if let Some(e) = edits.get(&t.id) {
            e.apply(t);
            if e.relinks() {
                relinked.insert(t.id, linker.link(t));
            }
        }
    }
    let links: Vec<(u32, u32)> = new.iter().map(|t| linker.link(&t.meta)).collect();

    // Album/artist entries only edited or removed tracks used, and nobody uses now, are dropped.
    let final_links: Vec<(u32, u32)> = current
        .iter()
        .zip(&current_links)
        .filter(|(t, _)| !remove.contains(&t.id))
        .map(|(t, l)| relinked.get(&t.id).copied().unwrap_or(*l))
        .chain(links.iter().copied())
        .collect();
    let used_albums: HashSet<u32> = final_links.iter().map(|l| l.0).collect();
    let used_artists: HashSet<u32> = final_links.iter().map(|l| l.1).collect();
    let mut drop_albums = HashSet::new();
    let mut drop_artists = HashSet::new();
    for (t, l) in existing.tracks.iter().zip(&current_links) {
        if relinked.contains_key(&t.id) || remove.contains(&t.id) {
            if !used_albums.contains(&l.0) {
                drop_albums.insert(l.0);
            }
            if !used_artists.contains(&l.1) {
                drop_artists.insert(l.1);
            }
        }
    }

    let all: Vec<&Track> = current.iter().filter(|t| !remove.contains(&t.id)).chain(new.iter().map(|t| &t.meta)).collect();

    let mut sections = Vec::new();
    for_each_mhsd(mhbd, |sd| {
        let body = match sd.u32(0x0C) {
            1 => {
                let mhlt = sd.first_child()?;
                mhlt.expect(b"mhlt")?;
                let mut items = Vec::with_capacity(sd.end() - mhlt.off);
                let mut mhit_len = 0x270;
                let mut kept = 0;
                let mut off = mhlt.off + mhlt.header_len();
                for _ in 0..mhlt.total_len() {
                    let it = Chunk::at(orig, off)?;
                    it.expect(b"mhit")?;
                    mhit_len = it.header_len();
                    off = it.end();
                    let id = it.u32(0x10);
                    if remove.contains(&id) {
                        continue;
                    }
                    match edits.get(&id) {
                        Some(e) => items.extend(rewrite_mhit(it, e, relinked.get(&id).copied())?),
                        None => items.extend_from_slice(&orig[it.off..it.end()]),
                    }
                    kept += 1;
                }
                for (t, (album, artist)) in new.iter().zip(&links) {
                    items.extend(mhit(t, t.meta.id, mhit_len, *album, *artist));
                }
                Builder::from_header(mhlt).finish_list(kept + new.len(), &items)
            }
            2 | 3 => rewrite_playlists(sd.first_child()?, new, &all, remove)?,
            4 => rewrite_list(sd.first_child()?, b"mhla", &linker.new_albums, &drop_albums)?,
            8 => rewrite_list(sd.first_child()?, b"mhli", &linker.new_artists, &drop_artists)?,
            _ => orig[sd.off + sd.header_len()..sd.end()].to_vec(),
        };
        sections.extend(Builder::from_header(sd).finish(&body));
        Ok(())
    })?;

    Ok(Builder::from_header(mhbd).finish(&sections))
}

fn for_each_mhsd<'a>(mhbd: Chunk<'a>, mut f: impl FnMut(Chunk<'a>) -> Result<()>) -> Result<()> {
    let mut sd = mhbd.first_child()?;
    for i in 0..mhbd.u32(0x14) {
        sd.expect(b"mhsd")?;
        f(sd)?;
        if i + 1 < mhbd.u32(0x14) {
            sd = Chunk::at(mhbd.buf, sd.end())?;
        }
    }
    Ok(())
}

/// Copy an album/artist list, dropping entries by id and appending new ones.
fn rewrite_list(list: Chunk, tag: &[u8; 4], extra: &[Vec<u8>], drop: &HashSet<u32>) -> Result<Vec<u8>> {
    list.expect(tag)?;
    let mut items = Vec::new();
    let mut kept = 0;
    let mut off = list.off + list.header_len();
    for _ in 0..list.total_len() {
        let item = Chunk::at(list.buf, off)?;
        if !drop.contains(&item.u32(0x10)) {
            items.extend_from_slice(&list.buf[item.off..item.end()]);
            kept += 1;
        }
        off = item.end();
    }
    for e in extra {
        items.extend_from_slice(e);
    }
    Ok(Builder::from_header(list).finish_list(kept + extra.len(), &items))
}

fn rewrite_playlists(mhlp: Chunk, new: &[NewTrack], all: &[&Track], remove: &HashSet<u32>) -> Result<Vec<u8>> {
    mhlp.expect(b"mhlp")?;
    let buf = mhlp.buf;
    let mut out = Vec::new();
    let mut off = mhlp.off + mhlp.header_len();
    for _ in 0..mhlp.total_len() {
        let yp = Chunk::at(buf, off)?;
        yp.expect(b"mhyp")?;
        if yp.u8(0x14) == 0 && remove.is_empty() {
            out.extend_from_slice(&buf[yp.off..yp.end()]);
        } else {
            out.extend(rewrite_playlist(yp, new, all, remove)?);
        }
        off = yp.end();
    }
    Ok(Builder::from_header(mhlp).finish_list(mhlp.total_len(), &out))
}

/// Drop removed tracks' items from a playlist. The master playlist also gets
/// the new tracks and fresh sort indexes.
fn rewrite_playlist(yp: Chunk, new: &[NewTrack], all: &[&Track], remove: &HashSet<u32>) -> Result<Vec<u8>> {
    let buf = yp.buf;
    let master = yp.u8(0x14) != 0;
    let mut kids = Vec::new();
    let mut off = yp.off + yp.header_len();
    let mut orders: HashMap<u32, Vec<(usize, String)>> = HashMap::new();
    for _ in 0..yp.u32(0x0C) {
        let od = Chunk::at(buf, off)?;
        od.expect(b"mhod")?;
        match od.u32(0x0C) {
            ty @ (52 | 53) if master => {
                let sort = od.u32(0x18);
                if ![SORT_TITLE, SORT_ALBUM, SORT_ARTIST, SORT_GENRE, SORT_COMPOSER].contains(&sort) {
                    bail!("unknown library index sort type {sort}");
                }
                let order = orders.entry(sort).or_insert_with(|| sorted_positions(all, sort));
                kids.extend(if ty == 52 { index_mhod(sort, order) } else { jump_mhod(sort, order) });
            }
            _ => kids.extend_from_slice(&buf[od.off..od.end()]),
        }
        off = od.end();
    }
    let mut items = 0;
    for _ in 0..yp.u32(0x10) {
        let ip = Chunk::at(buf, off)?;
        ip.expect(b"mhip")?;
        if !remove.contains(&ip.u32(0x18)) {
            kids.extend_from_slice(&buf[ip.off..ip.end()]);
            items += 1;
        }
        off = ip.end();
    }
    kids.extend_from_slice(&buf[off..yp.end()]);
    if master {
        for t in new {
            kids.extend(mhip(t.meta.id));
            items += 1;
        }
    }
    let mut h = Builder::from_header(yp);
    h.u32(0x10, items);
    Ok(h.finish(&kids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::itunesdb;

    fn sample() -> Option<Vec<u8>> {
        let root = std::env::var("RPOD_TEST_IPOD").ok()?;
        std::fs::read(format!("{root}/iPod_Control/iTunes/iTunesDB")).ok()
    }

    #[test]
    fn rewrite_without_changes_keeps_everything_but_indexes() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let out = add_tracks(&orig, &db, &mut []).unwrap();
        let re = itunesdb::parse(&out).unwrap();
        assert_eq!(re.tracks.len(), db.tracks.len());
        assert_eq!(re.playlists.iter().map(|p| p.items.len()).collect::<Vec<_>>(),
                   db.playlists.iter().map(|p| p.items.len()).collect::<Vec<_>>());
        // Track list section (up to the first playlist mhsd) must be byte-identical.
        if let Ok(p) = std::env::var("RPOD_TEST_OUT") { std::fs::write(p, &out).unwrap(); }
        assert!(out[0xF4..0x1C982C] == orig[0xF4..0x1C982C]);
    }

    #[test]
    fn add_track_round_trips() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let mut t = NewTrack { filetype: *b"M4A ", ..Default::default() };
        t.meta = Track {
            title: "Test Song".into(),
            artist: "Brand New Artist".into(),
            album: "Brand New Album".into(),
            genre: "Test".into(),
            kind: "AAC audio file".into(),
            location: "iPod_Control/Music/F00/TEST.m4a".into(),
            size: 1234567,
            length_ms: 200_000,
            track_no: 3,
            year: 2024,
            bitrate: 256,
            sample_rate: 44100,
            dbid: 0x1122334455667788,
            ..Default::default()
        };
        t.artwork = Some((5000, 12345));
        let mut new = [t];
        let out = add_tracks(&orig, &db, &mut new).unwrap();
        let re = itunesdb::parse(&out).unwrap();
        assert_eq!(re.tracks.len(), db.tracks.len() + 1);
        let added = re.tracks.last().unwrap();
        assert_eq!(added.title, "Test Song");
        assert_eq!(added.location, "iPod_Control/Music/F00/TEST.m4a");
        assert_eq!(added.dbid, 0x1122334455667788);
        assert_eq!(added.track_no, 3);
        assert!(added.has_artwork);
        assert!(re.playlists.iter().filter(|p| p.is_master).all(|p| p.items.contains(&added.id)));
        // Total length in mhbd must equal file length.
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize, out.len());
    }

    /// Raw mhit chunks keyed by track id.
    fn mhits(buf: &[u8]) -> HashMap<u32, Vec<u8>> {
        let mut out = HashMap::new();
        let mhbd = Chunk::at(buf, 0).unwrap();
        for_each_mhsd(mhbd, |sd| {
            if sd.u32(0x0C) == 1 {
                let mhlt = sd.first_child()?;
                let mut off = mhlt.off + mhlt.header_len();
                for _ in 0..mhlt.total_len() {
                    let it = Chunk::at(buf, off)?;
                    out.insert(it.u32(0x10), buf[it.off..it.end()].to_vec());
                    off = it.end();
                }
            }
            Ok(())
        })
        .unwrap();
        out
    }

    fn mhod_value(mhit_bytes: &[u8], ty: u32) -> Option<String> {
        let it = Chunk::at(mhit_bytes, 0).unwrap();
        let mut off = it.header_len();
        for _ in 0..it.u32(0x0C) {
            let od = Chunk::at(mhit_bytes, off).unwrap();
            if od.u32(0x0C) == ty {
                return Some(read_mhod_string(od));
            }
            off = od.end();
        }
        None
    }

    fn list_count(buf: &[u8], ty: u32) -> usize {
        let mut n = 0;
        for_each_mhsd(Chunk::at(buf, 0).unwrap(), |sd| {
            if sd.u32(0x0C) == ty {
                n = sd.first_child()?.total_len();
            }
            Ok(())
        })
        .unwrap();
        n
    }

    #[test]
    fn edit_text_and_numbers() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let id = db.tracks[0].id;
        let edit = TrackEdit {
            title: Some("Új cím – ő".into()),
            artist: Some("The New Band".into()),
            year: Some(1999),
            rating: Some(80),
            comment: Some(String::new()),
            ..Default::default()
        };
        let out = edit_tracks(&orig, &db, &HashMap::from([(id, edit)])).unwrap();
        let re = itunesdb::parse(&out).unwrap();
        let t = re.tracks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.title, "Új cím – ő");
        assert_eq!(t.artist, "The New Band");
        assert_eq!((t.year, t.rating), (1999, 80));
        assert_eq!(t.album, db.tracks[0].album, "untouched fields survive");
        assert_eq!(t.location, db.tracks[0].location);
        assert_eq!(mhod_value(&mhits(&out)[&id], 23).as_deref(), Some("New Band, The"));

        let (before, after) = (mhits(&orig), mhits(&out));
        for (tid, raw) in &before {
            if *tid != id {
                assert!(after[tid] == *raw, "track {tid} changed");
            }
        }
        assert_eq!(re.tracks.len(), db.tracks.len());
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize, out.len());
    }

    #[test]
    fn move_track_into_existing_album() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let (a, b) = (&db.tracks[0], db.tracks.iter().find(|t| t.album != db.tracks[0].album).unwrap());
        let edit = TrackEdit {
            album: Some(b.album.clone()),
            album_artist: Some(b.album_artist.clone()),
            artist: Some(b.artist.clone()),
            ..Default::default()
        };
        let out = edit_tracks(&orig, &db, &HashMap::from([(a.id, edit)])).unwrap();
        let m = mhits(&out);
        let album_of = |id: u32| Chunk::at(&m[&id], 0).unwrap().u32(0x120);
        assert_eq!(album_of(a.id), album_of(b.id));
        assert!(list_count(&out, 4) <= list_count(&orig, 4), "no new album entry needed");
    }

    #[test]
    fn rename_whole_album_replaces_its_entry() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let m = mhits(&orig);
        let album_id = |id: u32| Chunk::at(&m[&id], 0).unwrap().u32(0x120);
        let target = album_id(db.tracks[0].id);
        let edits: HashMap<u32, TrackEdit> = db
            .tracks
            .iter()
            .filter(|t| album_id(t.id) == target)
            .map(|t| (t.id, TrackEdit { album: Some("Renamed Album".into()), ..Default::default() }))
            .collect();
        let out = edit_tracks(&orig, &db, &edits).unwrap();
        assert_eq!(list_count(&out, 4), list_count(&orig, 4), "one entry added, the stale one dropped");
        let m2 = mhits(&out);
        let new_ids: HashSet<u32> = edits.keys().map(|id| Chunk::at(&m2[id], 0).unwrap().u32(0x120)).collect();
        assert_eq!(new_ids.len(), 1);
        assert!(!new_ids.contains(&target));
        let re = itunesdb::parse(&out).unwrap();
        assert!(re.tracks.iter().filter(|t| edits.contains_key(&t.id)).all(|t| t.album == "Renamed Album"));
    }

    #[test]
    fn remove_album_drops_its_tracks_items_and_entries() {
        let Some(orig) = sample() else { return };
        let db = itunesdb::parse(&orig).unwrap();
        let m = mhits(&orig);
        let album_id = |id: u32| Chunk::at(&m[&id], 0).unwrap().u32(0x120);
        let target = album_id(db.tracks[0].id);
        let mut remove: HashSet<u32> = db.tracks.iter().filter(|t| album_id(t.id) == target).map(|t| t.id).collect();
        // Plus a song from a regular playlist, if there is one.
        let listed = db.playlists.iter().position(|p| !p.is_master && !p.items.is_empty());
        if let Some(p) = listed {
            remove.insert(db.playlists[p].items[0]);
        }
        let out = remove_tracks(&orig, &db, &remove).unwrap();
        let re = itunesdb::parse(&out).unwrap();

        assert_eq!(re.tracks.len(), db.tracks.len() - remove.len());
        assert!(re.tracks.iter().all(|t| !remove.contains(&t.id)));
        assert!(re.playlists.iter().all(|p| p.items.iter().all(|i| !remove.contains(i))));
        let master = re.playlists.iter().find(|p| p.is_master).unwrap();
        assert_eq!(master.items.len(), re.tracks.len());
        assert!(list_count(&out, 4) < list_count(&orig, 4), "the album's entry is gone");
        if let Some(p) = listed {
            let gone = db.playlists[p].items.iter().filter(|i| remove.contains(i)).count();
            assert_eq!(re.playlists[p].items.len(), db.playlists[p].items.len() - gone);
        }
        let after = mhits(&out);
        assert!(after.iter().all(|(id, raw)| m[id] == *raw), "kept tracks are untouched");
        assert_eq!(u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize, out.len());
    }

    #[test]
    fn sort_forms() {
        assert_eq!(sort_form("The Strokes"), "Strokes, The");
        assert_eq!(sort_form("A Tribe Called Quest"), "Tribe Called Quest, A");
        assert_eq!(sort_form("Theory of a Deadman"), "Theory of a Deadman");
        assert_eq!(sort_form("Ő"), "Ő");
    }
}
