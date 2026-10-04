//! Playing songs straight off the iPod through the PC's speakers.
//!
//! rodio plays the current song with the next one already queued behind it,
//! so albums run on without a gap; `tick` notices when rodio has moved on and
//! queues the one after.

use crate::itunesdb::Track;
use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}

/// "Previous" restarts the song instead once it has played this long.
const RESTART_AFTER: Duration = Duration::from_secs(3);

pub struct Player {
    root: PathBuf,
    /// Opened on first play, so browsing never touches the sound system.
    device: Option<(MixerDeviceSink, rodio::Player)>,
    queue: Vec<Track>,
    /// Play order as indices into `queue`: shuffled or straight.
    order: Vec<usize>,
    /// Position in `order` of the song playing now.
    at: usize,
    /// Position in `order` of the song queued behind it, if any.
    next: Option<usize>,
    active: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// 0–100.
    pub volume: u8,
    shown_secs: u64,
    /// Set when a song can't be played; the app shows it once.
    pub error: Option<String>,
}

impl Player {
    pub fn new(root: PathBuf, volume: u8) -> Self {
        Self {
            root,
            device: None,
            queue: Vec::new(),
            order: Vec::new(),
            at: 0,
            next: None,
            active: false,
            shuffle: false,
            repeat: Repeat::Off,
            volume: volume.min(100),
            shown_secs: 0,
            error: None,
        }
    }

    /// The song playing (or paused) now, how far in, and whether paused.
    pub fn now(&self) -> Option<(&Track, Duration, bool)> {
        let (_, out) = self.device.as_ref().filter(|_| self.active)?;
        Some((&self.queue[self.order[self.at]], out.get_pos(), out.is_paused()))
    }

    pub fn playing_dbid(&self) -> Option<u64> {
        self.now().map(|(t, _, _)| t.dbid)
    }

    /// Play `queue` from `start`, replacing whatever was playing.
    pub fn play(&mut self, queue: Vec<Track>, start: usize) {
        if queue.is_empty() {
            return;
        }
        let start = start.min(queue.len() - 1);
        self.queue = queue;
        self.order = (0..self.queue.len()).collect();
        self.at = start;
        if self.shuffle {
            self.reshuffle();
        }
        self.start_at(self.at);
    }

    pub fn toggle_pause(&mut self) {
        match &self.device {
            Some((_, out)) if self.active => {
                if out.is_paused() { out.play() } else { out.pause() }
            }
            _ if !self.queue.is_empty() => self.start_at(self.at),
            _ => {}
        }
    }

    pub fn next(&mut self) {
        if !self.active {
            return;
        }
        // Skipping moves on even when repeating one song.
        match self.after(self.at, false) {
            Some(n) => self.start_at(n),
            None => self.stop(),
        }
    }

    pub fn prev(&mut self) {
        let Some((_, pos, _)) = self.now() else { return };
        if pos >= RESTART_AFTER || (self.at == 0 && self.repeat != Repeat::All) {
            self.seek_to(Duration::ZERO);
        } else {
            self.start_at(if self.at == 0 { self.order.len() - 1 } else { self.at - 1 });
        }
    }

    /// Jump `secs` forward or back in the current song.
    pub fn seek(&mut self, secs: i64) {
        let Some((t, pos, _)) = self.now() else { return };
        let length = Duration::from_millis(t.length_ms as u64);
        let target = if secs < 0 { pos.saturating_sub(Duration::from_secs(secs.unsigned_abs())) } else { pos + Duration::from_secs(secs as u64) };
        if length > Duration::ZERO && target >= length {
            self.next();
        } else {
            self.seek_to(target);
        }
    }

    fn seek_to(&mut self, pos: Duration) {
        if let Some((_, out)) = &self.device {
            let _ = out.try_seek(pos);
            self.shown_secs = u64::MAX;
        }
    }

    pub fn change_volume(&mut self, delta: i8) {
        self.volume = (self.volume as i16 + delta as i16).clamp(0, 100) as u8;
        if let Some((_, out)) = &self.device {
            out.set_volume(self.volume as f32 / 100.0);
        }
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.queue.is_empty() {
            return;
        }
        if self.shuffle {
            self.reshuffle();
        } else {
            self.at = self.order[self.at];
            self.order = (0..self.queue.len()).collect();
        }
        self.requeue();
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        self.requeue();
    }

    /// Stop and let go of the sound device and the open song file (so the
    /// iPod can be ejected).
    pub fn stop(&mut self) {
        self.device = None;
        self.next = None;
        self.active = false;
    }

    /// Follow rodio along the queue. Returns true when the display should change.
    pub fn tick(&mut self) -> bool {
        let Some((_, out)) = &self.device else { return false };
        if !self.active {
            return false;
        }
        let left = out.len();
        let mut changed = false;
        if left < 1 + self.next.is_some() as usize {
            changed = true;
            match self.next.take() {
                // The current song ended and the queued one took over.
                Some(n) if left > 0 => {
                    self.at = n;
                    self.queue_next();
                }
                // Nothing playable was queued: carry on from the following song.
                _ => match self.after(self.at, true) {
                    Some(n) => self.start_at(n),
                    None => self.active = false,
                },
            }
        }
        let secs = self.now().map_or(0, |(_, pos, _)| pos.as_secs());
        if secs != self.shown_secs {
            self.shown_secs = secs;
            changed = true;
        }
        changed
    }

    /// The position in `order` after `at`, honouring repeat. `natural` is
    /// false for an explicit skip, which leaves a repeated song.
    fn after(&self, at: usize, natural: bool) -> Option<usize> {
        match self.repeat {
            Repeat::One if natural => Some(at),
            _ if at + 1 < self.order.len() => Some(at + 1),
            Repeat::Off => None,
            _ => Some(0),
        }
    }

    /// Current song first, the rest in random order.
    fn reshuffle(&mut self) {
        let current = self.order[self.at];
        let mut rest: Vec<usize> = (0..self.queue.len()).filter(|&i| i != current).collect();
        for i in (1..rest.len()).rev() {
            rest.swap(i, (crate::dbwrite::rand_u64() % (i as u64 + 1)) as usize);
        }
        self.order = std::iter::once(current).chain(rest).collect();
        self.at = 0;
    }

    /// Start playing `order[at]`, skipping songs that can't be opened.
    fn start_at(&mut self, at: usize) {
        if self.device.is_none() {
            match open_device() {
                Ok(d) => self.device = Some(d),
                Err(e) => {
                    self.error = Some(format!("No sound output: {e:#}"));
                    return;
                }
            }
        }
        let (_, out) = self.device.as_ref().unwrap();
        out.clear();
        out.set_volume(self.volume as f32 / 100.0);
        self.next = None;
        let mut at = at;
        for _ in 0..self.order.len() {
            match self.load(at) {
                Ok(()) => {
                    self.at = at;
                    self.active = true;
                    self.shown_secs = u64::MAX;
                    self.device.as_ref().unwrap().1.play();
                    self.queue_next();
                    return;
                }
                Err(e) => {
                    self.error = Some(format!("Can't play {}: {e:#}", self.queue[self.order[at]].title));
                    match self.after(at, false) {
                        Some(n) => at = n,
                        None => break,
                    }
                }
            }
        }
        self.active = false;
    }

    /// Queue the song after the current one so it starts without a gap.
    fn queue_next(&mut self) {
        self.next = self.after(self.at, true).filter(|&n| self.load(n).is_ok());
    }

    /// Restart the current song where it was, so a changed shuffle or repeat
    /// applies to what's queued behind it.
    fn requeue(&mut self) {
        let Some((_, pos, paused)) = self.now() else { return };
        self.start_at(self.at);
        self.seek_to(pos);
        if paused {
            self.toggle_pause();
        }
    }

    fn load(&self, at: usize) -> Result<()> {
        let t = &self.queue[self.order[at]];
        let path = self.root.join(&t.location);
        // Opening reads the file's header on the calling (UI) thread: a few
        // milliseconds normally, longer if the iPod's disk has spun down.
        let file = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let source = Decoder::try_from(file).context("decoding")?;
        self.device.as_ref().unwrap().1.append(source);
        Ok(())
    }
}

fn open_device() -> Result<(MixerDeviceSink, rodio::Player)> {
    // Stream errors would otherwise be printed over the TUI.
    let mut sink = DeviceSinkBuilder::from_default_device()?.with_error_callback(|_| {}).open_sink_or_fallback()?;
    sink.log_on_drop(false);
    let out = rodio::Player::connect_new(sink.mixer());
    Ok((sink, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs ffmpeg to make sample files; skipped without it.
    #[test]
    fn decodes_ipod_formats() {
        let dir = std::env::temp_dir().join(format!("rpod-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, codec) in [("a.mp3", "libmp3lame"), ("b.m4a", "aac"), ("c.m4a", "alac")] {
            let path = dir.join(name);
            let ok = std::process::Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=d=1", "-c:a", codec])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return;
            }
            let source = Decoder::try_from(std::fs::File::open(&path).unwrap()).unwrap();
            assert!(source.count() > 40_000, "{name} decoded too few samples");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Plays a few seconds of a real iPod's first songs, quietly:
    /// `RPOD_TEST_IPOD=/run/media/… cargo test play_real -- --nocapture`
    #[test]
    fn play_real() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let ipod = crate::device::Ipod::open(std::path::Path::new(&root)).unwrap();
        let tracks: Vec<Track> = ipod.db.tracks.iter().filter(|t| !crate::export::is_video(t)).take(3).cloned().collect();
        let mut p = Player::new(root.into(), 20);
        p.play(tracks.clone(), 0);
        assert!(p.error.is_none(), "{:?}", p.error);
        std::thread::sleep(Duration::from_secs(2));
        p.tick();
        let (t, pos, paused) = p.now().unwrap();
        println!("playing {} at {pos:?}, paused {paused}, queued next: {:?}", t.title, p.next);
        assert!(pos >= Duration::from_secs(1) && !paused && p.next == Some(1));
        p.seek(30);
        std::thread::sleep(Duration::from_millis(500));
        println!("after +30 s: {:?}", p.now().unwrap().1);
        assert!(p.now().unwrap().1 >= Duration::from_secs(30));
        p.next();
        std::thread::sleep(Duration::from_secs(1));
        p.tick();
        println!("after next: {} at {:?}", p.now().unwrap().0.title, p.now().unwrap().1);
        assert_eq!(p.now().unwrap().0.dbid, tracks[1].dbid);
        p.stop();
        assert!(p.now().is_none());
    }

    #[test]
    fn order_follows_repeat_and_shuffle() {
        let mut p = Player::new(PathBuf::new(), 80);
        p.queue = (0..5).map(|i| Track { dbid: i, ..Default::default() }).collect();
        p.order = (0..5).collect();
        assert_eq!((p.after(3, true), p.after(4, true)), (Some(4), None));
        p.repeat = Repeat::All;
        assert_eq!(p.after(4, true), Some(0));
        p.repeat = Repeat::One;
        assert_eq!((p.after(2, true), p.after(2, false)), (Some(2), Some(3)));
        p.at = 2;
        p.reshuffle();
        assert_eq!((p.order[0], p.at), (2, 0));
        let mut sorted = p.order.clone();
        sorted.sort();
        assert_eq!(sorted, [0, 1, 2, 3, 4]);
    }
}
