//! Safety around writing to the iPod: backups and atomic file replacement.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn itunesdb_path(root: &Path) -> PathBuf {
    root.join("iPod_Control/iTunes/iTunesDB")
}

pub fn artwork_dir(root: &Path) -> PathBuf {
    root.join("iPod_Control/Artwork")
}

/// `~/.local/share/rpod/backups/<FirewireGuid>/<unix time>/`
fn backup_dir(root: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    let guid = std::fs::read_to_string(root.join("iPod_Control/Device/SysInfo"))
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("FirewireGuid:").map(|g| g.trim().to_string())))
        .unwrap_or_else(|| "unknown".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    base.join("rpod/backups").join(guid).join(stamp.to_string())
}

/// Copy iTunesDB and ArtworkDB somewhere safe before changing them.
pub fn backup(root: &Path) -> Result<PathBuf> {
    let dir = backup_dir(root);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(itunesdb_path(root), dir.join("iTunesDB")).context("backing up iTunesDB")?;
    let art = artwork_dir(root).join("ArtworkDB");
    if art.exists() {
        std::fs::copy(&art, dir.join("ArtworkDB")).context("backing up ArtworkDB")?;
    }
    Ok(dir)
}

/// Write via a temp file and rename, so an interrupted write never leaves a
/// half-written database.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("rpod-tmp");
    std::fs::write(&tmp, data)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Tests that point XDG_DATA_HOME at a temp dir hold this so they don't race.
#[cfg(test)]
pub static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
