//! Writing new tracks into an existing iTunesDB.
//!
//! The writer is conservative: every chunk it doesn't need to change is copied
//! byte-for-byte from the original file, so fields rPod doesn't understand
//! survive untouched. It appends to the track list, the master playlist, the
//! album list (mhla) and the artist list (mhli), and regenerates the master
//! playlist's sort indexes (mhod 52) and letter jump tables (mhod 53), which
//! the iPod's Music menus are built from.

use crate::bytes::{Chunk, utf16le};
use crate::itunesdb::{ITunesDb, Track};
use anyhow::{Result, bail};
use std::collections::HashMap;

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

    fn u8(&mut self, off: usize, v: u8) -> &mut Self {
        self.buf[off] = v;
        self
    }
    fn u16(&mut self, off: usize, v: u16) -> &mut Self {
        self.buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, off: usize, v: u32) -> &mut Self {
        self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, off: usize, v: u64) -> &mut Self {
        self.buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, off: usize, v: f32) -> &mut Self {
        self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        self
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

/// Case-insensitive key that files "The Strokes" under S, as iTunes does.
fn sort_key(s: &str) -> String {
    let f = fold(s);
    f.strip_prefix("the ").map(str::to_string).unwrap_or(f)
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

// ---------------------------------------------------------------- top level

/// Produce a new iTunesDB with `new` appended. Assigns `meta.id` on each.
pub fn add_tracks(orig: &[u8], existing: &ITunesDb, new: &mut [NewTrack]) -> Result<Vec<u8>> {
    let mhbd = Chunk::at(orig, 0)?;
    mhbd.expect(b"mhbd")?;

    let mut next_id = existing.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
    for t in new.iter_mut() {
        t.meta.id = next_id;
        next_id += 1;
    }

    // Pre-scan the album/artist lists so new tracks can join existing albums.
    let mut albums: HashMap<(String, String), u32> = HashMap::new();
    let mut artists: HashMap<String, u32> = HashMap::new();
    for_each_mhsd(mhbd, |sd| {
        match sd.u32(0x0C) {
            4 => albums = album_ids(sd.first_child()?)?,
            8 => artists = artist_ids(sd.first_child()?)?,
            _ => {}
        }
        Ok(())
    })?;
    let mut new_albums: Vec<Vec<u8>> = Vec::new();
    let mut new_artists: Vec<Vec<u8>> = Vec::new();
    let mut next_album = albums.values().max().copied().unwrap_or(0) + 1;
    let mut next_artist = artists.values().max().copied().unwrap_or(0) + 1;
    let mut links = Vec::with_capacity(new.len());
    for t in new.iter() {
        let album_id = *albums.entry(album_key(&t.meta)).or_insert_with(|| {
            new_albums.push(mhia(next_album, &t.meta));
            next_album += 1;
            next_album - 1
        });
        let artist_id = *artists.entry(fold(&t.meta.artist)).or_insert_with(|| {
            new_artists.push(mhii_artist(next_artist, &t.meta.artist));
            next_artist += 1;
            next_artist - 1
        });
        links.push((album_id, artist_id));
    }

    // Track list in final order, for the sort indexes.
    let all: Vec<&Track> = existing.tracks.iter().chain(new.iter().map(|t| &t.meta)).collect();

    let mut sections = Vec::new();
    for_each_mhsd(mhbd, |sd| {
        let body = match sd.u32(0x0C) {
            1 => {
                let mhlt = sd.first_child()?;
                mhlt.expect(b"mhlt")?;
                let mhit_len = Chunk::at(orig, mhlt.off + mhlt.header_len())
                    .map(|c| c.header_len())
                    .unwrap_or(0x270);
                let mut items = orig[mhlt.off + mhlt.header_len()..sd.end()].to_vec();
                for (t, (album, artist)) in new.iter().zip(&links) {
                    items.extend(mhit(t, t.meta.id, mhit_len, *album, *artist));
                }
                Builder::from_header(mhlt).finish_list(mhlt.total_len() + new.len(), &items)
            }
            2 | 3 => rewrite_playlists(sd.first_child()?, new, &all)?,
            4 => append_list(sd.first_child()?, b"mhla", sd.end(), &new_albums)?,
            8 => append_list(sd.first_child()?, b"mhli", sd.end(), &new_artists)?,
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

fn append_list(list: Chunk, tag: &[u8; 4], end: usize, extra: &[Vec<u8>]) -> Result<Vec<u8>> {
    list.expect(tag)?;
    let mut items = list.buf[list.off + list.header_len()..end].to_vec();
    for e in extra {
        items.extend_from_slice(e);
    }
    Ok(Builder::from_header(list).finish_list(list.total_len() + extra.len(), &items))
}

fn rewrite_playlists(mhlp: Chunk, new: &[NewTrack], all: &[&Track]) -> Result<Vec<u8>> {
    mhlp.expect(b"mhlp")?;
    let buf = mhlp.buf;
    let mut out = Vec::new();
    let mut off = mhlp.off + mhlp.header_len();
    for _ in 0..mhlp.total_len() {
        let yp = Chunk::at(buf, off)?;
        yp.expect(b"mhyp")?;
        if yp.u8(0x14) == 0 {
            out.extend_from_slice(&buf[yp.off..yp.end()]);
        } else {
            out.extend(rewrite_master(yp, new, all)?);
        }
        off = yp.end();
    }
    Ok(Builder::from_header(mhlp).finish_list(mhlp.total_len(), &out))
}

fn rewrite_master(yp: Chunk, new: &[NewTrack], all: &[&Track]) -> Result<Vec<u8>> {
    let buf = yp.buf;
    let mut kids = Vec::new();
    let mut off = yp.off + yp.header_len();
    let mut orders: HashMap<u32, Vec<(usize, String)>> = HashMap::new();
    for _ in 0..yp.u32(0x0C) {
        let od = Chunk::at(buf, off)?;
        od.expect(b"mhod")?;
        match od.u32(0x0C) {
            ty @ (52 | 53) => {
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
    kids.extend_from_slice(&buf[off..yp.end()]);
    for t in new {
        kids.extend(mhip(t.meta.id));
    }
    let mut h = Builder::from_header(yp);
    h.u32(0x10, yp.u32(0x10) + new.len() as u32);
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
}
