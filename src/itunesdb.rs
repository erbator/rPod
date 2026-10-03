//! Reader for `iPod_Control/iTunes/iTunesDB`.
//!
//! Layout (all little-endian):
//! ```text
//! mhbd                      database header
//! └─ mhsd type=1            track dataset
//! │  └─ mhlt                track list (count in 3rd word)
//! │     └─ mhit …           one per track, followed by its mhod strings
//! └─ mhsd type=2|3          playlists (3 = podcast-grouped variant)
//!    └─ mhlp                playlist list
//!       └─ mhyp …           playlist; mhod children, then mhip items
//! ```

use crate::bytes::{Chunk, mac_to_unix, utf16le};
use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Track {
    pub id: u32,
    pub dbid: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub composer: String,
    pub comment: String,
    pub kind: String,
    /// Path relative to the iPod root, with `/` separators.
    pub location: String,
    pub size: u32,
    pub length_ms: u32,
    pub track_no: u32,
    pub track_total: u32,
    pub disc_no: u32,
    pub disc_total: u32,
    pub year: u32,
    pub bitrate: u32,
    pub sample_rate: u32,
    /// 0–100, 20 per star.
    pub rating: u8,
    pub play_count: u32,
    pub skip_count: u32,
    pub compilation: bool,
    pub has_artwork: bool,
    pub date_added: Option<i64>,
    pub last_played: Option<i64>,
    pub media_type: u32,
}

impl Track {
    /// The artist an album should be filed under.
    pub fn sort_artist(&self) -> &str {
        if !self.album_artist.is_empty() {
            &self.album_artist
        } else if self.compilation {
            "Various Artists"
        } else if !self.artist.is_empty() {
            &self.artist
        } else {
            "Unknown Artist"
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Playlist {
    pub name: String,
    pub is_master: bool,
    pub is_podcast: bool,
    /// Track `id`s in playlist order.
    pub items: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct ITunesDb {
    pub version: u32,
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
}

pub fn read(path: &Path) -> Result<ITunesDb> {
    let buf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&buf)
}

pub fn parse(buf: &[u8]) -> Result<ITunesDb> {
    let mhbd = Chunk::at(buf, 0)?;
    mhbd.expect(b"mhbd")?;
    let mut db = ITunesDb {
        version: mhbd.u32(0x10),
        ..Default::default()
    };

    let mut seen_playlists = false;
    let mut sd = mhbd.first_child()?;
    for _ in 0..mhbd.u32(0x14) {
        sd.expect(b"mhsd")?;
        match sd.u32(0x0C) {
            1 => db.tracks = parse_tracks(sd.first_child()?)?,
            // Type 3 holds the same playlists with podcasts grouped; prefer type 2.
            2 => {
                db.playlists = parse_playlists(sd.first_child()?)?;
                seen_playlists = true;
            }
            3 if !seen_playlists => db.playlists = parse_playlists(sd.first_child()?)?,
            _ => {}
        }
        if sd.end() >= buf.len() {
            break;
        }
        sd = Chunk::at(buf, sd.end())?;
    }
    Ok(db)
}

fn parse_tracks(mhlt: Chunk) -> Result<Vec<Track>> {
    mhlt.expect(b"mhlt")?;
    let count = mhlt.total_len();
    let mut tracks = Vec::with_capacity(count);
    let mut off = mhlt.off + mhlt.header_len();
    for _ in 0..count {
        let it = Chunk::at(mhlt.buf, off)?;
        it.expect(b"mhit")?;
        tracks.push(parse_track(it)?);
        off = it.end();
    }
    Ok(tracks)
}

fn parse_track(it: Chunk) -> Result<Track> {
    let mut t = Track {
        id: it.u32(0x10),
        compilation: it.u8(0x1E) != 0,
        rating: it.u8(0x1F),
        size: it.u32(0x24),
        length_ms: it.u32(0x28),
        track_no: small(it.u32(0x2C)),
        track_total: small(it.u32(0x30)),
        year: it.u32(0x34),
        bitrate: it.u32(0x38),
        sample_rate: it.u32(0x3C) >> 16,
        play_count: it.u32(0x50),
        last_played: mac_to_unix(it.u32(0x58)),
        disc_no: small(it.u32(0x5C)),
        disc_total: small(it.u32(0x60)),
        date_added: mac_to_unix(it.u32(0x68)),
        dbid: it.u64(0x70),
        skip_count: it.u32(0x9C),
        has_artwork: it.u8(0xA4) == 1,
        media_type: if it.header_len() > 0xD0 { it.u32(0xD0) } else { 1 },
        ..Default::default()
    };

    let mut off = it.off + it.header_len();
    for _ in 0..it.u32(0x0C) {
        let od = Chunk::at(it.buf, off)?;
        od.expect(b"mhod")?;
        if let Some(s) = mhod_string(od) {
            match od.u32(0x0C) {
                1 => t.title = s,
                2 => t.location = s.replace(':', "/").trim_start_matches('/').to_string(),
                3 => t.album = s,
                4 => t.artist = s,
                5 => t.genre = s,
                6 => t.kind = s,
                8 => t.comment = s,
                12 => t.composer = s,
                22 => t.album_artist = s,
                _ => {}
            }
        }
        off = od.end();
    }
    Ok(t)
}

/// Some writers store 0xFFFFFFFF for "unset" track/disc numbers.
fn small(n: u32) -> u32 {
    if n > 0xFFFF { 0 } else { n }
}

/// Decode a string-bearing mhod. Non-string types (smart playlist rules,
/// playlist indices, column prefs) return `None`.
fn mhod_string(od: Chunk) -> Option<String> {
    let ty = od.u32(0x0C);
    match ty {
        // Podcast URLs: raw UTF-8 straight after the 0x18-byte header.
        15 | 16 => {
            let body = od.buf.get(od.off + 0x18..od.end())?;
            Some(String::from_utf8_lossy(body).into_owned())
        }
        1..=14 | 18..=31 | 200..=299 => {
            let len = od.u32(0x1C) as usize;
            let raw = od.slice(0x28, len)?;
            Some(match od.u32(0x18) {
                2 => String::from_utf8_lossy(raw).into_owned(),
                _ => utf16le(raw),
            })
        }
        _ => None,
    }
}

fn parse_playlists(mhlp: Chunk) -> Result<Vec<Playlist>> {
    mhlp.expect(b"mhlp")?;
    let count = mhlp.total_len();
    let mut lists = Vec::with_capacity(count);
    let mut off = mhlp.off + mhlp.header_len();
    for _ in 0..count {
        let yp = Chunk::at(mhlp.buf, off)?;
        yp.expect(b"mhyp")?;
        lists.push(parse_playlist(yp)?);
        off = yp.end();
    }
    Ok(lists)
}

fn parse_playlist(yp: Chunk) -> Result<Playlist> {
    let mut pl = Playlist {
        is_master: yp.u8(0x14) != 0,
        is_podcast: yp.u16(0x2A) != 0,
        ..Default::default()
    };
    let n_mhods = yp.u32(0x0C);
    let n_items = yp.u32(0x10);
    let mut off = yp.off + yp.header_len();
    for _ in 0..n_mhods {
        let od = Chunk::at(yp.buf, off)?;
        od.expect(b"mhod")?;
        if od.u32(0x0C) == 1 {
            pl.name = mhod_string(od).unwrap_or_default();
        }
        off = od.end();
    }
    pl.items.reserve(n_items as usize);
    for _ in 0..n_items {
        let ip = Chunk::at(yp.buf, off)?;
        ip.expect(b"mhip")?;
        pl.items.push(ip.u32(0x18));
        off = ip.end();
    }
    Ok(pl)
}
