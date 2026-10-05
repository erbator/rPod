//! "Fix missing covers": find covers for every album without art, review the
//! matches, and apply the accepted ones in one write.

use crate::covers::{self, Assignment};
use crate::edit::Report;
use crate::itunes::{self, AlbumHit};
use crate::itunesdb::Track;
use crate::widgets;
use image::DynamicImage;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Gauge, Paragraph, Row, Table, TableState, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

#[derive(PartialEq)]
enum State {
    Queued,
    Searching,
    /// Best match found; `bool` = confident.
    Found(bool),
    NoMatch,
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Decision {
    Undecided,
    Accept,
    Skip,
    /// Fixed one-off through the cover picker.
    Done,
}

pub struct AlbumRow {
    pub title: String,
    pub artist: String,
    pub tracks: Vec<Track>,
    state: State,
    hit: Option<AlbumHit>,
    score: f32,
    preview: Option<StatefulProtocol>,
    decision: Decision,
}

impl AlbumRow {
    pub fn new(title: String, artist: String, tracks: Vec<Track>) -> Self {
        Self {
            title,
            artist,
            tracks,
            state: State::Queued,
            hit: None,
            score: 0.0,
            preview: None,
            decision: Decision::Undecided,
        }
    }
}

enum Msg {
    Searching(usize),
    Found(usize, Result<Option<(AlbumHit, f32, bool)>, String>),
    Preview(usize, DynamicImage),
    Progress(String),
    /// What was written, and the rows whose cover couldn't be downloaded.
    Applied(Result<(Report, Vec<(usize, String)>), String>),
}

pub struct FixView {
    root: PathBuf,
    rows: Vec<AlbumRow>,
    table: TableState,
    picker: Picker,
    /// The saved "embed covers in song files" setting; the app refreshes it
    /// when the cover picker (which can toggle it too) closes.
    pub write_files: bool,
    /// Covers already written while some albums failed to download.
    applied: Option<Report>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    started: Instant,
    applying: Option<(Instant, String)>,
    message: Option<String>,
    /// The user wants the full picker for this row's album.
    pub wants_picker: Option<usize>,
    pub closed: Option<Option<Report>>,
}

impl FixView {
    pub fn new(root: PathBuf, rows: Vec<AlbumRow>, picker: Picker, write_files: bool) -> Self {
        let (tx, rx) = channel();
        let country = itunes::default_country();
        let jobs: Vec<itunes::Wanted> = rows.iter().map(|r| itunes::Wanted::from_tracks(&r.tracks)).collect();
        let worker = tx.clone();
        // One worker: searches are paced to Apple's rate limit anyway.
        std::thread::spawn(move || {
            for (i, wanted) in jobs.into_iter().enumerate() {
                if worker.send(Msg::Searching(i)).is_err() {
                    return; // screen closed
                }
                // Stop after the first search that gives a confident match.
                let res = itunes::find(&wanted, &country, |all| itunes::best_match(all, &wanted).is_some_and(|m| m.2)).map(|hits| {
                    itunes::best_match(&hits, &wanted).map(|(b, score, confident)| (hits[b].clone(), score, confident))
                });
                let preview = res.as_ref().ok().and_then(|m| m.as_ref()).map(|(h, _, _)| h.preview_url());
                worker.send(Msg::Found(i, res.map_err(|e| format!("{e:#}")))).ok();
                if let Some(img) = preview.and_then(|u| itunes::download(&u).ok()).and_then(|b| image::load_from_memory(&b).ok()) {
                    worker.send(Msg::Preview(i, img.thumbnail(400, 400))).ok();
                }
            }
        });
        let mut table = TableState::default();
        table.select((!rows.is_empty()).then_some(0));
        Self {
            root,
            rows,
            table,
            picker,
            write_files,
            applied: None,
            rx,
            tx,
            started: Instant::now(),
            applying: None,
            message: None,
            wants_picker: None,
            closed: None,
        }
    }

    /// The cover picker fixed this album directly.
    pub fn mark_done(&mut self, i: usize) {
        if let Some(r) = self.rows.get_mut(i) {
            r.decision = Decision::Done;
        }
    }

    pub fn row_tracks(&self, i: usize) -> Option<(&str, &[Track])> {
        self.rows.get(i).map(|r| (r.title.as_str(), r.tracks.as_slice()))
    }

    fn searching_done(&self) -> bool {
        self.rows.iter().all(|r| !matches!(r.state, State::Queued | State::Searching))
    }

    pub fn tick(&mut self) -> bool {
        let mut changed = self.applying.is_some() || !self.searching_done();
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Searching(i) => self.rows[i].state = State::Searching,
                Msg::Found(i, Ok(Some((hit, score, confident)))) => {
                    let r = &mut self.rows[i];
                    r.hit = Some(hit);
                    r.score = score;
                    r.state = State::Found(confident);
                }
                Msg::Found(i, Ok(None)) => self.rows[i].state = State::NoMatch,
                Msg::Found(i, Err(e)) => self.rows[i].state = State::Failed(e),
                Msg::Preview(i, img) => self.rows[i].preview = Some(self.picker.new_resize_protocol(img)),
                Msg::Progress(s) => {
                    if let Some((_, text)) = &mut self.applying {
                        *text = s;
                    }
                }
                Msg::Applied(Ok((report, failed))) => {
                    self.applying = None;
                    let report = match self.applied.take() {
                        Some(mut before) => {
                            before.tracks += report.tracks;
                            before.file_errors.extend(report.file_errors);
                            before
                        }
                        None => report,
                    };
                    if failed.is_empty() {
                        self.closed = Some(Some(report));
                        continue;
                    }
                    // The rest were written; keep the failures on screen to retry or pick by hand.
                    for r in self.rows.iter_mut().filter(|r| r.decision == Decision::Accept) {
                        r.decision = Decision::Done;
                    }
                    for (i, e) in &failed {
                        self.rows[*i].decision = Decision::Undecided;
                        self.rows[*i].state = State::Failed(format!("Cover download failed: {e}"));
                    }
                    let written = if report.tracks > 0 { format!("Covers added to {} track(s), but ", report.tracks) } else { String::new() };
                    self.message = Some(format!(
                        "{written}{} album(s) failed to download (marked error): y retries, enter picks one by hand.",
                        failed.len()
                    ));
                    self.applied = (report.tracks > 0).then_some(report);
                }
                Msg::Applied(Err(e)) => {
                    self.applying = None;
                    self.message = Some(format!("Couldn't apply covers: {e}"));
                }
            }
        }
        changed
    }

    fn apply(&mut self) {
        let accepted: Vec<(usize, AlbumHit, Vec<Track>)> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.decision == Decision::Accept)
            .filter_map(|(i, r)| Some((i, r.hit.clone()?, r.tracks.clone())))
            .collect();
        if accepted.is_empty() {
            self.message = Some("Nothing accepted yet: y accepts the selected album, a accepts every confident match.".into());
            return;
        }
        let (tx, root, write_files) = (self.tx.clone(), self.root.clone(), self.write_files);
        let n = accepted.len();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(Report, Vec<(usize, String)>)> {
                // Downloads run in parallel; only the iPod writes are sequential.
                use rayon::prelude::*;
                // Counting and sending under one lock keeps the progress text in order.
                let done = std::sync::Mutex::new(0);
                tx.send(Msg::Progress(format!("Downloading {n} covers…"))).ok();
                let pool = rayon::ThreadPoolBuilder::new().num_threads(6).build()?;
                let fetched: Vec<(usize, Vec<Track>, anyhow::Result<Vec<u8>>)> = pool.install(|| {
                    accepted
                        .into_par_iter()
                        .map(|(i, hit, tracks)| {
                            let image = itunes::fetch_cover(&hit);
                            let mut d = done.lock().unwrap();
                            *d += 1;
                            tx.send(Msg::Progress(format!("Downloaded {d}/{n} covers"))).ok();
                            (i, tracks, image)
                        })
                        .collect()
                });
                // One bad cover URL shouldn't sink the rest of the batch.
                let mut jobs = Vec::new();
                let mut failed = Vec::new();
                for (i, tracks, image) in fetched {
                    match image {
                        Ok(image) => jobs.push(Assignment { tracks, image }),
                        Err(e) => failed.push((i, format!("{e:#}"))),
                    }
                }
                if jobs.is_empty() {
                    return Ok((Report { tracks: 0, file_errors: Vec::new() }, failed));
                }
                tx.send(Msg::Progress("Writing to the iPod…".into())).ok();
                Ok((covers::apply(&root, &jobs, write_files)?, failed))
            })();
            tx.send(Msg::Applied(res.map_err(|e| format!("{e:#}")))).ok();
        });
        self.applying = Some((Instant::now(), "Starting…".into()));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.applying.is_some() {
            return;
        }
        self.message = None;
        let n = self.rows.len();
        let sel = self.table.selected().unwrap_or(0);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut decide = |d: Decision| {
            if let Some(r) = self.rows.get_mut(sel) {
                if r.hit.is_some() && r.decision != Decision::Done {
                    r.decision = if r.decision == d { Decision::Undecided } else { d };
                }
            }
        };
        match key.code {
            KeyCode::Char('s') if ctrl => self.apply(),
            // Covers already written still need the library reloaded.
            KeyCode::Esc | KeyCode::Char('q') => self.closed = Some(self.applied.take()),
            KeyCode::Char('y') => {
                decide(Decision::Accept);
                self.table.select(Some((sel + 1).min(n.saturating_sub(1))));
            }
            KeyCode::Char('n') => {
                decide(Decision::Skip);
                self.table.select(Some((sel + 1).min(n.saturating_sub(1))));
            }
            KeyCode::Char('a') => {
                for r in &mut self.rows {
                    if r.state == State::Found(true) && r.decision == Decision::Undecided {
                        r.decision = Decision::Accept;
                    }
                }
            }
            KeyCode::Char('f') => self.write_files = covers::toggle_embed_covers(),
            KeyCode::Enter => self.wants_picker = Some(sel),
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.table.select(Some((sel + 1).min(n - 1))),
            KeyCode::Up | KeyCode::Char('k') => self.table.select(Some(sel.saturating_sub(1))),
            KeyCode::PageDown if n > 0 => self.table.select(Some((sel + 15).min(n - 1))),
            KeyCode::PageUp => self.table.select(Some(sel.saturating_sub(15))),
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let [main, bottom, footer] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(4), Constraint::Length(1)]).areas(area);
        let [list, detail] = Layout::horizontal([Constraint::Min(40), Constraint::Length(44)]).areas(main);

        let rows: Vec<Row> = self
            .rows
            .iter()
            .map(|r| {
                let (status, color) = match (&r.state, r.decision) {
                    (_, Decision::Done) => ("✓ fixed".to_string(), Color::Green),
                    (_, Decision::Accept) => ("✓ accepted".to_string(), Color::Green),
                    (_, Decision::Skip) => ("– skipped".to_string(), DIM),
                    (State::Queued, _) => ("queued".to_string(), DIM),
                    (State::Searching, _) => ("searching…".to_string(), Color::Yellow),
                    (State::Found(true), _) => (format!("confident {:.0}%", r.score * 100.0), ACCENT),
                    (State::Found(false), _) => (format!("review {:.0}%", r.score * 100.0), Color::Yellow),
                    (State::NoMatch, _) => ("no match".to_string(), Color::Red),
                    (State::Failed(_), _) => ("error".to_string(), Color::Red),
                };
                let proposal = r.hit.as_ref().map(|h| format!("{} — {}", h.artist, h.album)).unwrap_or_default();
                Row::new(vec![
                    Cell::from(r.title.clone()),
                    Cell::from(r.artist.clone()).fg(Color::Gray),
                    Cell::from(r.tracks.len().to_string()).fg(DIM),
                    Cell::from(status).fg(color),
                    Cell::from(proposal).fg(DIM),
                ])
            })
            .collect();
        let accepted = self.rows.iter().filter(|r| r.decision == Decision::Accept).count();
        let table = Table::new(
            rows,
            [Constraint::Percentage(26), Constraint::Percentage(18), Constraint::Length(3), Constraint::Length(14), Constraint::Min(10)],
        )
        .header(Row::new(["Album", "Artist", "#", "Status", "Proposed"]).style(Style::new().fg(DIM).bold()))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(" Albums without covers ")
                .title_bottom(Line::from(format!(" {accepted} accepted · {} albums ", self.rows.len())).right_aligned().fg(DIM)),
        )
        .row_highlight_style(Style::new().bg(Color::Rgb(40, 70, 80)).add_modifier(Modifier::BOLD))
        .column_spacing(1);
        f.render_stateful_widget(table, list, &mut self.table);

        self.draw_detail(f, detail);
        self.draw_progress(f, bottom);

        let line = if let Some(m) = &self.message {
            Line::from(format!(" {m}")).fg(Color::Yellow)
        } else {
            let keys = [
                ("y", "accept"),
                ("n", "skip"),
                ("a", "accept all confident"),
                ("enter", "pick manually"),
                ("ctrl+s", "apply accepted"),
                ("f", if self.write_files { "embedding in files: on" } else { "embed in files" }),
                ("esc", "back"),
            ];
            widgets::key_hints(&keys)
        };
        f.render_widget(line, footer);
    }

    fn draw_detail(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Proposed cover ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(r) = self.table.selected().and_then(|i| self.rows.get_mut(i)) else { return };
        let fs = self.picker.font_size();
        let img_rows = ((inner.width as u32 * fs.width as u32) / fs.height.max(1) as u32) as u16;
        let [img, text] = Layout::vertical([Constraint::Length(img_rows.min(inner.height / 2 + 4)), Constraint::Min(0)]).areas(inner);
        match &mut r.preview {
            Some(p) => f.render_stateful_widget(StatefulImage::default().resize(Resize::Scale(None)), img, p),
            None => f.render_widget(Paragraph::new("\n\n\n   no preview yet").fg(DIM), img),
        }
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled("Your album", Style::new().fg(DIM))),
            Line::from(format!("{} — {}", r.artist, r.title)).bold(),
            Line::from(format!("{} track(s)", r.tracks.len())).fg(DIM),
            Line::from(""),
        ];
        match (&r.hit, &r.state) {
            (Some(h), state) => {
                lines.push(Line::from(Span::styled("iTunes match", Style::new().fg(DIM))));
                lines.push(Line::from(format!("{} — {}", h.artist, h.album)).fg(ACCENT));
                lines.push(Line::from(format!("{} · {} tracks · match {:.0}%", h.year, h.tracks, r.score * 100.0)).fg(DIM));
                if let State::Failed(e) = state {
                    lines.push(Line::from(e.clone()).fg(Color::Red));
                }
            }
            (None, State::Failed(e)) => lines.push(Line::from(e.clone()).fg(Color::Red)),
            (None, State::NoMatch) => lines.push(Line::from("No match. Press enter to search manually.").fg(Color::Yellow)),
            _ => {}
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), text);
    }

    fn draw_progress(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge, text] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        if let Some((t, msg)) = &self.applying {
            let spin = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][(t.elapsed().as_millis() / 80) as usize % 10];
            f.render_widget(Line::from(format!(" {spin} {msg}")).fg(ACCENT), gauge);
            return;
        }
        let done = self.rows.iter().filter(|r| !matches!(r.state, State::Queued | State::Searching)).count();
        let total = self.rows.len().max(1);
        let g = Gauge::default()
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Rgb(30, 30, 40)))
            .ratio(done as f64 / total as f64)
            .label(format!("searched {done}/{}", self.rows.len()));
        f.render_widget(g, gauge);
        let confident = self.rows.iter().filter(|r| r.state == State::Found(true)).count();
        let review = self.rows.iter().filter(|r| r.state == State::Found(false)).count();
        let remaining = self.rows.len() - done;
        let eta = if remaining == 0 {
            format!("done in {}s", self.started.elapsed().as_secs())
        } else {
            format!("about {}s left (Apple allows ~20 searches/min)", remaining * 3)
        };
        f.render_widget(
            Line::from(format!(" {confident} confident · {review} to review · {eta}")).fg(DIM),
            text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Ipod;
    use crate::library::Index;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn failed_downloads_stay_for_retry() {
        // No rows at creation, so no searches start; rows are set up by hand.
        let mut v = FixView::new(PathBuf::new(), Vec::new(), Picker::halfblocks(), false);
        v.rows = ["A", "B", "C"].iter().map(|t| AlbumRow::new(t.to_string(), "X".into(), Vec::new())).collect();
        v.rows[0].decision = Decision::Accept;
        v.rows[1].decision = Decision::Accept;
        v.rows[2].decision = Decision::Skip;
        let report = |tracks| Report { tracks, file_errors: Vec::new() };

        v.tx.send(Msg::Applied(Ok((report(10), vec![(1, "404".into())])))).unwrap();
        v.tick();
        assert!(v.closed.is_none(), "stays open to retry the failure");
        assert!(v.rows[0].decision == Decision::Done && v.rows[2].decision == Decision::Skip);
        assert!(v.rows[1].decision == Decision::Undecided && matches!(v.rows[1].state, State::Failed(_)));

        // Retrying it succeeds: the screen closes reporting both batches.
        v.rows[1].decision = Decision::Accept;
        v.tx.send(Msg::Applied(Ok((report(4), Vec::new())))).unwrap();
        v.tick();
        assert_eq!(v.closed.as_ref().and_then(|r| r.as_ref()).map(|r| r.tracks), Some(14));
    }

    #[test]
    fn leaving_reloads_only_if_covers_were_written() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        for (written, failed, reload) in [(3, vec![(1, "404".to_string())], Some(3)), (0, vec![(0, "404".into()), (1, "404".into())], None)] {
            let mut v = FixView::new(PathBuf::new(), Vec::new(), Picker::halfblocks(), false);
            v.rows = ["A", "B"].iter().map(|t| AlbumRow::new(t.to_string(), "X".into(), Vec::new())).collect();
            v.rows.iter_mut().for_each(|r| r.decision = Decision::Accept);
            v.tx.send(Msg::Applied(Ok((Report { tracks: written, file_errors: Vec::new() }, failed)))).unwrap();
            v.tick();
            v.on_key(esc);
            assert_eq!(v.closed.as_ref().map(|r| r.as_ref().map(|r| r.tracks)), Some(reload));
        }
    }

    /// Live searches for real albums without art:
    /// `RPOD_TEST_IPOD=… cargo test -- --ignored fix_live --nocapture`
    #[test]
    #[ignore]
    fn fix_live() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let ipod = Ipod::open(std::path::Path::new(&root)).unwrap();
        let index = Index::build(&ipod.db.tracks);
        let rows: Vec<AlbumRow> = index
            .albums
            .iter()
            .filter_map(|a| {
                let missing: Vec<Track> = a
                    .tracks
                    .iter()
                    .map(|&t| ipod.db.tracks[t].clone())
                    .filter(|t| !ipod.art.by_track.contains_key(&t.dbid))
                    .collect();
                (!missing.is_empty()).then(|| AlbumRow::new(a.title.clone(), a.artist.clone(), missing))
            })
            .collect();
        println!("{} albums without covers", rows.len());
        let mut v = FixView::new(root.into(), rows.into_iter().take(5).collect(), Picker::halfblocks(), false);
        let start = Instant::now();
        while !v.searching_done() && start.elapsed().as_secs() < 60 {
            v.tick();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
        v.tick();
        let mut term = Terminal::new(TestBackend::new(140, 22)).unwrap();
        term.draw(|f| v.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            println!("{}", (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>());
        }
    }
}
