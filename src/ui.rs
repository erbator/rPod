use crate::app::{App, ColKind, Item, Tab};
use crate::itunesdb::Track;
use crate::widgets;
use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use crate::player::Repeat;
use ratatui::widgets::{Block, BorderType, Cell, LineGauge, Paragraph, Row, Table, TableState, Wrap};
use ratatui_image::{Resize, StatefulImage};

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const DETAIL_WIDTH: u16 = 40;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    let bar_height = if app.player.now().is_some() { 2 } else { 0 };
    if app.fix.is_some() || app.download.is_some() || app.import.is_some() {
        let [screen, bar] = Layout::vertical([Constraint::Min(0), Constraint::Length(bar_height)])
            .areas(Rect { height: body.height + footer.height, ..body });
        if let Some(view) = &mut app.fix {
            view.draw(f, screen);
        } else if let Some(view) = &mut app.download {
            view.draw(f, screen);
        } else if let Some(view) = &mut app.import {
            view.draw(f, screen);
        }
        draw_now_playing(f, app, bar);
        if let Some(view) = &mut app.cover {
            view.draw(f, f.area());
        }
        return;
    }
    let [body, bar] = Layout::vertical([Constraint::Min(0), Constraint::Length(bar_height)]).areas(body);
    let [cols_area, detail_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(DETAIL_WIDTH)]).areas(body);
    draw_now_playing(f, app, bar);
    draw_columns(f, app, cols_area);
    draw_detail(f, app, detail_area);
    draw_footer(f, app, footer);
    if let Some(view) = &mut app.edit {
        view.draw(f, f.area());
    }
    if let Some(view) = &mut app.cover {
        view.draw(f, f.area());
    }
}

fn draw_now_playing(f: &mut Frame, app: &App, area: Rect) {
    let Some((t, pos, paused)) = app.player.now() else { return };
    let [top, gauge] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let mut spans = vec![
        Span::styled(if paused { " ⏸ " } else { " ▶ " }, Style::new().fg(ACCENT).bold()),
        Span::styled(t.title.as_str(), Style::new().bold()),
    ];
    if !t.artist.is_empty() {
        spans.push(Span::styled(format!(" — {}", t.artist), Style::new().fg(Color::Gray)));
    }
    if !t.album.is_empty() {
        spans.push(Span::styled(format!(" · {}", t.album), Style::new().fg(DIM)));
    }
    f.render_widget(Line::from(spans), top);

    let on = |lit: bool| Style::new().fg(if lit { ACCENT } else { DIM });
    let repeat = match app.player.repeat {
        Repeat::Off => "repeat",
        Repeat::All => "repeat all",
        Repeat::One => "repeat one",
    };
    let key = |k: &'static str| Span::styled(k, Style::new().fg(ACCENT));
    let state = Line::from(vec![
        key("p"),
        Span::styled(if paused { " play  " } else { " pause  " }, Style::new().fg(DIM)),
        key("< >"),
        Span::styled(" skip  ", Style::new().fg(DIM)),
        key("[ ]"),
        Span::styled(" seek  ", Style::new().fg(DIM)),
        key("- +"),
        Span::styled(format!(" vol {}%  ", app.player.volume), Style::new().fg(DIM)),
        key("z"),
        Span::styled(" shuffle  ", on(app.player.shuffle)),
        key("r"),
        Span::styled(format!(" {repeat} "), on(app.player.repeat != Repeat::Off)),
    ])
    .right_aligned();
    f.render_widget(state, top);

    let length = t.length_ms as u64;
    let ratio = if length > 0 { (pos.as_millis() as f64 / length as f64).min(1.0) } else { 0.0 };
    let label = format!(" {} / {} ", fmt_duration(pos.as_millis() as u32), fmt_duration(t.length_ms));
    let g = LineGauge::default()
        .filled_style(Style::new().fg(ACCENT))
        .unfilled_style(Style::new().fg(DIM))
        .ratio(ratio)
        .label(label);
    f.render_widget(g, gauge);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" rPod ", Style::new().bold().fg(Color::Black).bg(ACCENT)), Span::raw(" ")];
    let screen = if app.fix.is_some() {
        Some("Fix missing covers")
    } else if app.download.is_some() {
        Some("Download to PC")
    } else if app.import.is_some() {
        Some("Add music")
    } else {
        None
    };
    if let Some(name) = screen {
        spans.push(Span::styled(name, Style::new().fg(ACCENT).bold()));
    }
    for (i, tab) in Tab::ALL.iter().enumerate().filter(|_| screen.is_none()) {
        let style = if *tab == app.tab {
            Style::new().fg(ACCENT).bold().underlined()
        } else {
            Style::new().fg(DIM)
        };
        spans.push(Span::styled(format!("{} {}", i + 1, tab.title()), style));
        spans.push(Span::raw("  "));
    }
    f.render_widget(Line::from(spans), area);

    let info = format!(
        "{} · {} · {} songs ",
        app.ipod.name(),
        app.ipod.model(),
        app.ipod.db.tracks.len()
    );
    f.render_widget(Line::from(info).fg(DIM).right_aligned(), area);
}

fn draw_columns(f: &mut Frame, app: &mut App, area: Rect) {
    let widths: &[u16] = match app.cols.len() {
        1 => &[100],
        2 => &[38, 62],
        _ => &[24, 30, 46],
    };
    let areas = Layout::horizontal(widths.iter().map(|&w| Constraint::Percentage(w))).split(area);

    for (ci, rect) in areas.iter().enumerate() {
        let focused = ci == app.focus;
        let col = &app.cols[ci];
        // Only the visible window of rows is built; long lists stay cheap.
        let visible = rect.height.saturating_sub(2) as usize;
        let selected = col.state.selected();
        let mut offset = col.state.offset().min(col.items.len().saturating_sub(visible));
        if let Some(sel) = selected {
            if sel < offset {
                offset = sel;
            } else if visible > 0 && sel >= offset + visible {
                offset = sel + 1 - visible;
            }
        }
        let end = (offset + visible).min(col.items.len());
        let rows: Vec<Row> = col.items[offset..end].iter().map(|&it| row_for(app, col.kind, it)).collect();
        let mut state = TableState::default().with_selected(selected.map(|s| s - offset));

        let mut title = vec![Span::raw(format!(" {} ", col.title))];
        if !col.filter.is_empty() {
            title.push(Span::styled(format!("/{} ", col.filter), Style::new().fg(Color::Yellow)));
        }
        let marked_here = col.items.iter().filter(|it| matches!(it, Item::Track(t) if app.marked.contains(t))).count();
        if marked_here > 0 {
            title.push(Span::styled(format!("● {marked_here} marked "), Style::new().fg(Color::Yellow)));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(if focused { Style::new().fg(ACCENT) } else { Style::new().fg(DIM) })
            .title(Line::from(title))
            .title_bottom(Line::from(format!(" {} ", col.items.len())).right_aligned().fg(DIM));

        let highlight = if focused {
            Style::new().bg(ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            Style::new().bg(Color::Rgb(50, 50, 60))
        };
        let table = Table::new(rows, col_widths(col.kind))
            .block(block)
            .row_highlight_style(highlight)
            .column_spacing(1);
        f.render_stateful_widget(table, *rect, &mut state);
        *app.cols[ci].state.offset_mut() = offset;
    }
}

fn col_widths(kind: ColKind) -> Vec<Constraint> {
    match kind {
        ColKind::Names => vec![Constraint::Min(0), Constraint::Length(5)],
        ColKind::AlbumTracks => vec![Constraint::Length(3), Constraint::Min(0), Constraint::Length(5)],
        ColKind::Tracks => vec![
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(35),
            Constraint::Length(5),
        ],
    }
}

fn row_for<'a>(app: &'a App, kind: ColKind, item: Item) -> Row<'a> {
    let dim = |s: String| Cell::from(Line::from(s).fg(DIM).right_aligned());
    match item {
        Item::Artist(a) => {
            let ar = &app.index.artists[a];
            Row::new(vec![Cell::from(ar.name.as_str()), dim(ar.track_count.to_string())])
        }
        Item::Album(a) => {
            let al = &app.index.albums[a];
            let year = if al.year > 0 { al.year.to_string() } else { String::new() };
            Row::new(vec![Cell::from(al.title.as_str()), dim(year)])
        }
        Item::Playlist(p) => {
            let pl = &app.ipod.db.playlists[p];
            let name = if pl.is_master { format!("♫ {}", pl.name) } else { pl.name.clone() };
            Row::new(vec![Cell::from(name), dim(pl.items.len().to_string())])
        }
        Item::Track(t) => {
            let tr = &app.ipod.db.tracks[t];
            let dur = dim(fmt_duration(tr.length_ms));
            let playing = app.player.playing_dbid() == Some(tr.dbid);
            let mark = if app.marked.contains(&t) {
                Style::new().fg(Color::Yellow)
            } else if playing {
                Style::new().fg(ACCENT)
            } else {
                Style::new()
            };
            let title = if playing { Cell::from(format!("♪ {}", tr.title)) } else { Cell::from(tr.title.as_str()) };
            let row = match kind {
                ColKind::AlbumTracks => {
                    let no = if tr.track_no > 0 { tr.track_no.to_string() } else { String::new() };
                    Row::new(vec![dim(no), title, dur])
                }
                _ => Row::new(vec![
                    title,
                    Cell::from(Line::from(tr.artist.as_str()).fg(Color::Gray)),
                    Cell::from(Line::from(tr.album.as_str()).fg(DIM)),
                    dur,
                ]),
            };
            row.style(mark)
        }
    }
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // A square cover in pixels needs fewer rows than columns, since cells are tall.
    let fs = app.picker.font_size();
    let cover_rows = ((inner.width as u32 * fs.width as u32) / fs.height.max(1) as u32) as u16;
    let [cover, text] =
        Layout::vertical([Constraint::Length(cover_rows.min(inner.height / 2)), Constraint::Min(0)]).areas(inner);

    match &mut app.art {
        Some((_, Some(proto))) => {
            let img = StatefulImage::default().resize(Resize::Scale(Some(FilterType::CatmullRom)));
            f.render_stateful_widget(img, cover, proto);
        }
        Some((_, None)) => {
            let msg = Paragraph::new("\n\n\nno artwork").fg(DIM).centered();
            f.render_widget(msg, cover);
        }
        None => {}
    }

    let Some(t) = app.shown_track() else { return };
    let focused_item = app.cols.get(app.focus).and_then(|c| c.selected());
    let lines = match focused_item {
        Some(Item::Track(_)) => track_lines(&app.ipod.db.tracks[t]),
        Some(Item::Album(a)) => {
            let al = &app.index.albums[a];
            let ms: u64 = al.tracks.iter().map(|&i| app.ipod.db.tracks[i].length_ms as u64).sum();
            summary(&al.title, &al.artist, &[
                ("Year", if al.year > 0 { al.year.to_string() } else { "—".into() }),
                ("Tracks", al.tracks.len().to_string()),
                ("Length", fmt_long(ms)),
            ])
        }
        Some(Item::Artist(a)) => {
            let ar = &app.index.artists[a];
            summary(&ar.name, "", &[
                ("Albums", ar.albums.len().to_string()),
                ("Tracks", ar.track_count.to_string()),
            ])
        }
        Some(Item::Playlist(p)) => {
            let pl = &app.ipod.db.playlists[p];
            let ms: u64 = pl
                .items
                .iter()
                .filter_map(|id| app.index.by_id.get(id))
                .map(|&i| app.ipod.db.tracks[i].length_ms as u64)
                .sum();
            summary(&pl.name, "", &[("Tracks", pl.items.len().to_string()), ("Length", fmt_long(ms))])
        }
        None => vec![],
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);
}

fn summary<'a>(title: &str, subtitle: &str, fields: &[(&'a str, String)]) -> Vec<Line<'a>> {
    let mut lines = vec![Line::from(""), Line::from(title.to_string()).bold()];
    if !subtitle.is_empty() {
        lines.push(Line::from(subtitle.to_string()).fg(ACCENT));
    }
    lines.push(Line::from(""));
    lines.extend(fields.iter().map(|(k, v)| field(k, v.clone())));
    lines
}

fn track_lines(t: &Track) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(""),
        Line::from(t.title.clone()).bold(),
        Line::from(t.artist.clone()).fg(ACCENT),
        Line::from(t.album.clone()).fg(Color::Gray),
        Line::from(""),
    ];
    let num = |n: u32, of: u32| match (n, of) {
        (0, _) => "—".to_string(),
        (n, 0) => n.to_string(),
        (n, of) => format!("{n} of {of}"),
    };
    let mut push = |k: &'static str, v: String| {
        if !v.is_empty() {
            lines.push(field(k, v));
        }
    };
    if !t.album_artist.is_empty() && t.album_artist != t.artist {
        push("Album artist", t.album_artist.clone());
    }
    push("Genre", t.genre.clone());
    push("Composer", t.composer.clone());
    push("Year", if t.year > 0 { t.year.to_string() } else { String::new() });
    push("Track", num(t.track_no, t.track_total));
    if t.disc_no > 0 {
        push("Disc", num(t.disc_no, t.disc_total));
    }
    push("Length", fmt_duration(t.length_ms));
    push("Format", format!("{} · {} kbps · {:.1} kHz", t.kind, t.bitrate, t.sample_rate as f32 / 1000.0));
    push("Size", format!("{:.1} MB", t.size as f64 / 1_048_576.0));
    push("Rating", "★".repeat((t.rating / 20) as usize) + &"☆".repeat(5 - (t.rating / 20).min(5) as usize));
    push("Plays", t.play_count.to_string());
    push("Skips", t.skip_count.to_string());
    push("Comment", t.comment.clone());
    lines
}

fn field(k: &str, v: String) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:>12} "), Style::new().fg(DIM)), Span::raw(v)])
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if app.filtering {
        Line::from(vec![
            Span::styled(" filter ", Style::new().fg(Color::Black).bg(Color::Yellow)),
            Span::raw(format!(" {}▏", app.cols[app.focus].filter)),
            Span::styled("   enter keep · esc clear", Style::new().fg(DIM)),
        ])
    } else if let Some((title, tracks)) = &app.confirm_delete {
        Line::from(vec![
            Span::styled(" delete ", Style::new().fg(Color::Black).bg(Color::Red)),
            Span::raw(format!(" {} song(s) from the iPod: {title}?", tracks.len())),
            Span::styled("   y delete · any other key cancels", Style::new().fg(DIM)),
        ])
    } else if let Some(msg) = &app.status {
        Line::from(format!(" {msg}")).fg(Color::Yellow)
    } else {
        let keys = [
            ("enter", "play"),
            ("i", "edit"),
            ("c", "cover"),
            ("C", "fix covers"),
            ("d", "download"),
            ("S", "sync"),
            ("x", "delete"),
            ("space", "mark"),
            ("a", "add music"),
            ("e", "eject"),
            ("1-4/tab", "view"),
            ("/", "filter"),
            ("q", "quit"),
        ];
        widgets::key_hints(keys.iter().filter(|(k, _)| *k != "c" || app.can_pick_cover()))
    };
    f.render_widget(line, area);
}

fn fmt_duration(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn fmt_long(ms: u64) -> String {
    let m = ms / 60_000;
    if m >= 60 { format!("{} h {} min", m / 60, m % 60) } else { format!("{m} min") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Ipod;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend};

    /// Renders screens against a real iPod copy: `RPOD_TEST_IPOD=/path cargo test -- --nocapture`
    #[test]
    fn render_snapshot() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let ipod = Ipod::open(std::path::Path::new(&root)).unwrap();
        let mut app = App::new(ipod, ratatui_image::picker::Picker::halfblocks());
        let mut term = Terminal::new(TestBackend::new(150, 40)).unwrap();
        let keys = [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('l'), KeyCode::Char('l'), KeyCode::Char('j')];
        for k in keys {
            app.on_key(KeyEvent::from(k));
        }
        term.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}", buffer_text(term.backend().buffer()));
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        term.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}", buffer_text(term.backend().buffer()));
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::device::Ipod;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Instant;

    /// `RPOD_TEST_IPOD=/ipod cargo test --release bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn timings() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let t = Instant::now();
        let ipod = Ipod::open(std::path::Path::new(&root)).unwrap();
        println!("open (db + artwork db):  {:?}", t.elapsed());

        #[allow(deprecated)]
        let mut picker = ratatui_image::picker::Picker::from_fontsize((10, 20).into());
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        let t = Instant::now();
        let mut app = App::new(ipod, picker);
        println!("index + first art:       {:?}", t.elapsed());

        let mut term = Terminal::new(TestBackend::new(200, 60)).unwrap();
        for (name, tab) in [("Songs", '3'), ("Artists", '1')] {
            app.on_key(KeyEvent::from(KeyCode::Char(tab)));
            if tab == '1' {
                app.on_key(KeyEvent::from(KeyCode::Char('l')));
                app.on_key(KeyEvent::from(KeyCode::Char('l')));
            }
            term.draw(|f| draw(f, &mut app)).unwrap();
            let n = 200;
            let t = Instant::now();
            for _ in 0..n {
                app.on_key(KeyEvent::from(KeyCode::Char('j')));
                term.draw(|f| draw(f, &mut app)).unwrap();
            }
            println!("{name}: move + redraw:   {:?} per keypress", t.elapsed() / n);
            let t = Instant::now();
            for _ in 0..n {
                term.draw(|f| draw(f, &mut app)).unwrap();
            }
            println!("{name}: redraw only:     {:?}", t.elapsed() / n);
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
            std::thread::sleep(std::time::Duration::from_millis(80));
            app.tick();
            let t = Instant::now();
            term.draw(|f| draw(f, &mut app)).unwrap();
            println!("{name}: frame with new cover: {:?}", t.elapsed());
            let buf = term.backend().buffer();
            let bytes: usize = buf.content().iter().map(|c| c.symbol().len()).sum();
            println!("{name}: bytes in frame:   {} KB", bytes / 1024);
        }
    }
}
