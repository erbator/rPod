//! Album covers from Apple's public iTunes Search API.
//!
//! Search results carry a 100×100 artwork URL; rewriting its size segment
//! gets bigger versions, and another rewrite gets the label's original
//! upload. (Technique from Ben Dodson's iTunes Artwork Finder.)

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Apple allows roughly 20 searches a minute per IP.
const MIN_INTERVAL: Duration = Duration::from_secs(3);
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// A best match is "confident" (safe to accept in bulk) when it scores at
/// least this and beats the runner-up by `MARGIN`.
pub const CONFIDENT: f32 = 0.85;
const MARGIN: f32 = 0.05;

#[derive(Debug, Clone)]
pub struct AlbumHit {
    pub album: String,
    pub artist: String,
    pub year: u32,
    pub tracks: u32,
    art100: String,
}

impl AlbumHit {
    /// 600×600 preview for the picker grid.
    pub fn preview_url(&self) -> String {
        self.art100.replace("100x100", "600x600")
    }

    /// A 3000×3000 JPEG (or the largest size below that Apple has).
    pub fn hires_url(&self) -> String {
        self.art100.replace("100x100bb", "3000x3000bb")
    }

    /// The label's original upload, uncompressed by Apple's resizer.
    pub fn original_url(&self) -> Option<String> {
        let (_, rest) = self.art100.split_once("/image/thumb/")?;
        let (path, _size) = rest.rsplit_once('/')?;
        Some(format!("https://a5.mzstatic.com/us/r1000/0/{path}"))
    }
}

#[derive(Deserialize)]
struct Response {
    results: Vec<RawHit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHit {
    collection_name: Option<String>,
    artist_name: Option<String>,
    artwork_url100: Option<String>,
    track_count: Option<u32>,
    release_date: Option<String>,
}

fn parse(json: &[u8]) -> Result<Vec<AlbumHit>> {
    let r: Response = serde_json::from_slice(json).context("unexpected iTunes response")?;
    Ok(r.results
        .into_iter()
        .filter_map(|h| {
            Some(AlbumHit {
                album: h.collection_name?,
                artist: h.artist_name.unwrap_or_default(),
                year: h.release_date.as_deref().and_then(|d| d.get(..4)?.parse().ok()).unwrap_or(0),
                tracks: h.track_count.unwrap_or(0),
                art100: h.artwork_url100?,
            })
        })
        .collect())
}

/// Two-letter store country from the system locale ("hu_HU.UTF-8" → "hu").
pub fn default_country() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty() && v != "C" && v != "POSIX")
        .and_then(|v| v.split(['.', '@']).next()?.split('_').nth(1).map(str::to_lowercase))
        .filter(|cc| cc.len() == 2)
        .unwrap_or_else(|| "us".into())
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build())
}

fn cache_path(term: &str, country: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    term.to_lowercase().hash(&mut h);
    Some(base.join("rpod/itunes").join(format!("{country}-{:016x}.json", h.finish())))
}

/// Wait so searches stay under Apple's rate limit. Returns once a slot is free.
fn pace() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if let Some(t) = *last {
        let wait = MIN_INTERVAL.saturating_sub(t.elapsed());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
    *last = Some(Instant::now());
}

/// Whether `search` would be answered from the cache (no network, no wait).
pub fn is_cached(term: &str, country: &str) -> bool {
    cache_path(term, country)
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < CACHE_TTL)
}

pub fn search(term: &str, country: &str) -> Result<Vec<AlbumHit>> {
    let cache = cache_path(term, country);
    if is_cached(term, country) {
        if let Some(Ok(bytes)) = cache.as_ref().map(std::fs::read) {
            if let Ok(hits) = parse(&bytes) {
                return Ok(hits);
            }
        }
    }
    pace();
    let bytes = agent()
        .get("https://itunes.apple.com/search")
        .query("term", term)
        .query("entity", "album")
        .query("country", country)
        .query("limit", "25")
        .call()
        .context("searching iTunes")?
        .body_mut()
        .with_config()
        .limit(5 << 20)
        .read_to_vec()?;
    let hits = parse(&bytes)?;
    if let Some(p) = cache {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, &bytes);
    }
    Ok(hits)
}

pub fn download(url: &str) -> Result<Vec<u8>> {
    let bytes = agent()
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?
        .body_mut()
        .with_config()
        .limit(40 << 20)
        .read_to_vec()?;
    if image::guess_format(&bytes).is_err() {
        bail!("not an image");
    }
    Ok(bytes)
}

/// The biggest cover available: 3000 px, else the original, else 600 px.
pub fn fetch_cover(hit: &AlbumHit) -> Result<Vec<u8>> {
    download(&hit.hires_url())
        .or_else(|e| hit.original_url().map_or(Err(e), |u| download(&u)))
        .or_else(|_| download(&hit.preview_url()))
}

// ---------------------------------------------------------------- matching

/// Lowercase letters and digits only, minus edition noise words. Bracketed
/// text is kept: "(Blue Album)" vs "(Green Album)" is the whole difference.
fn normalize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect();
    let noise = [
        "deluxe", "edition", "remastered", "remaster", "expanded", "version", "anniversary", "bonus", "track",
        "tracks", "explicit", "clean", "ep", "single", "the",
    ];
    let is_year = |w: &str| w.len() == 4 && (w.starts_with("19") || w.starts_with("20")) && w.chars().all(|c| c.is_ascii_digit());
    out.split_whitespace().filter(|w| !noise.contains(w) && !is_year(w)).collect::<Vec<_>>().join(" ")
}

/// 0..1 similarity from edit distance.
fn similarity(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (normalize(a).chars().collect(), normalize(b).chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + (ca != cb) as usize).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f32 / a.len().max(b.len()) as f32
}

/// How well a result matches an album we have (0..1).
pub fn score(hit: &AlbumHit, artist: &str, album: &str, track_count: usize) -> f32 {
    let tracks = if hit.tracks == 0 || track_count == 0 {
        0.5
    } else {
        let (a, b) = (hit.tracks as f32, track_count as f32);
        a.min(b) / a.max(b)
    };
    0.55 * similarity(&hit.album, album) + 0.35 * similarity(&hit.artist, artist) + 0.10 * tracks
}

/// Index of the best result, its score, and whether it's confident.
pub fn best_match(hits: &[AlbumHit], artist: &str, album: &str, track_count: usize) -> Option<(usize, f32, bool)> {
    let mut scored: Vec<(usize, f32)> =
        hits.iter().enumerate().map(|(i, h)| (i, score(h, artist, album, track_count))).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (best, top) = *scored.first()?;
    let runner_up = scored.get(1).map_or(0.0, |s| s.1);
    Some((best, top, top >= CONFIDENT && top - runner_up >= MARGIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"resultCount":2,"results":[
      {"wrapperType":"collection","collectionType":"Album","artistName":"Weezer",
       "collectionName":"Weezer (Blue Album)","trackCount":10,"releaseDate":"1994-05-10T07:00:00Z",
       "artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/100x100bb.jpg"},
      {"wrapperType":"collection","artistName":"Weezer","collectionName":"Pinkerton (Deluxe Edition)",
       "trackCount":35,"releaseDate":"1996-09-24T07:00:00Z",
       "artworkUrl100":"https://is3-ssl.mzstatic.com/image/thumb/Music/y.jpg/100x100bb.jpg"}]}"#;

    #[test]
    fn parses_and_rewrites_urls() {
        let hits = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].year, hits[0].tracks), (1994, 10));
        assert_eq!(
            hits[0].preview_url(),
            "https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/600x600bb.jpg"
        );
        assert_eq!(
            hits[0].hires_url(),
            "https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/3000x3000bb.jpg"
        );
        assert_eq!(
            hits[0].original_url().unwrap(),
            "https://a5.mzstatic.com/us/r1000/0/Music125/v4/aa/bb/cc/x.jpg"
        );
    }

    #[test]
    fn scores_the_right_album_highest() {
        let hits = parse(SAMPLE.as_bytes()).unwrap();
        let blue = score(&hits[0], "Weezer", "Weezer (Blue Album)", 10);
        let pink = score(&hits[1], "Weezer", "Weezer (Blue Album)", 10);
        assert!(blue >= CONFIDENT, "{blue}");
        assert!(pink < blue);
        assert!(score(&hits[1], "Weezer", "Pinkerton", 35) > CONFIDENT, "edition noise ignored");
        let green = AlbumHit { album: "Weezer (Green Album)".into(), ..hits[0].clone() };
        assert!(score(&green, "Weezer", "Weezer (Blue Album)", 10) < blue, "bracket text still counts");

        let remaster = AlbumHit { album: "Weezer (2024 Remaster)".into(), ..hits[0].clone() };
        assert!(score(&remaster, "Weezer", "Weezer", 10) > 0.95, "years and remaster noise ignored");

        // Two near-identical candidates: best is found, but not confident.
        let teal = AlbumHit { album: "Weezer (Teal Album)".into(), ..hits[0].clone() };
        let (_, _, confident) = best_match(&[green, teal], "Weezer", "Weezer (Blue Album)", 10).unwrap();
        assert!(!confident);
        let (i, _, confident) = best_match(&hits, "Weezer", "Weezer (Blue Album)", 10).unwrap();
        assert!(i == 0 && confident);
    }

    #[test]
    fn country_from_locale() {
        // SAFETY: single-threaded within this test; only this test reads these.
        unsafe {
            std::env::remove_var("LC_ALL");
            std::env::remove_var("LC_MESSAGES");
            std::env::set_var("LANG", "hu_HU.UTF-8");
        }
        assert_eq!(default_country(), "hu");
    }

    /// Hits the real API: `cargo test -- --ignored itunes_live`
    #[test]
    #[ignore]
    fn itunes_live() {
        let hits = search("weezer blue album", "us").unwrap();
        for h in &hits {
            println!("  {:.3}  {} — {} ({}, {} tracks)", score(h, "Weezer", "Weezer (Blue Album)", 10), h.artist, h.album, h.year, h.tracks);
        }
        let (i, top, confident) = best_match(&hits, "Weezer", "Weezer (Blue Album)", 10).unwrap();
        let best = &hits[i];
        println!("score {top:.3}, confident: {confident}");
        println!("best: {} — {} ({}), {}", best.artist, best.album, best.year, best.hires_url());
        let bytes = fetch_cover(best).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        println!("cover: {}×{}, {} KB", img.width(), img.height(), bytes.len() / 1024);
        assert!(img.width() >= 600);
    }
}
