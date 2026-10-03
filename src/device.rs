//! Locating mounted iPods and reading their identity.

use crate::artworkdb::{self, ArtworkDb};
use crate::itunesdb::{self, ITunesDb};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Ipod {
    pub root: PathBuf,
    pub info: BTreeMap<String, String>,
    pub db: ITunesDb,
    pub art: ArtworkDb,
}

impl Ipod {
    pub fn open(root: &Path) -> Result<Self> {
        let ctl = root.join("iPod_Control");
        let db = itunesdb::read(&ctl.join("iTunes/iTunesDB"))?;
        // Missing or unreadable artwork shouldn't stop browsing.
        let art = artworkdb::read(&ctl.join("Artwork")).unwrap_or_default();
        Ok(Self {
            root: root.to_path_buf(),
            info: read_sysinfo(&ctl.join("Device/SysInfo")),
            db,
            art,
        })
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "iPod".into())
    }

    /// e.g. "iPod Video 5.5th Gen 30GB"
    pub fn model(&self) -> String {
        let get = |k: &str| self.info.get(k).map(String::as_str).unwrap_or("");
        [get("ModelFamily"), get("Generation"), get("Capacity")]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Flush, unmount and power off the iPod so it's safe to unplug.
pub fn eject(root: &Path) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/mounts")?;
    let dev = mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?, unescape_mount(f.next()?)))
        })
        .find(|(_, m)| m == root)
        .map(|(d, _)| d.to_string())
        .context("iPod mount not found")?;
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("udisksctl").args(args).output().context("running udisksctl")?;
        if !out.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    run(&["unmount", "-b", &dev])?;
    // Power off the whole disk (sdc), not the partition (sdc2).
    let disk = dev.trim_end_matches(|c: char| c.is_ascii_digit());
    run(&["power-off", "-b", disk])
}

/// `SysInfo` is `Key: value` lines.
fn read_sysinfo(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Mount points that contain an `iPod_Control` directory.
pub fn find_mounted() -> Result<Vec<PathBuf>> {
    let mounts = std::fs::read_to_string("/proc/mounts").context("reading /proc/mounts")?;
    Ok(mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(unescape_mount)
        .filter(|p| p.join("iPod_Control/iTunes/iTunesDB").is_file())
        .collect())
}

/// /proc/mounts escapes spaces and friends as octal, e.g. `\040`.
fn unescape_mount(s: &str) -> PathBuf {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}
