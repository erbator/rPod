<div align="center">
  <h1>rPod</h1>

  <p>A terminal iPod manager for Linux.</p>

  <p>
    <a href="https://github.com/erbator/rPod/actions"><img src="https://img.shields.io/github/actions/workflow/status/erbator/rPod/ci.yml?branch=master&style=flat-square&label=CI" alt="CI status" /></a>
    <a href="https://github.com/erbator/rPod/releases"><img src="https://img.shields.io/github/v/release/erbator/rPod?style=flat-square" alt="Latest release" /></a>
    <a href="./LICENSE.md"><img src="https://img.shields.io/badge/license-PolyForm%20Noncommercial-blue?style=flat-square" alt="PolyForm Noncommercial license" /></a>
    <img src="https://img.shields.io/badge/Rust-1.85+-DEA584?logo=rust&logoColor=white&style=flat-square" alt="Rust 1.85+" />
  </p>

  <p>
    <a href="#features">Features</a> ·
    <a href="#compatibility">Compatibility</a> ·
    <a href="#quick-start">Quick start</a> ·
    <a href="#keys">Keys</a> ·
    <a href="#adding-music">Adding music</a> ·
    <a href="#internals">Internals</a>
  </p>

  <img src="showcase.gif" alt="rPod browsing an iPod with album art" width="800" />
</div>

rPod browses, edits and fills an iPod from the terminal. It reads and writes the
iPod's own databases directly, with no iTunes or libgpod, and is built against
the iPod Video 5G/5.5G.

## Features

| Feature | What it does |
|---------|--------------|
| Browsing | Artists, albums, songs and playlists in drill-down columns, live filter, track details |
| Album art | Full-resolution covers in Kitty, WezTerm and Ghostty; half-blocks elsewhere |
| Editing | One track or a whole album at once, mixed values, auto-numbering, tags written to files too |
| Deleting | A song, album, artist or marked songs, from the database, playlists and disk |
| Covers | Search Apple's catalogue in a grid of covers, or fix every missing cover in one pass |
| Adding music | Drag and drop, parallel ffmpeg conversion, duplicate and free-space checks |
| Downloading | Copy songs back to your PC with proper names and tags; later syncs copy only what's new |
| Playing | Play songs straight off the iPod: gapless albums, shuffle, repeat, seeking |
| Safety | Backs up before every write and checks the result before replacing anything |

## Compatibility

| Device | Browse | Add, edit, covers |
|--------|--------|-------------------|
| iPod Video 5G / 5.5G | yes (tested on a 5.5G) | yes (tested on a copy of a real database) |
| iPod Classic 6G / 6.5G / 7G | untested | no: needs the hash58 signature |
| iPod nano 3G–4G | untested | no: needs hash58 |
| iPod nano 5G–7G | untested | no: needs hash72 / hashAB |
| iPod 1G–4G, mini, nano 1G–2G | untested | untested |
| iPod touch, shuffle | no | no |

All of this has been tested on exactly one iPod, a borrowed Video 5.5G, so
read every "untested" literally.

**Don't change anything on an iPod Classic (6G or later) yet.** Those models
reject a database without a valid signature, which rPod doesn't write. The iPod
would show an empty library until you restore the database from the backup.
Browsing is fine.

## How it works

Files are scanned and converted with ffmpeg in parallel, but copied to the iPod
one at a time: the iPod Video has a tiny hard disk that slows to a crawl when
several writes compete for it. The databases are backed up and written last, and
if that fails, the songs it copied are deleted again.

## Quick start

Install a prebuilt binary for x86_64 or ARM64 Linux with glibc 2.35 or newer
(checked against its SHA-256 checksum; other machines build from source with
cargo):

```sh
curl -fsSL https://raw.githubusercontent.com/erbator/rPod/master/install.sh | sh
```

Or build it yourself with Rust 1.85 or newer and the ALSA headers (`alsa-lib`
on Arch, `libasound2-dev` on Debian/Ubuntu):

```sh
git clone https://github.com/erbator/rPod && cd rPod
cargo build --release
```

Plug in the iPod, let your desktop mount it, and run `rpod`. It finds the iPod
on its own, or takes the mount path as an argument.

Optional: `ffmpeg` for converting formats the iPod can't play, `udisksctl` for
the eject key. The install script takes `RPOD_VERSION` and `RPOD_INSTALL_DIR`;
`uninstall.sh` removes rPod and keeps settings and backups unless you pass
`--purge`.

## Keys

| Category | Keys |
|----------|------|
| Navigation | `↑↓` / `jk` move, `←→` / `hl` / `Enter` columns, `1`–`4` / `Tab` views, `g` `G` jump, `/` filter |
| Editing | `i` edit selection, `space` mark tracks; in the editor `n` auto-number, `t` totals, `x` tidy, `f` tags to files, `Ctrl+S` save |
| Covers | `c` pick a cover, `C` fix missing covers; `y` / `n` / `a` accept, skip, accept confident, `f` also embed in song files, `Ctrl+S` write |
| Playing | `Enter` on a song plays from there, `p` pause, `<` `>` previous/next, `[` `]` seek 10 s, `-` `+` volume, `z` shuffle, `r` repeat |
| Library | `a` add music, `d` download selection, `x` delete selection, `S` sync everything to PC, `e` eject, `q` quit |

The cover picker hides albums by other artists and singles (`x` shows them),
searches with `/`, switches store country with `Tab`, and
accepts a dropped image file as your own cover. Covers go into the iPod's own
artwork database; embedding them in the song files as well (`f`) is off by
default because it rewrites every file over USB, a second or two per track. Always eject with `e` (or from
your desktop) after a change; the iPod rebuilds its menus when ejected.

| Command | Does |
|---------|------|
| `rpod add PATH...` | add music without the TUI |
| `rpod dump` | print the database as text |
| `rpod cover ROOT N OUT.png` | export track N's cover |

## Adding music

Press `a` or drop files and folders onto the terminal window. In the queue, `c`
picks a cover for an album and `C` finds covers for every album without one.

| Setting | Default | Choices |
|---------|---------|---------|
| Lossless (FLAC, WAV, AIFF, APE, WavPack) | ALAC | AAC 320/256/192, MP3 320, MP3 V0 |
| Unplayable lossy (Ogg, Opus, Musepack) | AAC 256 | AAC 192/128, MP3 320, MP3 V0 |
| Shrink MP3/AAC above | off | 320/256/192 kbps |
| Parallel conversions | CPU cores | 1 to 2× cores |
| Skip duplicates | yes | yes/no |
| Folder art fallback | yes | yes/no |

Audio above 48 kHz or 16-bit is always converted to 16-bit/44.1 kHz, the iPod
Video's limit. Settings live in `~/.config/rpod/settings.json`.

Before every write, rPod copies `iTunesDB` and `ArtworkDB` to
`~/.local/share/rpod/backups/<FirewireGuid>/<unix-timestamp>/`. To undo a
change, copy them back into `iPod_Control/iTunes/` and `iPod_Control/Artwork/`.

## Playing

`Enter` on a song plays it and then the rest of the list it's in: the album,
playlist, artist or Songs view. The bar above the footer shows what's playing
and the player keys, which also work on the Add music, download and
fix-covers screens. Sound goes through ALSA, so PipeWire and PulseAudio work
as usual. Volume is remembered; ejecting stops playback first so the iPod can
unmount.

## Deleting

`x` (or `Delete`) deletes the selection from the iPod: a song, an album, an
artist or the marked songs, after a `y` to confirm. To delete songs in a
playlist, open it and pick them; the playlist row itself is refused. The
databases are backed up and the new iTunesDB is checked before any song file
is deleted. The backup only restores the database: deleted song files are
gone for good.

## Downloading to your PC

`d` copies the selection (a song, album, artist, playlist or marked songs) off
the iPod; `S` copies the whole library. Files land in `~/Music` by default
as `Artist/Album/01 Title.mp3`, compilations under `Compilations/Album`; press
`o` or drop a folder on the screen to change it, and the choice is remembered.

Many songs on an iPod have no tags of their own, so every download gets the
iPod's title, artist, album artist, album, numbers, year, genre and
compilation flag written in: ID3v2.3 for MP3 (what Windows and older players
read best), iTunes tags for M4A. Files with no cover, or a smaller one, get the
iPod's thumbnail; bigger covers are kept. Videos stay on the iPod.

rPod remembers what it downloaded, per iPod and folder, in
`~/.local/share/rpod/downloads/`. Running it again copies only new songs and
retags or renames ones you edited on the iPod since. Nothing on the iPod is
changed, and nothing on the PC is deleted or overwritten: a different file
already at a song's path is left alone and reported. Stop with `Esc` and the
next run picks up where it left off.

## Internals

| Piece | Format |
|-------|--------|
| iTunesDB | `mhbd` → `mhsd` sections for tracks (`mhit`), playlists (`mhyp`), albums (`mhia`) and artists (`mhii`) |
| Menus | Master playlist `mhod` 52/53 sort indexes and jump tables, regenerated on every write |
| ArtworkDB | `mhii` per track → `mhni` per thumbnail size → `.ithmb` file name |
| Thumbnails | Raw RGB565 little-endian; 100×100 (1028) and 200×200 (1029) on the iPod Video |

Writes copy every unchanged chunk verbatim, rename a temp file into place, and
re-parse the result before replacing the original. Covers in the browser are
cached per album and load only once scrolling pauses, since each one costs
hundreds of KB of terminal escape sequences. Cover search tries up to three
queries and scores results on title, artist, track count and year; a match by a
clearly different artist scores low.

Tests that need iPod data read a copy of one from `RPOD_TEST_IPOD`:

```sh
RPOD_TEST_IPOD=/path/to/ipod-copy cargo test --release
```

## Acknowledgements

Thanks to [DarkAaronfox](https://github.com/DarkAaronfox) for lending me his
iPod Video 5.5G; see also his Soulseek client
[crabseek](https://github.com/DarkAaronfox/crabseek). Format work leans on the
iPodLinux wiki and libgpod; [iOpenPod](https://github.com/TheRealSavi/iOpenPod)
was a reference for field layouts (no code copied), and the artwork URL
techniques come from
[Ben Dodson's iTunes Artwork Finder](https://bendodson.com/projects/itunes-artwork-finder/).
Built on [ratatui](https://ratatui.rs),
[ratatui-image](https://github.com/benjajaja/ratatui-image) and
[lofty](https://github.com/Serial-ATA/lofty-rs).

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md)
