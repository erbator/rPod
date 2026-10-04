//! Album covers from Apple's public iTunes Search API.
//!
//! Search results carry a 100×100 artwork URL; rewriting its size segment
//! gets bigger versions. (Technique from Ben Dodson's iTunes Artwork Finder.)

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
    /// Apple's collection id, for merging results from several searches.
    pub id: u64,
    pub album: String,
    pub artist: String,
    pub year: u32,
    pub tracks: u32,
    art100: String,
}

impl AlbumHit {
    /// 600×600 preview for the picker grid.
    pub fn preview_url(&self) -> String {
        self.sized(600)
    }

    /// A 1000×1000 JPEG: enough for the iPod's thumbnails and for embedding,
    /// at a fifth of the 3000 px download.
    pub fn cover_url(&self) -> String {
        self.sized(1000)
    }

    /// Apple's image server renders the size named in the URL's last segment
    /// (`…/100x100bb.jpg`), so that segment is replaced whatever its suffix.
    fn sized(&self, px: u32) -> String {
        match self.art100.rsplit_once('/') {
            Some((base, _)) => format!("{base}/{px}x{px}bb.jpg"),
            None => self.art100.clone(),
        }
    }
}

#[derive(Deserialize)]
struct Response {
    results: Vec<RawHit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawArtist {
    artist_id: Option<u64>,
    artist_name: Option<String>,
}

#[derive(Deserialize)]
struct ArtistResponse {
    results: Vec<RawArtist>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHit {
    collection_id: Option<u64>,
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
            let art100 = h.artwork_url100?;
            Some(AlbumHit {
                id: h.collection_id.unwrap_or_else(|| {
                    use std::hash::{Hash, Hasher};
                    let mut s = std::collections::hash_map::DefaultHasher::new();
                    art100.hash(&mut s);
                    s.finish()
                }),
                album: h.collection_name?,
                artist: h.artist_name.unwrap_or_default(),
                year: h.release_date.as_deref().and_then(|d| d.get(..4)?.parse().ok()).unwrap_or(0),
                tracks: h.track_count.unwrap_or(0),
                art100,
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

fn cache_path(term: &str, country: &str, by_artist: bool) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    term.to_lowercase().hash(&mut h);
    let kind = if by_artist { "discography" } else { "term" };
    Some(base.join("rpod/itunes").join(format!("{country}-{kind}-{:016x}.json", h.finish())))
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

fn cached(path: &Option<PathBuf>) -> Option<Vec<u8>> {
    let p = path.as_ref()?;
    let age = SystemTime::now().duration_since(std::fs::metadata(p).ok()?.modified().ok()?).ok()?;
    (age < CACHE_TTL).then(|| std::fs::read(p).ok()).flatten()
}

fn get(url: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
    pace();
    let mut req = agent().get(url);
    for (k, v) in query {
        req = req.query(k, v);
    }
    Ok(req.call().context("searching iTunes")?.body_mut().with_config().limit(5 << 20).read_to_vec()?)
}

/// Every album by the artist named `artist`. Album search misses some
/// releases outright (e.g. Foo Fighters' "The Colour And The Shape"), but
/// looking up the artist's id lists the whole discography.
fn discography(artist: &str, country: &str) -> Result<Vec<u8>> {
    let found = get(
        "https://itunes.apple.com/search",
        &[("term", artist), ("entity", "musicArtist"), ("country", country), ("limit", "5")],
    )?;
    let r: ArtistResponse = serde_json::from_slice(&found).context("unexpected iTunes response")?;
    // A name in another script ("陳勳奇" for Frankie Chan) can't be compared,
    // so it ranks as a borderline match. Ties keep Apple's order: two artists
    // can share a name, and the first is the well-known one.
    let mut best: Option<(u64, f32)> = None;
    for a in r.results {
        let (Some(id), Some(name)) = (a.artist_id, a.artist_name) else { continue };
        let sim = if same_script(&name, artist) { artist_similarity(&name, artist) } else { 0.5 };
        if sim >= 0.5 && best.is_none_or(|(_, b)| sim > b) {
            best = Some((id, sim));
        }
    }
    let Some((id, _)) = best else {
        return Ok(br#"{"results":[]}"#.to_vec());
    };
    let id = id.to_string();
    get(
        "https://itunes.apple.com/lookup",
        &[("id", &id), ("entity", "album"), ("country", country), ("limit", "200")],
    )
}

/// One album search (or, with `by_artist`, an artist's discography),
/// answered from the cache when possible.
fn request(term: &str, country: &str, by_artist: bool) -> Result<Vec<AlbumHit>> {
    let cache = cache_path(term, country, by_artist);
    if let Some(hits) = cached(&cache).and_then(|b| parse(&b).ok()) {
        return Ok(hits);
    }
    let bytes = if by_artist {
        discography(term, country)?
    } else {
        get(
            "https://itunes.apple.com/search",
            &[("term", term), ("entity", "album"), ("country", country), ("limit", "25")],
        )?
    };
    let hits = parse(&bytes)?;
    if let Some(p) = cache {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, &bytes);
    }
    Ok(hits)
}

/// A free-text album search (what the user types in the picker).
pub fn search(term: &str, country: &str) -> Result<Vec<AlbumHit>> {
    request(term, country, false)
}

/// What we're looking for: the album as it's tagged on the iPod.
#[derive(Clone, Debug, Default)]
pub struct Wanted {
    pub artist: String,
    pub album: String,
    pub year: u32,
    pub tracks: usize,
}

impl Wanted {
    pub fn from_tracks(tracks: &[crate::itunesdb::Track]) -> Self {
        let first = tracks.first().cloned().unwrap_or_default();
        let artist = if first.album_artist.is_empty() { first.artist } else { first.album_artist };
        Self { artist, album: first.album, year: tracks.iter().map(|t| t.year).max().unwrap_or(0), tracks: tracks.len() }
    }

    /// The searches to try, most specific first. Long tags make bad search
    /// terms (Apple returns unrelated popular albums when too many words
    /// don't match), so they're trimmed to the main artist and the album's
    /// core words. The last resort lists the artist's whole discography,
    /// which finds albums Apple titles differently (e.g. in another language)
    /// or leaves out of album search entirely.
    fn searches(&self) -> Vec<(String, bool)> {
        let artist = primary_artist(&self.artist);
        let album = query_words(&self.album);
        let mut out: Vec<(String, bool)> = Vec::new();
        let mut push = |term: String, by_artist: bool| {
            let term = term.trim().to_string();
            if !term.is_empty() && !out.iter().any(|(t, b)| t.eq_ignore_ascii_case(&term) && *b == by_artist) {
                out.push((term, by_artist));
            }
        };
        push(format!("{artist} {album}"), false);
        if album.split_whitespace().count() >= 2 {
            push(album.clone(), false);
        }
        let generic = ["various artists", "unknown artist", "soundtrack", "va"];
        if !artist.is_empty() && !generic.contains(&artist.to_lowercase().as_str()) {
            push(artist, true);
        }
        out
    }
}

/// Run the searches for `w` in order, merging results. After each search
/// `progress` gets everything found so far; returning true stops early.
pub fn find(w: &Wanted, country: &str, mut progress: impl FnMut(&[AlbumHit]) -> bool) -> Result<Vec<AlbumHit>> {
    let mut all: Vec<AlbumHit> = Vec::new();
    let mut last_err = None;
    let mut any_ok = false;
    for (term, by_artist) in w.searches() {
        match request(&term, country, by_artist) {
            Ok(hits) => {
                any_ok = true;
                for h in hits {
                    if !all.iter().any(|x| x.id == h.id) {
                        all.push(h);
                    }
                }
                if progress(&all) {
                    break;
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    match (any_ok, last_err) {
        (false, Some(e)) => Err(e),
        _ => Ok(all),
    }
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

/// The cover at 1000 px, falling back to the 600 px preview.
pub fn fetch_cover(hit: &AlbumHit) -> Result<Vec<u8>> {
    download(&hit.cover_url()).or_else(|_| download(&hit.preview_url()))
}

// ---------------------------------------------------------------- matching

/// Separators between artists in a combined artist string.
const ARTIST_SEPARATORS: &[&str] =
    &[";", "/", ",", " & ", " feat. ", " feat ", " ft. ", " ft ", " featuring ", " x ", " with ", " and ", " vs. ", " vs "];

fn split_artists(s: &str) -> Vec<String> {
    let mut parts = vec![s.to_lowercase()];
    for sep in ARTIST_SEPARATORS {
        parts = parts.iter().flat_map(|p| p.split(sep).map(str::to_string).collect::<Vec<_>>()).collect();
    }
    parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// The first artist of a combined credit, as typed (for search terms).
fn primary_artist(s: &str) -> String {
    let lower = s.to_lowercase();
    let cut = ARTIST_SEPARATORS.iter().filter_map(|sep| lower.find(sep)).min().unwrap_or(s.len());
    s[..cut].trim().to_string()
}

/// An album title trimmed to words worth searching for: no bracketed
/// extras, edition noise, disc numbers or punctuation.
fn query_words(album: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in album.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            _ if depth > 0 => {}
            c if c.is_alphanumeric() || c == '\'' => out.push(c),
            _ => out.push(' '),
        }
    }
    let noise = [
        "deluxe", "edition", "remastered", "remaster", "expanded", "anniversary", "bonus", "ost", "soundtrack",
        "disc", "cd", "ep", "single",
    ];
    let words: Vec<&str> = out.split_whitespace().filter(|w| !noise.contains(&w.to_lowercase().as_str())).collect();
    // "Disc 2" leaves a lone digit behind.
    let words: Vec<&str> = words.into_iter().filter(|w| !(w.len() == 1 && w.chars().all(|c| c.is_ascii_digit()))).collect();
    if words.is_empty() { album.trim().to_string() } else { words.join(" ") }
}

/// Lowercase letters and digits only, minus edition noise words. Bracketed
/// text is kept: "(Blue Album)" vs "(Green Album)" is the whole difference.
fn normalize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_lowercase().next().unwrap_or(c) } else { ' ' })
        .collect();
    let noise = [
        "deluxe", "edition", "remastered", "remaster", "expanded", "version", "anniversary", "bonus", "track",
        "tracks", "explicit", "clean", "ep", "single", "the", "ost", "original", "motion", "picture", "soundtrack",
    ];
    let is_year = |w: &str| w.len() == 4 && (w.starts_with("19") || w.starts_with("20")) && w.chars().all(|c| c.is_ascii_digit());
    out.split_whitespace().filter(|w| !noise.contains(w) && !is_year(w)).collect::<Vec<_>>().join(" ")
}

/// 0..1 similarity from edit distance of the normalized strings.
fn edit_similarity(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
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

/// Shared-word similarity (Dice coefficient). Long titles that word things
/// differently ("OST Departure" vs "Music Record Departure") score fairly here
/// where letter-by-letter comparison falls apart.
fn word_similarity(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split_whitespace().collect(), b.split_whitespace().collect());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|w| b.contains(w)).count();
    2.0 * shared as f32 / (a.len() + b.len()) as f32
}

fn similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (normalize(a), normalize(b));
    edit_similarity(&a, &b).max(word_similarity(&a, &b))
}

/// Best match between any artist of one credit and any of the other, so
/// "Nujabes / Fat Jon the Ample Soul Physician" matches "Nujabes/fat jon".
fn artist_similarity(a: &str, b: &str) -> f32 {
    let whole = similarity(a, b);
    let (pa, pb) = (split_artists(a), split_artists(b));
    pa.iter().flat_map(|x| pb.iter().map(move |y| similarity(x, y))).fold(whole, f32::max)
}

/// Titles in different scripts ("Fallen Angels" vs "墮落天使") can't be
/// compared by spelling at all.
fn same_script(a: &str, b: &str) -> bool {
    let latin = |s: &str| normalize(s).chars().any(|c| c.is_ascii_alphanumeric());
    latin(a) == latin(b)
}

fn generic_artist(a: &str) -> bool {
    let a = a.trim().to_lowercase();
    a.is_empty() || ["various artists", "various", "va", "unknown artist", "soundtrack"].contains(&a.as_str())
}

/// How well a result matches the album we have (0..1).
pub fn score(hit: &AlbumHit, w: &Wanted) -> f32 {
    let tracks = if hit.tracks == 0 || w.tracks == 0 {
        0.5
    } else {
        let (a, b) = (hit.tracks as f32, w.tracks as f32);
        a.min(b) / a.max(b)
    };
    let year = match (hit.year, w.year) {
        (0, _) | (_, 0) => 0.5,
        (a, b) => match a.abs_diff(b) {
            0 => 1.0,
            1 => 0.8,
            2..=3 => 0.4,
            _ => 0.0,
        },
    };
    let album = if same_script(&hit.album, &w.album) { similarity(&hit.album, &w.album) } else { 0.5 };
    let artist = artist_similarity(&hit.artist, &w.artist);
    // The right title by a clearly different artist is a different album
    // (a band called "Fallen Angels" is not the Fallen Angels soundtrack).
    let gate = if generic_artist(&w.artist) || generic_artist(&hit.artist) { 1.0 } else { (0.4 + artist).min(1.0) };
    (0.5 * album + 0.3 * artist + 0.1 * tracks + 0.1 * year) * gate
}

/// Index of the best result, its score, and whether it's confident.
/// Whether a result could be the album at all. Apple pads searches with
/// albums by other artists (same-name releases, AI-generated filler) and with
/// singles; those are hidden rather than just ranked low.
pub fn plausible(hit: &AlbumHit, w: &Wanted) -> bool {
    // A one- or two-track release isn't a full album's cover.
    if w.tracks >= 4 && hit.tracks > 0 && (hit.tracks as usize) * 4 <= w.tracks {
        return false;
    }
    if generic_artist(&w.artist) {
        return !same_script(&hit.album, &w.album) || similarity(&hit.album, &w.album) >= 0.4;
    }
    // A name in another script can't be compared, so it gets the benefit of the doubt.
    !same_script(&hit.artist, &w.artist) || artist_similarity(&hit.artist, &w.artist) >= 0.5
}

/// Index of the best plausible result, its score, and whether it's confident.
pub fn best_match(hits: &[AlbumHit], w: &Wanted) -> Option<(usize, f32, bool)> {
    let mut scored: Vec<(usize, f32)> =
        hits.iter().enumerate().filter(|(_, h)| plausible(h, w)).map(|(i, h)| (i, score(h, w))).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (best, top) = *scored.first()?;
    let runner_up = scored.get(1).map_or(0.0, |s| s.1);
    Some((best, top, top >= CONFIDENT && top - runner_up >= MARGIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"resultCount":2,"results":[
      {"wrapperType":"collection","collectionType":"Album","collectionId":1,"artistName":"Weezer",
       "collectionName":"Weezer (Blue Album)","trackCount":10,"releaseDate":"1994-05-10T07:00:00Z",
       "artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/100x100bb.jpg"},
      {"wrapperType":"collection","collectionId":2,"artistName":"Weezer","collectionName":"Pinkerton (Deluxe Edition)",
       "trackCount":35,"releaseDate":"1996-09-24T07:00:00Z",
       "artworkUrl100":"https://is3-ssl.mzstatic.com/image/thumb/Music/y.jpg/100x100bb.jpg"}]}"#;

    fn hit(artist: &str, album: &str, year: u32, tracks: u32) -> AlbumHit {
        AlbumHit { id: 0, album: album.into(), artist: artist.into(), year, tracks, art100: String::new() }
    }

    fn want(artist: &str, album: &str, year: u32, tracks: usize) -> Wanted {
        Wanted { artist: artist.into(), album: album.into(), year, tracks }
    }

    #[test]
    fn parses_and_rewrites_urls() {
        let hits = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].id, hits[0].year, hits[0].tracks), (1, 1994, 10));
        assert_eq!(
            hits[0].preview_url(),
            "https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/600x600bb.jpg"
        );
        assert_eq!(
            hits[0].cover_url(),
            "https://is1-ssl.mzstatic.com/image/thumb/Music125/v4/aa/bb/cc/x.jpg/1000x1000bb.jpg"
        );
        // Other size suffixes still become the full-size cover, not the 100 px original.
        let mut odd = hits[0].clone();
        odd.art100 = "https://is1-ssl.mzstatic.com/image/thumb/Music/x.jpg/100x100-75.jpg".into();
        assert_eq!(odd.cover_url(), "https://is1-ssl.mzstatic.com/image/thumb/Music/x.jpg/1000x1000bb.jpg");
    }

    #[test]
    fn exact_album_is_confident() {
        let w = want("Weezer", "Weezer (Blue Album)", 1994, 10);
        let hits = [hit("Weezer", "Weezer (Blue Album)", 1994, 10), hit("Weezer", "Weezer (Green Album)", 2001, 10)];
        let (i, _, confident) = best_match(&hits, &w).unwrap();
        assert!(i == 0 && confident);
    }

    #[test]
    fn near_identical_candidates_are_not_confident() {
        // Apple has no "Blue Album" by that name; Green and Teal tie.
        let w = want("Weezer", "Weezer (Blue Album)", 0, 10);
        let hits = [hit("Weezer", "Weezer (Green Album)", 2001, 10), hit("Weezer", "Weezer (Teal Album)", 2019, 10)];
        assert!(!best_match(&hits, &w).unwrap().2);
    }

    #[test]
    fn long_titles_and_combined_artists_match() {
        // Real case: tagged "OST Departure", Apple says "Music Record Departure".
        let w = want("Nujabes / Fat Jon the Ample Soul Physician", "Samurai Champloo OST Departure", 2004, 14);
        let hits = [
            hit("Nujabes/fat jon", "Samurai Champloo Music Record Departure", 2004, 14),
            hit("Nujabes", "Luv(sic) Hexalogy", 2015, 6),
            hit("Geek Music", "Samurai Champloo - Battle Cry - Main Theme - Single", 2016, 1),
        ];
        let (i, top, confident) = best_match(&hits, &w).unwrap();
        assert_eq!(i, 0);
        assert!(top > 0.8 && confident, "{top}");
    }

    #[test]
    fn translated_title_found_by_artist_tracks_and_year() {
        // Real case: Apple lists Fallen Angels under its Chinese title.
        let w = want("Frankie Chan & Roel A. Garcia", "Fallen Angels (OST)", 0, 19);
        let hits = [
            hit("Frankie Chan, Roel A. Garcia & 杜可風", "東邪西毒 (電影原聲帶)", 1994, 15),
            hit("Roel A. Gracia & Frankie Chan", "墮落天使(電影原聲大碟)", 2016, 18),
            hit("Frankie Jordan", "Tu Parles Trop - Single", 1961, 3),
            // Same title, wrong artist: the band "Fallen Angels".
            hit("Fallen Angels", "Fallen Angels", 1984, 16),
            hit("Bob Dylan", "Fallen Angels", 2016, 12),
        ];
        let (i, _, confident) = best_match(&hits, &w).unwrap();
        assert_eq!(i, 1, "artist + 18 tracks beats the same-title albums by other artists");
        assert!(!confident, "a title match is missing, so a human confirms");
    }

    #[test]
    fn unrelated_artists_and_singles_are_not_plausible() {
        let w = want("Frankie Chan & Roel A. Garcia", "Fallen Angels (OST)", 0, 19);
        assert!(plausible(&hit("Roel A. Gracia & Frankie Chan", "墮落天使(電影原聲大碟)", 2016, 18), &w));
        assert!(!plausible(&hit("Fallen Angels", "Fallen Angels", 1984, 16), &w));
        assert!(!plausible(&hit("Lofi Dreams Collective", "Fallen Angels (Chill Beats)", 2024, 19), &w));
        assert!(!plausible(&hit("Frankie Chan", "Fallen Angels - Single", 2020, 1), &w), "single for an album");
        assert!(plausible(&hit("坂本龍一", "Merry Christmas Mr. Lawrence", 1983, 19), &want("Ryuichi Sakamoto", "Merry Christmas Mr. Lawrence", 1983, 19)), "other script: unknown");
        let various = want("Various Artists", "Pulp Fiction", 1994, 16);
        assert!(plausible(&hit("Various Artists", "Pulp Fiction (Music from the Motion Picture)", 1994, 16), &various));
        assert!(!plausible(&hit("Various Artists", "Now That's What I Call Music 42", 1999, 16), &various));
        // Only plausible results can be the best match.
        let hits = [hit("Fallen Angels", "Fallen Angels", 1984, 16), hit("Bob Dylan", "Fallen Angels", 2016, 12)];
        assert!(best_match(&hits, &w).is_none());
    }

    #[test]
    fn compilations_are_not_penalized_for_artist() {
        let w = want("Various Artists", "Pulp Fiction (Music from the Motion Picture)", 1994, 16);
        let hits = [hit("Various Artists", "Pulp Fiction (Music from the Motion Picture)", 1994, 16)];
        assert!(best_match(&hits, &w).unwrap().2);
    }

    #[test]
    fn year_breaks_ties_between_same_artist_albums() {
        let w = want("My Bloody Valentine", "Loveless", 1991, 11);
        let hits = [hit("my bloody valentine", "m b v", 2013, 9), hit("my bloody valentine", "loveless", 1991, 11)];
        let (i, _, confident) = best_match(&hits, &w).unwrap();
        assert!(i == 1 && confident);
    }

    #[test]
    fn search_terms_are_trimmed() {
        let w = want("Nujabes / Fat Jon the Ample Soul Physician", "Samurai Champloo OST Departure", 2004, 14);
        let s = w.searches();
        assert_eq!(s[0], ("Nujabes Samurai Champloo Departure".into(), false));
        assert_eq!(s[1], ("Samurai Champloo Departure".into(), false));
        assert_eq!(s[2], ("Nujabes".into(), true));
        assert_eq!(query_words("Clubber's Guide to... 2001 (Disc 2)"), "Clubber's Guide to 2001");
        assert_eq!(primary_artist("Dean Blunt & Elias Rønnenfelt"), "Dean Blunt");
        let various = want("Various Artists", "Now 42", 0, 20).searches();
        assert!(various.iter().all(|(_, by_artist)| !by_artist));
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

    /// Hits the real API: `cargo test -- --ignored itunes_live --nocapture`
    #[test]
    #[ignore]
    fn itunes_live() {
        for w in [
            want("A Tribe Called Quest", "The Low End Theory", 1991, 14),
            want("Nujabes / Fat Jon the Ample Soul Physician", "Samurai Champloo OST Departure", 2004, 14),
            want("My Bloody Valentine", "Loveless", 1991, 11),
            want("Foo Fighters", "The Colour and the Shape", 1997, 13),
        ] {
            let hits = find(&w, "hu", |_| false).unwrap();
            let (i, top, confident) = best_match(&hits, &w).unwrap();
            println!("{} → {} — {} ({}, {}t) score {top:.2} confident {confident}", w.album, hits[i].artist, hits[i].album, hits[i].year, hits[i].tracks);
        }
    }
}
