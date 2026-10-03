mod app;
mod artwrite;
mod artworkdb;
mod bytes;
mod covers;
mod coverui;
mod dbwrite;
mod device;
mod edit;
mod editui;
mod fixui;
mod itunes;
mod itunesdb;
mod import;
mod importui;
mod library;
mod store;
mod tags;
mod ui;
mod widgets;

use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use ratatui::crossterm::execute;
use std::time::Duration;
use device::Ipod;
use std::path::PathBuf;

const USAGE: &str = "\
usage: rpod [IPOD_ROOT]          browse an iPod (auto-detected if omitted)
       rpod dump [IPOD_ROOT]     print the iPod's database as text
       rpod cover IPOD_ROOT N OUT.png   export track N's cover
       rpod add [--root IPOD_ROOT] PATH...   add songs/folders without the TUI
       rpod --version";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => println!("{USAGE}"),
        Some("-V" | "--version") => println!("rpod {}", env!("CARGO_PKG_VERSION")),
        Some("dump") => dump(&Ipod::open(&resolve_root(args.get(1))?)?),
        Some("cover") if args.len() == 4 => {
            let ipod = Ipod::open(&PathBuf::from(&args[1]))?;
            let t = &ipod.db.tracks[args[2].parse::<usize>()?];
            let Some(thumb) = ipod.art.best_thumb(t.dbid) else {
                bail!("{} has no artwork", t.title);
            };
            ipod.art.load(thumb)?.save(&args[3])?;
        }
        Some("add") => add_cli(&args[1..])?,
        Some(a) if a.starts_with('-') => bail!("unknown option {a}\n{USAGE}"),
        root => run_tui(&resolve_root(root.map(String::from).as_ref())?)?,
    }
    Ok(())
}

fn run_tui(root: &std::path::Path) -> Result<()> {
    let ipod = Ipod::open(root)?;
    let mut terminal = ratatui::init();
    // Bracketed paste is how dropped files arrive: Kitty pastes their paths.
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    let mut app = app::App::new(ipod, picker);
    let result = (|| -> Result<()> {
        let mut dirty = true;
        while !app.quit {
            if dirty {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                dirty = false;
            }
            let wait = app
                .next_deadline()
                .map_or(Duration::from_millis(100), |d| d.saturating_duration_since(std::time::Instant::now()))
                .min(Duration::from_millis(100));
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    Event::Paste(text) => app.on_paste(&text),
                    _ => {}
                }
                dirty = true;
            }
            dirty |= app.tick();
        }
        Ok(())
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn add_cli(args: &[String]) -> Result<()> {
    let (root, paths) = match args {
        [flag, root, rest @ ..] if flag == "--root" => (PathBuf::from(root), rest),
        rest => (resolve_root(None)?, rest),
    };
    let ipod = Ipod::open(&root)?;
    let on_ipod = ipod.db.tracks.iter().map(import::dup_key).collect();
    let t0 = std::time::Instant::now();
    let files = import::expand(&paths.iter().map(PathBuf::from).collect::<Vec<_>>());
    let items = import::scan(&files, &on_ipod);
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "scanned {} files in {:.2?}", items.len(), t0.elapsed());
    let settings = import::Settings::load();
    for it in &items {
        let _ = writeln!(std::io::stdout(), "{:<40} {:<14} {:?}", it.meta.title, it.format, it.action(&settings));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let names: Vec<String> = items.iter().map(|i| i.meta.title.clone()).collect();
    let t0 = std::time::Instant::now();
    std::thread::spawn(move || import::run(root, items, settings, tx));
    // Keep draining progress even if stdout goes away (e.g. piped into
    // `head`): a panic here would abandon the import half-way.
    let mut out = std::io::stdout();
    for p in rx {
        let at = format!("[{:>6.2?}]", t0.elapsed());
        let _ = match p {
            import::Progress::Item(i, st) => writeln!(out, "{at}   {}: {st:?}", names[i]),
            import::Progress::Phase(s) => writeln!(out, "{at} {s}"),
            import::Progress::Finished(Ok(n)) => writeln!(out, "{at} done: added {n}"),
            import::Progress::Finished(Err(e)) => bail!(e),
        };
    }
    Ok(())
}

fn resolve_root(arg: Option<&String>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(PathBuf::from(p));
    }
    match device::find_mounted()?.into_iter().next() {
        Some(p) => Ok(p),
        None => bail!("no mounted iPod found; plug one in or pass its mount path"),
    }
}

fn dump(ipod: &Ipod) {
    let db = &ipod.db;
    println!("{} — {} (db version {:#x})", ipod.name(), ipod.model(), db.version);
    println!("{} tracks, {} playlists, {} with artwork\n", db.tracks.len(), db.playlists.len(), ipod.art.by_track.len());
    for (i, t) in db.tracks.iter().enumerate() {
        let thumbs = ipod.art.by_track.get(&t.dbid).map_or(0, Vec::len);
        println!(
            "{i:5} {:>2}/{:<2} {} — {} — {} [{}] {}:{:02} {}kbps {} art={thumbs} plays={} {}",
            t.track_no, t.track_total, t.artist, t.album, t.title, t.year,
            t.length_ms / 60000, t.length_ms / 1000 % 60, t.bitrate, t.kind, t.play_count, t.location
        );
    }
    println!();
    for p in &db.playlists {
        println!("playlist {:?} master={} podcast={} items={}", p.name, p.is_master, p.is_podcast, p.items.len());
    }
}
