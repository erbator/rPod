//! The download screen (`d` for a selection, `S` for everything): what will
//! be copied off the iPod and where to, then live progress while it runs.

use crate::export::{self, Planned, Progress, Step, Summary};
use crate::import::{self, Settings};
use crate::itunesdb::Track;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Gauge, Paragraph, Row, Table};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
/// Measured reading an iPod Video over USB 2.0, for the time estimate
/// before a run; during one the real rate is used.
const READ_BYTES_PER_SEC: f64 = 13e6;

enum State {
    Waiting,
    Working,
    Done,
    Warned(String),
    Failed(String),
}

struct Run {
    rx: Receiver<Progress>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    total: usize,
    done: usize,
    failed: usize,
    total_bytes: u64,
    done_bytes: u64,
    result: Option<Result<Summary, String>>,
}

pub struct DownloadView {
    root: PathBuf,
    title: String,
    tracks: Vec<Track>,
    videos: usize,
    dest: PathBuf,
    plan: Arc<Vec<Planned>>,
    states: Vec<State>,
    sel: usize,
    top: usize,
    /// The song being copied, and whether the list follows it. Scrolling
    /// away stops following; scrolling back onto it resumes.
    working: Option<usize>,
    follow: bool,
    path_input: Option<String>,
    message: Option<String>,
    run: Option<Run>,
    /// Set when the user leaves the screen.
    pub closed: bool,
}

impl DownloadView {
    pub fn new(root: PathBuf, title: String, tracks: Vec<Track>) -> Self {
        let (tracks, videos): (Vec<Track>, Vec<Track>) = tracks.into_iter().partition(|t| !export::is_video(t));
        let mut view = Self {
            root,
            title,
            tracks,
            videos: videos.len(),
            dest: Settings::load().download_dir,
            plan: Arc::new(Vec::new()),
            states: Vec::new(),
            sel: 0,
            top: 0,
            working: None,
            follow: true,
            path_input: None,
            message: None,
            run: None,
            closed: false,
        };
        view.replan();
        view
    }

    fn replan(&mut self) {
        self.plan = Arc::new(export::plan(&self.root, &self.dest, &self.tracks));
        self.states = self.plan.iter().map(|_| State::Waiting).collect();
        self.sel = 0;
        self.top = 0;
    }

    fn set_dest(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let path = match text.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map_or_else(|| PathBuf::from(text), |h| PathBuf::from(h).join(rest)),
            None => PathBuf::from(text),
        };
        if !path.is_absolute() {
            self.message = Some("Use a full path, like ~/Music or /mnt/usb/Music.".into());
            return;
        }
        if path.is_file() {
            self.message = Some("That's a file; pick a folder.".into());
            return;
        }
        let mut settings = Settings::load();
        settings.download_dir = path.clone();
        settings.save();
        self.dest = path;
        self.replan();
        self.message = Some(format!("Downloading to {}.", tilde(&self.dest)));
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(input) = &mut self.path_input {
            input.push_str(text.trim());
        } else if self.run.is_none() {
            match import::parse_dropped(text).into_iter().find(|p| p.is_dir()) {
                Some(dir) => self.set_dest(&dir.to_string_lossy()),
                None => self.message = Some("Drop a folder here to download into it.".into()),
            }
        }
    }

    fn count(&self, step: Step) -> usize {
        self.plan.iter().filter(|p| p.step == step).count()
    }

    fn copy_bytes(&self) -> u64 {
        self.plan.iter().filter(|p| p.step == Step::Copy).map(|p| p.track.size as u64).sum()
    }

    fn start(&mut self) {
        let total = self.count(Step::Copy) + self.count(Step::Update);
        if total == 0 {
            self.message = Some("Nothing to do: these songs are already on your PC.".into());
            return;
        }
        let needed = self.copy_bytes();
        if free_space(&self.dest).is_some_and(|free| needed > free) {
            self.message = Some(format!("Not enough free space in {}.", tilde(&self.dest)));
            return;
        }
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (root, dest, plan, c) = (self.root.clone(), self.dest.clone(), self.plan.clone(), cancel.clone());
        std::thread::spawn(move || export::run(root, dest, plan, c, tx));
        self.run = Some(Run {
            rx,
            cancel,
            started: Instant::now(),
            total,
            done: 0,
            failed: 0,
            total_bytes: needed,
            done_bytes: 0,
            result: None,
        });
    }

    /// Drain progress. Returns true if anything changed.
    pub fn tick(&mut self) -> bool {
        let Some(run) = &mut self.run else { return false };
        let mut changed = false;
        while let Ok(p) = run.rx.try_recv() {
            changed = true;
            match p {
                Progress::Started(i) => {
                    self.states[i] = State::Working;
                    self.working = Some(i);
                    if self.follow {
                        self.sel = i;
                    }
                }
                Progress::Done(i, warning) => {
                    run.done += 1;
                    if self.plan[i].step == Step::Copy {
                        run.done_bytes += self.plan[i].track.size as u64;
                    }
                    self.states[i] = warning.map_or(State::Done, State::Warned);
                }
                Progress::Failed(i, e) => {
                    run.done += 1;
                    run.failed += 1;
                    self.states[i] = State::Failed(e);
                }
                Progress::Finished(r) => run.result = Some(r),
            }
        }
        // The elapsed/remaining time ticks along between songs.
        changed || run.result.is_none()
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.message = None;
        if let Some(input) = &mut self.path_input {
            match key.code {
                KeyCode::Enter => {
                    let text = std::mem::take(input);
                    self.path_input = None;
                    self.set_dest(&text);
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
            match (&run.result, key.code) {
                (Some(_), KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) => self.closed = true,
                (None, KeyCode::Esc | KeyCode::Char('q')) => {
                    run.cancel.store(true, Ordering::Relaxed);
                    self.message = Some("Stopping after this song…".into());
                }
                _ => self.scroll(key),
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.closed = true,
            KeyCode::Enter | KeyCode::Char('s') => self.start(),
            KeyCode::Char('o') => self.path_input = Some(tilde(&self.dest)),
            _ => self.scroll(key),
        }
    }

    fn scroll(&mut self, key: KeyEvent) {
        let last = self.plan.len().saturating_sub(1);
        self.sel = match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.sel + 1,
            KeyCode::Up | KeyCode::Char('k') => self.sel.saturating_sub(1),
            KeyCode::PageDown => self.sel + 20,
            KeyCode::PageUp => self.sel.saturating_sub(20),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => self.sel,
        }
        .min(last);
        if self.working.is_some() {
            self.follow = self.working == Some(self.sel);
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let [top, list, footer] =
            Layout::vertical([Constraint::Length(7), Constraint::Min(3), Constraint::Length(1)]).areas(area);
        if self.run.is_some() {
            self.draw_progress(f, top);
        } else {
            self.draw_summary(f, top);
        }
        self.draw_list(f, list);
        self.draw_footer(f, footer);
    }

    fn draw_summary(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(format!(" {} ", self.title));
        let bytes = self.copy_bytes();
        let (copy, update, have, taken) =
            (self.count(Step::Copy), self.count(Step::Update), self.count(Step::Have), self.count(Step::Taken));
        let mut counts = vec![
            Span::styled(format!(" {copy} "), Style::new().bold().fg(Color::White)),
            Span::raw("to copy  "),
            Span::styled(format!("{update} to update  "), Style::new().fg(Color::Yellow)),
            Span::styled(format!("{have} already on PC"), Style::new().fg(DIM)),
        ];
        if taken > 0 {
            counts.push(Span::styled(format!("  {taken} skipped (another file in the way)"), Style::new().fg(Color::Red)));
        }
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" To ", Style::new().fg(DIM)),
                Span::styled(tilde(&self.dest), Style::new().fg(ACCENT).bold()),
                Span::styled("  (o to change)", Style::new().fg(DIM)),
            ]),
            Line::from(counts),
        ];
        if copy > 0 {
            let secs = (bytes as f64 / READ_BYTES_PER_SEC) as u64;
            lines.push(Line::from(format!(" {} to copy, about {}", size(bytes), duration(secs))));
        }
        if let Some(free) = free_space(&self.dest) {
            let color = if bytes > free { Color::Red } else { Color::Gray };
            lines.push(Line::from(format!(" Free: {} → {}", size(free), size(free.saturating_sub(bytes)))).fg(color));
        }
        if self.videos > 0 {
            lines.push(Line::from(format!(" {} video(s) stay on the iPod.", self.videos)).fg(DIM));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_progress(&self, f: &mut Frame, area: Rect) {
        let run = self.run.as_ref().unwrap();
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(format!(" {} ", self.title));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge, text] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(inner);

        let color = match &run.result {
            Some(Err(_)) => Color::Red,
            Some(Ok(_)) => Color::Green,
            None => ACCENT,
        };
        let ratio = if run.result.is_some() { 1.0 } else { run.done as f64 / run.total.max(1) as f64 };
        let g = Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Rgb(30, 30, 40)))
            .ratio(ratio.min(1.0))
            .label(format!("{} / {}", run.done, run.total));
        f.render_widget(g, gauge.inner(Margin::new(1, 1)));

        let elapsed = run.started.elapsed().as_secs_f64();
        let mut lines = Vec::new();
        match &run.result {
            None => {
                let mut status = format!(" {} of {} · {} elapsed", size(run.done_bytes), size(run.total_bytes), duration(elapsed as u64));
                if run.done_bytes > 0 && elapsed > 3.0 {
                    let left = run.total_bytes.saturating_sub(run.done_bytes) as f64 / (run.done_bytes as f64 / elapsed);
                    status += &format!(" · about {} left", duration(left as u64));
                }
                lines.push(Line::from(status));
                lines.push(Line::from(format!(" Copying to {} · {} failed", tilde(&self.dest), run.failed)).fg(DIM));
            }
            Some(Ok(s)) => {
                let verb = if s.cancelled { "Stopped" } else { "Done" };
                lines.push(Line::from(format!(" {verb}: copied {}, updated {}, {} failed.", s.copied, s.updated, s.failed)).bold());
                lines.push(Line::from(" Press Enter to go back.").fg(ACCENT));
            }
            Some(Err(e)) => {
                lines.push(Line::from(format!(" Download failed: {e}")).fg(Color::Red));
                lines.push(Line::from(" Press Enter to go back.").fg(ACCENT));
            }
        }
        f.render_widget(Paragraph::new(lines), text);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Songs ");
        let height = block.inner(area).height as usize;
        // Only the visible rows are built: a whole library is thousands.
        if self.sel < self.top {
            self.top = self.sel;
        } else if height > 0 && self.sel >= self.top + height {
            self.top = self.sel + 1 - height;
        }
        let rows: Vec<Row> = self
            .plan
            .iter()
            .zip(&self.states)
            .enumerate()
            .skip(self.top)
            .take(height)
            .map(|(i, (p, state))| {
                let (label, color) = match (state, p.step) {
                    (State::Working, _) => ("copying", ACCENT),
                    (State::Done, _) => ("✓ done", Color::Green),
                    (State::Warned(_), _) => ("✓ no tags", Color::Yellow),
                    (State::Failed(_), _) => ("✗ failed", Color::Red),
                    (State::Waiting, Step::Copy) => ("copy", Color::White),
                    (State::Waiting, Step::Update) => ("update", Color::Yellow),
                    (State::Waiting, Step::Have) => ("on PC", DIM),
                    (State::Waiting, Step::Taken) => ("in the way", Color::Red),
                };
                let mut path = p.rel.to_string_lossy().into_owned();
                if let State::Warned(e) | State::Failed(e) = state {
                    path = format!("{path}  — {e}");
                }
                let row = Row::new([Cell::from(label).fg(color), Cell::from(path)]);
                if i == self.sel { row.style(Style::new().bg(Color::Rgb(40, 44, 60))) } else { row }
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(11), Constraint::Min(0)]).block(block);
        f.render_widget(table, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = if let Some(input) = &self.path_input {
            Line::from(vec![
                Span::styled(" folder ", Style::new().fg(Color::Black).bg(Color::Yellow)),
                Span::raw(format!(" {input}▏")),
                Span::styled("   enter set · esc cancel", Style::new().fg(DIM)),
            ])
        } else if let Some(msg) = &self.message {
            Line::from(format!(" {msg}")).fg(Color::Yellow)
        } else {
            let keys: &[(&str, &str)] = match &self.run {
                Some(r) if r.result.is_none() => &[("esc", "stop"), ("↑↓", "scroll")],
                Some(_) => &[("enter", "back")],
                None => &[("enter", "start"), ("o", "folder"), ("drop", "folder"), ("↑↓", "scroll"), ("esc", "back")],
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

/// Free space where `dest` is or will be (it may not exist yet).
fn free_space(dest: &Path) -> Option<u64> {
    dest.ancestors().find(|p| p.exists()).and_then(import::free_space)
}

fn tilde(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn size(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.1} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs.div_ceil(60)),
        _ => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// `RPOD_TEST_IPOD=/ipod RPOD_TEST_DEST=/tmp/out cargo test download_screen -- --nocapture`
    #[test]
    fn download_screen_snapshot() {
        let (Ok(root), Ok(dest)) = (std::env::var("RPOD_TEST_IPOD"), std::env::var("RPOD_TEST_DEST")) else { return };
        let ipod = crate::device::Ipod::open(Path::new(&root)).unwrap();
        let mut view = DownloadView::new(root.into(), "Sync all music to PC".into(), ipod.db.tracks.clone());
        view.dest = dest.into();
        view.replan();
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        term.draw(|f| view.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            println!("{}", (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>());
        }
    }
}
