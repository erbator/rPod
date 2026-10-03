//! Browse indexes (artists → albums → tracks) built over a flat track list.

use crate::itunesdb::Track;
use std::collections::HashMap;

pub struct Album {
    pub title: String,
    pub artist: String,
    pub year: u32,
    /// Indices into the track list, in disc/track order.
    pub tracks: Vec<usize>,
}

pub struct Artist {
    pub name: String,
    pub albums: Vec<usize>,
    pub track_count: usize,
}

pub struct Index {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    /// All tracks ordered by artist, album, disc, track.
    pub songs: Vec<usize>,
    /// Track id → index, for resolving playlist items.
    pub by_id: HashMap<u32, usize>,
    /// Track index → album index.
    pub album_of: Vec<usize>,
}

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

impl Index {
    pub fn build(tracks: &[Track]) -> Self {
        let mut album_of: HashMap<(String, String), usize> = HashMap::new();
        let mut albums: Vec<Album> = Vec::new();
        for (i, t) in tracks.iter().enumerate() {
            let key = (fold(t.sort_artist()), fold(&t.album));
            let a = *album_of.entry(key).or_insert_with(|| {
                albums.push(Album {
                    title: if t.album.is_empty() { "Unknown Album".into() } else { t.album.clone() },
                    artist: t.sort_artist().to_string(),
                    year: 0,
                    tracks: Vec::new(),
                });
                albums.len() - 1
            });
            albums[a].year = albums[a].year.max(t.year);
            albums[a].tracks.push(i);
        }
        for a in &mut albums {
            a.tracks.sort_by_key(|&i| {
                let t = &tracks[i];
                (t.disc_no, t.track_no, fold(&t.title))
            });
        }
        albums.sort_by_cached_key(|a| (fold(&a.artist), a.year, fold(&a.title)));

        let mut artists: Vec<Artist> = Vec::new();
        for (ai, a) in albums.iter().enumerate() {
            match artists.last_mut() {
                Some(last) if fold(&last.name) == fold(&a.artist) => {
                    last.albums.push(ai);
                    last.track_count += a.tracks.len();
                }
                _ => artists.push(Artist {
                    name: a.artist.clone(),
                    albums: vec![ai],
                    track_count: a.tracks.len(),
                }),
            }
        }

        let mut album_of = vec![0; tracks.len()];
        for (ai, a) in albums.iter().enumerate() {
            for &t in &a.tracks {
                album_of[t] = ai;
            }
        }
        let songs = albums.iter().flat_map(|a| a.tracks.iter().copied()).collect();
        let by_id = tracks.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        Self { artists, albums, songs, by_id, album_of }
    }
}
