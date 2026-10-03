//! The cover picker pop-up: search iTunes, show results as a grid of real
//! cover images, apply one to a set of tracks. A dropped image file works too.

use crate::covers::{self, Assignment};
use crate::edit::Report;
use crate::itunes::{self, AlbumHit};
use crate::itunesdb::Track;
use crate::widgets::Input;
use image::DynamicImage;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Instant;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const CARD_W: u16 = 24;
const CARD_H: u16 = 14;

enum Preview {
    Loading,
    Ready(StatefulProtocol),
    Failed,
}

enum Source {
    Itunes(AlbumHit),
    /// An image file the user dropped in.
    Local(PathBuf, Vec<u8>),
}

struct Card {
    source: Source,
    score: f32,
    preview: Preview,
}

enum Msg {
    Results(u64, Result<Vec<AlbumHit>, String>),
    Preview(u64, usize, Option<DynamicImage>),
    Applied(Result<(Report, Vec<u8>), String>),
}

pub struct CoverPicker {
    root: PathBuf,
    title: String,
    tracks: Vec<Track>,
    artist: String,
    album: String,
    query: Input,
    editing_query: bool,
    country: String,
    other_country: String,
    cards: Vec<Card>,
    sel: usize,
    scroll: usize,
    /// Columns in the last drawn grid, for ↑↓ navigation.
    cols: usize,
    searching: Option<Instant>,
    /// Bumped per search so late results from an older search are ignored.
    generation: u64,
    applying: Option<Instant>,
    message: Option<String>,
    picker: Picker,
    write_files: bool,
    /// Just return the chosen image instead of writing it to the iPod
    /// (used by the Add music queue, before the songs are on the iPod).
    pub choose_only: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Set when the picker closes: `Some((report, image))` after applying.
    pub closed: Option<Option<(Report, Vec<u8>)>>,
}

impl CoverPicker {
    pub fn new(root: PathBuf, title: String, tracks: Vec<Track>, picker: Picker, write_files: bool) -> Self {
        let first = &tracks[0];
        let artist = if first.album_artist.is_empty() { first.artist.clone() } else { first.album_artist.clone() };
        let album = first.album.clone();
        let country = itunes::default_country();
        let other_country = if country == "us" { "gb".into() } else { "us".into() };
        let (tx, rx) = channel();
        let mut p = Self {
            root,
            title,
            tracks,
            query: Input::new(&format!("{artist} {album}").trim().to_string()),
            artist,
            album,
            editing_query: false,
            country,
            other_country,
            cards: Vec::new(),
            sel: 0,
            scroll: 0,
            cols: 1,
            searching: None,
            generation: 0,
            applying: None,
            message: None,
            picker,
            write_files,
            choose_only: false,
            tx,
            rx,
            closed: None,
        };
        p.search();
        p
    }

    fn search(&mut self) {
        let term = self.query.text();
        if term.trim().is_empty() {
            return;
        }
        self.generation += 1;
        self.cards.retain(|c| matches!(c.source, Source::Local(..)));
        self.sel = 0;
        self.scroll = 0;
        self.searching = Some(Instant::now());
        let (tx, generation, country) = (self.tx.clone(), self.generation, self.country.clone());
        std::thread::spawn(move || {
            let res = itunes::search(&term, &country).map_err(|e| format!("{e:#}"));
            let previews: Vec<String> = res.as_ref().map(|h| h.iter().map(AlbumHit::preview_url).collect()).unwrap_or_default();
            tx.send(Msg::Results(generation, res)).ok();
            // Previews download in parallel; results arrive in score order later.
            use rayon::prelude::*;
            previews.par_iter().enumerate().for_each(|(i, url)| {
                let img = itunes::download(url).ok().and_then(|b| image::load_from_memory(&b).ok());
                tx.send(Msg::Preview(generation, i, img.map(|i| i.thumbnail(300, 300)))).ok();
            });
        });
    }

    /// Image files dropped onto the picker become a "your image" card.
    pub fn on_paste(&mut self, text: &str) {
        if self.editing_query {
            self.query.insert(&text.replace(['\n', '\r'], " "));
            return;
        }
        for path in crate::import::parse_dropped(text) {
            match std::fs::read(&path).ok().filter(|b| image::guess_format(b).is_ok()) {
                Some(bytes) => {
                    let preview = image::load_from_memory(&bytes)
                        .map(|img| Preview::Ready(self.picker.new_resize_protocol(img.thumbnail(300, 300))))
                        .unwrap_or(Preview::Failed);
                    self.cards.insert(0, Card { source: Source::Local(path, bytes), score: 1.0, preview });
                    self.sel = 0;
                    self.scroll = 0;
                }
                None => self.message = Some(format!("{} isn't an image", path.display())),
            }
        }
    }

    /// Drain background results. Returns true if a redraw is needed.
    pub fn tick(&mut self) -> bool {
        let mut changed = self.searching.is_some() || self.applying.is_some();
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Results(g, _) | Msg::Preview(g, _, _) if g != self.generation => {}
                Msg::Results(_, Err(e)) => {
                    self.searching = None;
                    self.message = Some(format!("Search failed: {e}"));
                }
                Msg::Results(_, Ok(hits)) => {
                    self.searching = None;
                    if hits.is_empty() {
                        self.message = Some("No results. Try fewer words, or another store with tab.".into());
                    }
                    let n = self.tracks.len();
                    self.cards.extend(hits.into_iter().map(|h| {
                        let score = itunes::score(&h, &self.artist, &self.album, n);
                        Card { source: Source::Itunes(h), score, preview: Preview::Loading }
                    }));
                    if let Some((i, _)) = self.cards.iter().enumerate().max_by(|a, b| a.1.score.total_cmp(&b.1.score)) {
                        self.sel = i;
                    }
                }
                Msg::Preview(_, i, img) => {
                    // Previews are indexed in search-result order, which is
                    // the order the iTunes cards were appended in.
                    let offset = self.cards.iter().take_while(|c| matches!(c.source, Source::Local(..))).count();
                    if let Some(card) = self.cards.get_mut(offset + i) {
                        card.preview = match img {
                            Some(img) => Preview::Ready(self.picker.new_resize_protocol(img)),
                            None => Preview::Failed,
                        };
                    }
                }
                Msg::Applied(Ok(done)) => {
                    self.applying = None;
                    self.closed = Some(Some(done));
                }
                Msg::Applied(Err(e)) => {
                    self.applying = None;
                    self.message = Some(format!("Couldn't apply cover: {e}"));
                }
            }
        }
        changed
    }

    fn apply(&mut self) {
        let Some(card) = self.cards.get(self.sel) else { return };
        let (tx, root, tracks, write_files) = (self.tx.clone(), self.root.clone(), self.tracks.clone(), self.write_files);
        let choose_only = self.choose_only;
        let source = match &card.source {
            Source::Itunes(h) => Ok(h.clone()),
            Source::Local(_, b) => Err(b.clone()),
        };
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(Report, Vec<u8>)> {
                let bytes = match source {
                    Ok(hit) => itunes::fetch_cover(&hit)?,
                    Err(local) => local,
                };
                if choose_only {
                    return Ok((Report { tracks: 0, file_errors: Vec::new() }, bytes));
                }
                let report = covers::apply(&root, &[Assignment { tracks, image: bytes.clone() }], write_files)?;
                Ok((report, bytes))
            })();
            tx.send(Msg::Applied(res.map_err(|e| format!("{e:#}")))).ok();
        });
        self.applying = Some(Instant::now());
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.applying.is_some() {
            return;
        }
        self.message = None;
        if self.editing_query {
            match key.code {
                KeyCode::Enter => {
                    self.editing_query = false;
                    self.search();
                }
                KeyCode::Esc => self.editing_query = false,
                _ => {
                    self.query.key(key);
                }
            }
            return;
        }
        let n = self.cards.len();
        let cols = self.cols.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.closed = Some(None),
            KeyCode::Char('/') | KeyCode::Char('s') => self.editing_query = true,
            KeyCode::Tab => {
                std::mem::swap(&mut self.country, &mut self.other_country);
                self.search();
            }
            KeyCode::Enter if n > 0 => self.apply(),
            KeyCode::Right | KeyCode::Char('l') if n > 0 => self.sel = (self.sel + 1).min(n - 1),
            KeyCode::Left | KeyCode::Char('h') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.sel = (self.sel + cols).min(n - 1),
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(cols),
            KeyCode::Char('b') => {
                // Jump to the best-scoring result.
                if let Some((i, _)) = self.cards.iter().enumerate().max_by(|a, b| a.1.score.total_cmp(&b.1.score)) {
                    self.sel = i;
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame, screen: Rect) {
        f.buffer_mut().set_style(screen, Style::new().fg(DIM).remove_modifier(Modifier::BOLD));
        let w = screen.width.saturating_sub(4).min(CARD_W * 5 + 4);
        let h = screen.height.saturating_sub(2).min(CARD_H * 2 + 6);
        let area = Rect { x: screen.x + (screen.width - w) / 2, y: screen.y + (screen.height - h) / 2, width: w, height: h };
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Line::from(vec![
                Span::styled(" Cover ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
                Span::styled(format!(" {} ", self.title), Style::new().fg(Color::White)),
            ]));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [search, grid, status, keys] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1), Constraint::Length(1)]).areas(inner);

        // Search line.
        let mut line = vec![Span::styled(" Search  ", Style::new().fg(DIM))];
        if self.editing_query {
            line.extend(self.query.spans());
        } else {
            line.push(Span::styled(self.query.text(), Style::new().fg(Color::White)));
        }
        line.push(Span::styled(format!("   store: {}", self.country.to_uppercase()), Style::new().fg(ACCENT)));
        f.render_widget(Line::from(line), search);

        // Grid of cards, scrolled to keep the selection visible.
        self.cols = (grid.width / CARD_W).max(1) as usize;
        let rows = (grid.height / CARD_H).max(1) as usize;
        let sel_row = self.sel / self.cols;
        if sel_row < self.scroll {
            self.scroll = sel_row;
        } else if sel_row >= self.scroll + rows {
            self.scroll = sel_row + 1 - rows;
        }
        let best = self.cards.iter().enumerate().max_by(|a, b| a.1.score.total_cmp(&b.1.score)).map(|(i, _)| i);
        let first = self.scroll * self.cols;
        for (slot, i) in (first..self.cards.len()).take(rows * self.cols).enumerate() {
            let (cx, cy) = ((slot % self.cols) as u16, (slot / self.cols) as u16);
            let rect = Rect { x: grid.x + cx * CARD_W, y: grid.y + cy * CARD_H, width: CARD_W, height: CARD_H };
            self.draw_card(f, rect, i, Some(i) == best);
        }
        if self.cards.is_empty() && self.searching.is_none() && self.message.is_none() {
            f.render_widget(Paragraph::new("\nNo results yet.").fg(DIM).centered(), grid);
        }

        let status_line = if let Some(t) = self.applying {
            let what = if self.choose_only { "Downloading cover…".to_string() } else { format!("Downloading and applying cover to {} track(s)…", self.tracks.len()) };
            Line::from(format!(" {} {what}", spin(t))).fg(ACCENT)
        } else if let Some(t) = self.searching {
            Line::from(format!(" {} Searching iTunes ({})…", spin(t), self.country.to_uppercase())).fg(ACCENT)
        } else if let Some(m) = &self.message {
            Line::from(format!(" {m}")).fg(Color::Yellow)
        } else {
            Line::from(format!(" {} results · drop an image file here to use your own", self.cards.len())).fg(DIM)
        };
        f.render_widget(status_line, status);

        let help: &[(&str, &str)] = if self.editing_query {
            &[("enter", "search"), ("esc", "cancel")]
        } else {
            &[("←→↑↓", "choose"), ("enter", "apply"), ("b", "best"), ("/", "search"), ("tab", "store"), ("esc", "close")]
        };
        let spans: Vec<Span> = help
            .iter()
            .flat_map(|(k, d)| [Span::styled(format!(" {k} "), Style::new().fg(ACCENT)), Span::styled(format!("{d} "), Style::new().fg(DIM))])
            .collect();
        f.render_widget(Line::from(spans), keys);
    }

    fn draw_card(&mut self, f: &mut Frame, rect: Rect, i: usize, is_best: bool) {
        let selected = i == self.sel;
        let card = &mut self.cards[i];
        let border = if selected { Style::new().fg(ACCENT).bold() } else { Style::new().fg(Color::Rgb(60, 60, 70)) };
        let mut block = Block::bordered().border_type(BorderType::Rounded).border_style(border);
        if is_best {
            block = block.title(Span::styled(" ★ best ", Style::new().fg(Color::Yellow)));
        }
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let [img, text] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);
        match &mut card.preview {
            Preview::Ready(p) => f.render_stateful_widget(StatefulImage::default().resize(Resize::Fit(None)), img, p),
            Preview::Loading => f.render_widget(Paragraph::new("\n\n\n   loading…").fg(DIM), img),
            Preview::Failed => f.render_widget(Paragraph::new("\n\n\n   no preview").fg(DIM), img),
        }
        let (title, sub) = match &card.source {
            Source::Itunes(h) => (
                h.album.clone(),
                format!("{} · {} · {}t", h.artist, if h.year > 0 { h.year.to_string() } else { "—".into() }, h.tracks),
            ),
            Source::Local(p, _) => (
                "Your image".into(),
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            ),
        };
        let style = if selected { Style::new().fg(Color::White).bold() } else { Style::new().fg(Color::Gray) };
        f.render_widget(
            Paragraph::new(vec![Line::from(title).style(style), Line::from(sub).fg(DIM)]),
            text,
        );
    }
}

fn spin(t: Instant) -> char {
    SPINNER[(t.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// Live search + render: `cargo test -- --ignored picker_live --nocapture`
    #[test]
    #[ignore]
    fn picker_live() {
        let t = Track { title: "Butter".into(), artist: "A Tribe Called Quest".into(), album: "The Low End Theory".into(), ..Default::default() };
        let mut p = CoverPicker::new("/nonexistent".into(), "The Low End Theory".into(), vec![t; 14], Picker::halfblocks(), false);
        let start = Instant::now();
        while start.elapsed().as_secs() < 20 {
            p.tick();
            if p.searching.is_none() && p.cards.iter().all(|c| !matches!(c.preview, Preview::Loading)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let mut term = Terminal::new(TestBackend::new(130, 36)).unwrap();
        term.draw(|f| p.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{line}");
        }
        let best = &p.cards[p.sel];
        match &best.source {
            Source::Itunes(h) => println!("selected: {} — {} ({:.2})", h.artist, h.album, best.score),
            Source::Local(..) => unreachable!(),
        }
        assert!(!p.cards.is_empty());
    }
}
