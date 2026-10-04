# rPod

[![License: PolyForm Noncommercial](https://img.shields.io/badge/license-PolyForm%20Noncommercial-ef9421)](LICENSE.md)
[![CI](https://github.com/erbator/rPod/actions/workflows/ci.yml/badge.svg)](https://github.com/erbator/rPod/actions/workflows/ci.yml)

A terminal iPod manager for Linux, written in Rust. It reads and writes the iPod's own databases directly (no iTunes, no libgpod) and is built against the iPod Video 5G/5.5G.

<img src="showcase.gif" alt="rPod browsing an iPod with album art" width="1000">

## Install

Either use the install script, which puts a static binary for x86_64 or ARM64 Linux in `~/.local/bin` after checking its SHA-256 checksum (and falls back to building with cargo on other machines):

```bash
curl -fsSL https://raw.githubusercontent.com/erbator/rPod/master/install.sh | sh
```

or build it yourself:

```bash
git clone https://github.com/erbator/rPod && cd rPod
cargo build --release   # → target/release/rpod
```

The script takes `RPOD_VERSION` (e.g. `v0.2.0`) and `RPOD_INSTALL_DIR`. To uninstall, run `uninstall.sh` the same way; it keeps your settings and database backups unless you pass `--purge` (`| sh -s -- --purge`).

Optional: `ffmpeg` for converting formats the iPod can't play, `udisksctl` for the eject key, and Kitty, WezTerm or Ghostty for full-resolution album art. Other terminals get a half-block approximation.

## Compatibility

| Device | Browse | Add, edit, covers |
|---|---|---|
| iPod Video 5G / 5.5G | yes (tested on a 5.5G) | yes (tested on a copy of a real database) |
| iPod Classic 6G / 6.5G / 7G | untested | no: needs the hash58 signature |
| iPod nano 3G–4G | untested | no: needs hash58 |
| iPod nano 5G–7G | untested | no: needs hash72 / hashAB |
| iPod 1G–4G, mini, nano 1G–2G | untested | untested |
| iPod touch, shuffle | no | no |

**Don't change anything on an iPod Classic (6G or later) yet.** Those models reject a database without a valid signature, which rPod doesn't write. The iPod would show an empty library until you restore the database from the backup. Browsing is fine.

## Usage

Plug in the iPod and let your desktop mount it. `rpod` finds it on its own, or you can pass the mount path (`rpod "/run/media/$USER/IPOD"`).

| Key | Action |
|---|---|
| <kbd>↑</kbd> <kbd>↓</kbd> / <kbd>j</kbd> <kbd>k</kbd> | move |
| <kbd>←</kbd> <kbd>→</kbd> / <kbd>h</kbd> <kbd>l</kbd> / <kbd>Enter</kbd> | move between columns |
| <kbd>1</kbd>–<kbd>4</kbd> / <kbd>Tab</kbd> | Artists, Albums, Songs, Playlists |
| <kbd>g</kbd> <kbd>G</kbd> / <kbd>PgUp</kbd> <kbd>PgDn</kbd> | jump |
| <kbd>/</kbd> | filter the current column |
| <kbd>i</kbd> | edit the selected track, album, artist or playlist |
| <kbd>space</kbd> | mark tracks to edit together |
| <kbd>c</kbd> | find a cover for the selection |
| <kbd>C</kbd> | fix missing covers across the iPod |
| <kbd>a</kbd> | add music |
| <kbd>e</kbd> | eject |
| <kbd>q</kbd> | quit |

Eject with <kbd>e</kbd> (or from your desktop) before unplugging after any change; the iPod rebuilds its menus from the new database when ejected.

There's also a command line:

```bash
rpod add ~/Music/Some\ Album         # add music without the TUI
rpod dump                            # print the database as text
rpod cover /path/to/ipod 42 out.png  # export track 42's cover
```

### Editing

<kbd>i</kbd> opens an editor for title, artist, album, album artist, genre, year, track and disc numbers, composer, comment, compilation and rating. When several tracks are selected, fields that differ show as `‹mixed›` and keep their per-track values unless you type over them. <kbd>n</kbd> numbers the tracks 1…n, <kbd>t</kbd> fills in the totals, <kbd>x</kbd> tidies whitespace. <kbd>Ctrl</kbd>+<kbd>S</kbd> saves to the iPod's database and also writes the tags into the audio files, unless you turn that off with <kbd>f</kbd>.

### Covers

Covers come from Apple's iTunes catalogue (up to 3000×3000) or from an image file dropped onto the window.

<kbd>c</kbd> opens a grid of results for the selected album, best match first. <kbd>Enter</kbd> applies, <kbd>b</kbd> returns to the best match, <kbd>/</kbd> runs your own search, and <kbd>Tab</kbd> switches the store country (your system's by default).

<kbd>C</kbd> searches for every album with tracks lacking art and lists each result as `confident`, `review` or `no match`. <kbd>y</kbd> accepts, <kbd>n</kbd> skips, <kbd>a</kbd> accepts all confident matches, <kbd>Enter</kbd> opens the picker for that album, and <kbd>Ctrl</kbd>+<kbd>S</kbd> writes the accepted covers in one go. Nothing is written before that.

The iPod gets its own thumbnails, and the audio files get a 1000 px JPEG embedded. Searches are paced to Apple's limit of about 20 a minute and cached for a week in `~/.cache/rpod`.

## Adding music

Press <kbd>a</kbd> or drop files and folders onto the terminal window (Kitty pastes dropped paths). Files are scanned in parallel; anything the iPod can't play is converted with ffmpeg, also in parallel; then the files are copied to the iPod one at a time, because the iPod Video's hard disk slows down badly under parallel writes. Covers come from the file's tags, a `cover.jpg`/`folder.jpg` next to it, or online: in the queue, <kbd>c</kbd> picks a cover for an album and <kbd>C</kbd> finds covers for every album without one, using only the confident matches. Songs already on the iPod are skipped, free space is checked first, and multi-valued artist tags are kept whole ("Nujabes, Fat Jon").

| Setting | Default | Choices |
|---|---|---|
| Lossless (FLAC, WAV, AIFF, APE, WavPack) | ALAC | AAC 320/256/192, MP3 320, MP3 V0 |
| Unplayable lossy (Ogg, Opus, Musepack) | AAC 256 | AAC 192/128, MP3 320, MP3 V0 |
| Shrink MP3/AAC above | off | 320/256/192 kbps |
| Parallel conversions | CPU cores | 1 to 2× cores |
| Skip duplicates | yes | yes/no |
| Folder art fallback | yes | yes/no |

Audio above 48 kHz or 16-bit is always converted to 16-bit/44.1 kHz, since the iPod Video can't play it. Settings are stored in `~/.config/rpod/settings.json`.

### Backups and undo

Before every write, rPod copies `iTunesDB` and `ArtworkDB` to `~/.local/share/rpod/backups/<FirewireGuid>/<unix-timestamp>/`. To undo a change, copy those two files back into `iPod_Control/iTunes/` and `iPod_Control/Artwork/`.

## Internals

rPod works on the iPod's on-disk formats directly. Writes are conservative: every chunk that doesn't need to change is copied byte for byte, the new database is written to a temp file and renamed into place, and the result is re-parsed and checked before it replaces the original. If an import fails, the audio files it copied are removed.

```text
mhbd                          database header
├─ mhsd type 1                tracks
│  └─ mhlt → mhit …           one per track, each followed by mhod strings
├─ mhsd type 2 / 3            playlists (3 = podcast-grouped copy)
│  └─ mhlp → mhyp …           mhod 52/53 sort indexes + jump tables, then mhip items
├─ mhsd type 4                album list (mhla → mhia)
└─ mhsd type 8                artist list (mhli → mhii)
```

Adding or editing tracks rewrites the affected `mhit`s and `mhip`s, links tracks to album (`mhia`) and artist (`mhii`) entries, and regenerates the master playlist's sort indexes (title, album, artist, genre, composer) and their letter jump tables, which the iPod's Music menus are built from. Edited fields also get their "sort as" records regenerated ("The Strokes" → "Strokes, The").

```text
mhfd
└─ mhsd type 1 → mhli → mhii …     one image entry per track (song id = track dbid)
                         └─ mhod 2 → mhni   one per thumbnail size
                                     └─ mhod 3   ":F1029_1.ithmb"
```

Thumbnails are raw RGB565 little-endian pixels at an offset inside `.ithmb` files: 100×100 (format 1028) and 200×200 (format 1029) on the iPod Video. A replaced cover's old pixels stay in the `.ithmb` file as unused space.

Cover art in the browser is cached per album and only loaded once scrolling pauses, because sending a cover to the terminal costs several hundred KB of escape sequences; holding a key would otherwise push that on every keypress. Opening a 1,400-track database takes a few milliseconds.

Cover search tries up to three queries (main artist plus the album's core words, the album alone, the artist's discography) and scores results on title, artist, track count and year. A matching title by a clearly different artist scores low, titles in another script count as unknown rather than wrong, and a match is only "confident" if it clearly beats the runner-up.

| File | Purpose |
|---|---|
| `bytes.rs` | bounds-checked little-endian chunk reader |
| `itunesdb.rs`, `artworkdb.rs` | parsers |
| `dbwrite.rs`, `artwrite.rs` | writers |
| `store.rs` | backups and atomic writes |
| `device.rs` | finding mounted iPods, `SysInfo`, eject |
| `library.rs` | artist/album/song indexes |
| `import.rs` | scan, convert, copy, commit |
| `edit.rs`, `tags.rs` | metadata edits and file tags |
| `itunes.rs`, `covers.rs` | cover search and applying covers |
| `app.rs`, `ui.rs`, `importui.rs`, `editui.rs`, `coverui.rs`, `fixui.rs`, `widgets.rs` | the TUI |

## Development

Tests that need iPod data read it from an environment variable, so no library ends up in the repo. Point it at a copy of an iPod (a folder with `iPod_Control/{iTunes,Artwork,Device}`), never the device:

```bash
RPOD_TEST_IPOD=/path/to/ipod-copy cargo test --release
```

## Acknowledgements

Thanks to [DarkAaronfox](https://github.com/DarkAaronfox) for lending me his iPod Video 5.5G, which rPod was built and tested against; see also his Soulseek client [crabseek](https://github.com/DarkAaronfox/crabseek). The format work leans on the iPodLinux wiki and libgpod. [iOpenPod](https://github.com/TheRealSavi/iOpenPod) (GPLv3) was a reference for field layouts; no code was copied. The artwork URL techniques come from [Ben Dodson's iTunes Artwork Finder](https://bendodson.com/projects/itunes-artwork-finder/). Built on [ratatui](https://ratatui.rs), [ratatui-image](https://github.com/benjajaja/ratatui-image) and [lofty](https://github.com/Serial-ATA/lofty-rs).

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md): free to use, change and share for any noncommercial purpose.
