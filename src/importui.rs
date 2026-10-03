//! The "Add music" screen: a drop zone / queue, conversion options, and a
//! live progress view while files are converted and copied.

use crate::import::{self, Action, Item, LOSSLESS_TARGETS, LOSSY_TARGETS, Progress, Settings, Status};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Gauge, Paragraph, Row, Table, TableState, Wrap};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Pane {
    Queue,
    Options,
}

struct Run {
    rx: Receiver<Progress>,
    total: usize,
    done: usize,
    failed: usize,
    phase: String,
    started: Instant,
    result: Option<Result<usize, String>>,
}

pub struct ImportView {
    root: PathBuf,
    items: Vec<Item>,
    table: TableState,
    settings: Settings,
    focus: Pane,
    opt: usize,
    path_input: Option<String>,
    scanning: usize,
    scan_tx: Sender<Vec<Item>>,
    scan_rx: Receiver<Vec<Item>>,
    on_ipod: Arc<HashSet<(String, String, String)>>,
    run: Option<Run>,
    message: Option<String>,
    ffmpeg: bool,
    /// Set when the user leaves the screen; `true` if the library changed.
    pub closed: Option<bool>,
}

const OPTION_COUNT: usize = 6;
const SHRINK_STEPS: &[u32] = &[0, 320, 256, 192];

impl ImportView {
    pub fn new(root: PathBuf, on_ipod: HashSet<(String, String, String)>) -> Self {
        let (scan_tx, scan_rx) = channel();
        Self {
            root,
            items: Vec::new(),
            table: TableState::default(),
            settings: Settings::load(),
            focus: Pane::Queue,
            opt: 0,
            path_input: None,
            scanning: 0,
            scan_tx,
            scan_rx,
            on_ipod: Arc::new(on_ipod),
            run: None,
            message: None,
            ffmpeg: import::ffmpeg_available(),
            closed: None,
        }
    }

    /// Queue files/folders; tags are read on a background thread.
    pub fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.run.is_some() {
            return;
        }
        self.scanning += 1;
        let tx = self.scan_tx.clone();
        let on_ipod = self.on_ipod.clone();
        std::thread::spawn(move || {
            let files = import::expand(&paths);
            tx.send(import::scan(&files, &on_ipod)).ok();
        });
    }

    pub fn on_paste(&mut self, text: &str) {
        let paths = import::parse_dropped(text);
        if paths.is_empty() {
            self.message = Some("Nothing usable in that drop — expected files or folders.".into());
        } else {
            self.add_paths(paths);
        }
    }

    /// Drain background channels. Returns true if anything changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(new) = self.scan_rx.try_recv() {
            self.scanning -= 1;
            let known: HashSet<PathBuf> = self.items.iter().map(|i| i.src.clone()).collect();
            let before = self.items.len();
            self.items.extend(new.into_iter().filter(|i| !known.contains(&i.src)));
            let added = self.items.len() - before;
            self.message = Some(if added == 0 {
                "No new audio files found there.".into()
            } else {
                format!("Added {added} file{} to the queue.", if added == 1 { "" } else { "s" })
            });
            if self.table.selected().is_none() && !self.items.is_empty() {
                self.table.select(Some(0));
            }
            changed = true;
        }
        if let Some(run) = &mut self.run {
            while let Ok(p) = run.rx.try_recv() {
                changed = true;
                match p {
                    Progress::Item(i, status) => {
                        match &status {
                            Status::Working("waiting for database") => run.done += 1,
                            Status::Failed(_) => {
                                run.done += 1;
                                run.failed += 1;
                            }
                            _ => {}
                        }
                        self.items[i].status = status;
                    }
                    Progress::Phase(s) => run.phase = s,
                    Progress::Finished(r) => {
                        run.phase = match &r {
                            Ok(n) => format!("Added {n} song{} to your iPod.", if *n == 1 { "" } else { "s" }),
                            Err(e) => format!("Import failed: {e}"),
                        };
                        run.result = Some(r);
                    }
                }
            }
        }
        changed
    }

    fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.result.is_none())
    }

    fn start(&mut self) {
        let work = self.items.iter().filter(|i| !matches!(i.action(&self.settings), Action::Skip(_))).count();
        if work == 0 {
            self.message = Some("Nothing to add — drop some music here first.".into());
            return;
        }
        let needs_ffmpeg = self.items.iter().any(|i| matches!(i.action(&self.settings), Action::Convert(_)));
        if needs_ffmpeg && !self.ffmpeg {
            self.message = Some("ffmpeg isn't installed, so files that need converting can't be added.".into());
            return;
        }
        let needed: u64 = self.items.iter().map(|i| i.estimated_size(&self.settings)).sum();
        if import::free_space(&self.root).is_some_and(|free| needed > free) {
            self.message = Some("Not enough free space on the iPod for this queue.".into());
            return;
        }
        self.settings.save();
        let (tx, rx) = channel();
        let (root, items, settings) = (self.root.clone(), self.items.clone(), self.settings.clone());
        std::thread::spawn(move || import::run(root, items, settings, tx));
        self.run = Some(Run {
            rx,
            total: work,
            done: 0,
            failed: 0,
            phase: "Converting and copying…".into(),
            started: Instant::now(),
            result: None,
        });
        self.message = None;
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.message = None;
        if let Some(input) = &mut self.path_input {
            match key.code {
                KeyCode::Enter => {
                    let text = std::mem::take(input);
                    self.path_input = None;
                    let expanded = match text.trim().strip_prefix("~/") {
                        Some(rest) => std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or(text.clone()),
                        None => text.trim().to_string(),
                    };
                    self.on_paste(&expanded);
                }
                KeyCode::Esc => self.path_input = None,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return;
        }

        if let Some(run) = &self.run {
            if run.result.is_some() && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                self.closed = Some(matches!(run.result, Some(Ok(n)) if n > 0));
            }
            return; // no editing while running
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.closed = Some(false),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Pane::Queue { Pane::Options } else { Pane::Queue };
            }
            KeyCode::Char('o') => self.path_input = Some(String::new()),
            KeyCode::Char('s') | KeyCode::Enter if self.focus == Pane::Queue => self.start(),
            KeyCode::Char('s') => self.start(),
            _ if self.focus == Pane::Options => self.on_option_key(key),
            _ => self.on_queue_key(key),
        }
    }

    fn on_queue_key(&mut self, key: KeyEvent) {
        let n = self.items.len();
        let sel = self.table.selected().unwrap_or(0);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.table.select(Some((sel + 1).min(n - 1))),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.table.select(Some(sel.saturating_sub(1))),
            KeyCode::PageDown if n > 0 => self.table.select(Some((sel + 20).min(n - 1))),
            KeyCode::PageUp if n > 0 => self.table.select(Some(sel.saturating_sub(20))),
            KeyCode::Char(' ') if n > 0 => {
                self.items[sel].enabled = !self.items[sel].enabled;
                self.table.select(Some((sel + 1).min(n - 1)));
            }
            KeyCode::Char('a') => {
                let all = self.items.iter().all(|i| i.enabled);
                self.items.iter_mut().for_each(|i| i.enabled = !all);
            }
            KeyCode::Char('d') | KeyCode::Delete if n > 0 => {
                self.items.remove(sel);
                self.table.select(if self.items.is_empty() { None } else { Some(sel.min(self.items.len() - 1)) });
            }
            KeyCode::Char('D') => {
                self.items.clear();
                self.table.select(None);
            }
            _ => {}
        }
    }

    fn on_option_key(&mut self, key: KeyEvent) {
        let dir: isize = match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.opt = (self.opt + 1) % OPTION_COUNT;
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.opt = (self.opt + OPTION_COUNT - 1) % OPTION_COUNT;
                return;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::Enter => 1,
            KeyCode::Left | KeyCode::Char('h') => -1,
            _ => return,
        };
        let cycle = |len: usize, cur: usize| ((cur as isize + dir).rem_euclid(len as isize)) as usize;
        let s = &mut self.settings;
        match self.opt {
            0 => {
                let i = LOSSLESS_TARGETS.iter().position(|t| *t == s.lossless).unwrap_or(0);
                s.lossless = LOSSLESS_TARGETS[cycle(LOSSLESS_TARGETS.len(), i)];
            }
            1 => {
                let i = LOSSY_TARGETS.iter().position(|t| *t == s.lossy).unwrap_or(0);
                s.lossy = LOSSY_TARGETS[cycle(LOSSY_TARGETS.len(), i)];
            }
            2 => {
                let i = SHRINK_STEPS.iter().position(|&k| k == s.shrink_above).unwrap_or(0);
                s.shrink_above = SHRINK_STEPS[cycle(SHRINK_STEPS.len(), i)];
            }
            3 => {
                let max = std::thread::available_parallelism().map_or(8, |n| n.get()) * 2;
                s.jobs = (s.jobs as isize + dir).clamp(1, max as isize) as usize;
            }
            4 => s.skip_duplicates = !s.skip_duplicates,
            _ => s.folder_art = !s.folder_art,
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let [queue, bottom, footer] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(10), Constraint::Length(1)]).areas(area);
        self.draw_queue(f, queue);
        if self.run.is_some() {
            self.draw_progress(f, bottom);
        } else {
            let [opts, summary] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(bottom);
            self.draw_options(f, opts);
            self.draw_summary(f, summary);
        }
        self.draw_footer(f, footer);
    }

    fn draw_queue(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Pane::Queue && self.run.is_none();
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
            .title(" Add music ")
            .title_bottom(Line::from(format!(" {} files ", self.items.len())).right_aligned().fg(DIM));

        if self.items.is_empty() {
            let mut text = vec![
                Line::from(""),
                Line::from(""),
                Line::from("⇩  Drag & drop songs or folders onto this window  ⇩").bold().fg(ACCENT),
                Line::from(""),
                Line::from("or press  o  to type a path (e.g. ~/Music/Some Album)").fg(Color::Gray),
                Line::from(""),
                Line::from("FLAC, WAV, AIFF, ALAC, Ogg, Opus, APE, WavPack, MP3 and AAC are all fine —").fg(DIM),
                Line::from("anything the iPod can't play is converted for you.").fg(DIM),
            ];
            if self.scanning > 0 {
                text.push(Line::from(""));
                text.push(Line::from("Reading tags…").fg(Color::Yellow));
            }
            f.render_widget(Paragraph::new(text).centered().block(block), area);
            return;
        }

        let s = &self.settings;
        let rows: Vec<Row> = self
            .items
            .iter()
            .map(|it| {
                let action = it.action(s);
                let check = if it.enabled { "✓" } else { " " };
                let (act_text, act_color) = match &action {
                    Action::Copy => ("copy".to_string(), Color::Green),
                    Action::Convert(t) => (format!("→ {}", t.label()), Color::Yellow),
                    Action::Skip(why) => (format!("skip ({why})"), DIM),
                };
                let (st, st_color) = match &it.status {
                    Status::Pending => (String::new(), DIM),
                    Status::Working(w) => (format!("{w}…"), Color::Yellow),
                    Status::Done => ("✓ added".into(), Color::Green),
                    Status::Failed(e) => (format!("✗ {e}"), Color::Red),
                };
                let mut title = it.meta.title.clone();
                if !it.will_have_art(s) {
                    title.push_str(" ·");
                }
                let style = if matches!(action, Action::Skip(_)) { Style::new().fg(DIM) } else { Style::new() };
                Row::new(vec![
                    Cell::from(check).fg(ACCENT),
                    Cell::from(title),
                    Cell::from(it.meta.artist.clone()).fg(Color::Gray),
                    Cell::from(it.meta.album.clone()).fg(DIM),
                    Cell::from(it.format.clone()).fg(Color::Gray),
                    Cell::from(act_text).fg(act_color),
                    Cell::from(st).fg(st_color),
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Length(1),
            Constraint::Percentage(26),
            Constraint::Percentage(16),
            Constraint::Percentage(18),
            Constraint::Length(13),
            Constraint::Length(22),
            Constraint::Min(10),
        ];
        let header = Row::new(["", "Title", "Artist", "Album", "Source", "Action", "Status"])
            .style(Style::new().fg(DIM).add_modifier(Modifier::BOLD));
        let highlight = if focused {
            Style::new().bg(Color::Rgb(40, 70, 80)).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let table = Table::new(rows, widths).header(header).block(block).row_highlight_style(highlight).column_spacing(1);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn option_rows(&self) -> Vec<(&'static str, String)> {
        let s = &self.settings;
        vec![
            ("Lossless (FLAC, WAV, AIFF, APE)", s.lossless.label()),
            ("Ogg / Opus / Musepack", s.lossy.label()),
            (
                "Shrink MP3/AAC above",
                if s.shrink_above == 0 { "off — copy as-is".into() } else { format!("{} kbps → {}", s.shrink_above, s.lossy.label()) },
            ),
            ("Parallel conversions", s.jobs.to_string()),
            ("Skip songs already on iPod", if s.skip_duplicates { "yes" } else { "no" }.into()),
            ("Folder art (cover.jpg) fallback", if s.folder_art { "yes" } else { "no" }.into()),
        ]
    }

    fn draw_options(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Pane::Options;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
            .title(" Conversion ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = self
            .option_rows()
            .into_iter()
            .enumerate()
            .map(|(i, (k, v))| {
                let sel = focused && i == self.opt;
                let val = if sel { format!("‹ {v} ›") } else { format!("  {v}") };
                Line::from(vec![
                    Span::styled(format!(" {k:<32}"), Style::new().fg(if sel { Color::White } else { Color::Gray })),
                    Span::styled(val, if sel { Style::new().fg(ACCENT).bold() } else { Style::new().fg(ACCENT) }),
                ])
            })
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(" Hi-res (>48 kHz or 24-bit) → 16-bit/44.1 kHz, the iPod's limit.").fg(DIM));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn draw_summary(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Summary ");
        let s = &self.settings;
        let (mut copy, mut conv, mut skip) = (0, 0, 0);
        let mut no_art = 0;
        for it in &self.items {
            match it.action(s) {
                Action::Copy => copy += 1,
                Action::Convert(_) => conv += 1,
                Action::Skip(_) => skip += 1,
            }
            if !it.will_have_art(s) && !matches!(it.action(s), Action::Skip(_)) {
                no_art += 1;
            }
        }
        let size: u64 = self.items.iter().map(|i| i.estimated_size(s)).sum();
        let free = import::free_space(&self.root);
        let gb = |b: u64| if b >= 1_000_000_000 { format!("{:.2} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) };

        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!(" {} ", copy + conv), Style::new().bold().fg(Color::White)),
                Span::raw("to add  "),
                Span::styled(format!("{copy} copy  "), Style::new().fg(Color::Green)),
                Span::styled(format!("{conv} convert  "), Style::new().fg(Color::Yellow)),
                Span::styled(format!("{skip} skip"), Style::new().fg(DIM)),
            ]),
            Line::from(format!(" ≈ {} on the iPod", gb(size))),
        ];
        if let Some(free) = free {
            let after = free.saturating_sub(size);
            let color = if size > free { Color::Red } else { Color::Gray };
            lines.push(Line::from(format!(" Free: {} → {}", gb(free), gb(after))).fg(color));
        }
        if no_art > 0 {
            lines.push(Line::from(format!(" {no_art} without cover art (marked ·)")).fg(DIM));
        }
        if !self.ffmpeg && conv > 0 {
            lines.push(Line::from(" ffmpeg not found — install it to convert").fg(Color::Red));
        }
        if self.scanning > 0 {
            lines.push(Line::from(" Reading tags…").fg(Color::Yellow));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_progress(&self, f: &mut Frame, area: Rect) {
        let run = self.run.as_ref().unwrap();
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(" Progress ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge, text] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(inner);

        let ratio = if run.result.is_some() { 1.0 } else { run.done as f64 / run.total.max(1) as f64 };
        let color = match &run.result {
            Some(Err(_)) => Color::Red,
            Some(Ok(_)) => Color::Green,
            None => ACCENT,
        };
        let g = Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Rgb(30, 30, 40)))
            .ratio(ratio.min(1.0))
            .label(format!("{} / {}", run.done, run.total));
        f.render_widget(g, gauge.inner(ratatui::layout::Margin::new(1, 1)));

        let secs = run.started.elapsed().as_secs();
        let mut lines = vec![
            Line::from(format!(" {}", run.phase)).bold(),
            Line::from(format!(" {}:{:02} elapsed · {} failed", secs / 60, secs % 60, run.failed)).fg(DIM),
        ];
        if run.result.is_some() {
            lines.push(Line::from(""));
            lines.push(Line::from(" Press Enter to go back. Eject the iPod before unplugging it.").fg(ACCENT));
        }
        f.render_widget(Paragraph::new(lines), text);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = if let Some(input) = &self.path_input {
            Line::from(vec![
                Span::styled(" path ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                Span::raw(format!(" {input}▏")),
                Span::styled("   enter add · esc cancel", Style::new().fg(DIM)),
            ])
        } else if let Some(msg) = &self.message {
            Line::from(format!(" {msg}")).fg(Color::Yellow)
        } else if self.running() {
            Line::from(" Working… don't unplug the iPod.").fg(Color::Yellow)
        } else {
            let keys: &[(&str, &str)] = if self.focus == Pane::Options {
                &[("↑↓", "option"), ("←→", "change"), ("tab", "queue"), ("s", "start"), ("esc", "back")]
            } else {
                &[("drop", "add files"), ("o", "path"), ("space", "toggle"), ("a", "all"), ("d", "remove"), ("tab", "options"), ("enter", "start"), ("esc", "back")]
            };
            Line::from(
                keys.iter()
                    .flat_map(|(k, d)| [Span::styled(format!(" {k} "), Style::new().fg(ACCENT)), Span::styled(format!("{d} "), Style::new().fg(DIM))])
                    .collect::<Vec<_>>(),
            )
        };
        f.render_widget(line, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// `RPOD_TEST_SRC=/music/dir RPOD_TEST_IPOD=/ipod cargo test import_screen -- --nocapture`
    #[test]
    fn import_screen_snapshot() {
        let (Ok(src), Ok(root)) = (std::env::var("RPOD_TEST_SRC"), std::env::var("RPOD_TEST_IPOD")) else { return };
        let mut view = ImportView::new(root.into(), HashSet::new());
        let mut term = Terminal::new(TestBackend::new(150, 30)).unwrap();
        term.draw(|f| view.draw(f, f.area())).unwrap();
        println!("{}", text(term.backend().buffer()));
        view.items = import::scan(&import::expand(&[src.into()]), &HashSet::new());
        view.table.select(Some(0));
        view.focus = Pane::Options;
        term.draw(|f| view.draw(f, f.area())).unwrap();
        println!("{}", text(term.backend().buffer()));
    }

    fn text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
