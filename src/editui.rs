//! The metadata editor pop-up.

use crate::edit::{self, FIELDS, Field, Form, Kind, Report};
use crate::itunesdb::Track;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const CHANGED: Color = Color::Yellow;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A single-line text input with a cursor.
struct Input {
    chars: Vec<char>,
    cur: usize,
}

impl Input {
    fn new(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        Self { cur: chars.len(), chars }
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.chars.insert(self.cur, c);
            self.cur += 1;
        }
    }

    /// Returns false for keys it doesn't handle.
    fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(self.chars.len()),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.chars.len(),
            KeyCode::Backspace if self.cur > 0 => {
                self.cur -= 1;
                self.chars.remove(self.cur);
            }
            KeyCode::Delete if self.cur < self.chars.len() => {
                self.chars.remove(self.cur);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.chars.drain(..self.cur);
                self.cur = 0;
            }
            KeyCode::Char(c) => self.insert(&c.to_string()),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    fn spans(&self) -> Vec<Span<'static>> {
        let before: String = self.chars[..self.cur].iter().collect();
        let at = self.chars.get(self.cur).map_or(" ".to_string(), |c| c.to_string());
        let after: String = self.chars.get(self.cur + 1..).map_or(String::new(), |s| s.iter().collect());
        vec![
            Span::styled(before, Style::new().fg(Color::White)),
            Span::styled(at, Style::new().fg(Color::Black).bg(Color::White)),
            Span::styled(after, Style::new().fg(Color::White)),
        ]
    }
}

pub struct EditView {
    root: PathBuf,
    title: String,
    tracks: Vec<Track>,
    /// Value shared by all tracks, or `None` when they differ.
    common: Vec<Option<String>>,
    form: Form,
    sel: usize,
    input: Option<Input>,
    confirm_discard: bool,
    message: Option<String>,
    pub write_files: bool,
    cover: Option<StatefulProtocol>,
    saving: Option<(Receiver<Result<Report, String>>, Instant)>,
    /// Set when the editor closes: `Some(report)` after a save.
    pub closed: Option<Option<Report>>,
}

impl EditView {
    pub fn new(root: PathBuf, title: String, tracks: Vec<Track>, cover: Option<StatefulProtocol>, write_files: bool) -> Self {
        let common = FIELDS
            .iter()
            .map(|f| {
                let first = f.get(&tracks[0]);
                tracks.iter().all(|t| f.get(t) == first).then_some(first)
            })
            .collect();
        Self {
            root,
            title,
            tracks,
            common,
            form: Form::default(),
            sel: 0,
            input: None,
            confirm_discard: false,
            message: None,
            write_files,
            cover,
            saving: None,
            closed: None,
        }
    }

    fn field(&self) -> Field {
        FIELDS[self.sel]
    }

    /// The value shown for a field: typed, else shared, else mixed (`None`).
    fn shown(&self, i: usize) -> Option<String> {
        self.form.values.get(&FIELDS[i]).cloned().or_else(|| self.common[i].clone())
    }

    fn change_count(&self) -> usize {
        self.form.values.len() + self.form.auto_number as usize + self.form.clean as usize
    }

    /// Set a field, or forget the edit if it matches the original again.
    fn set(&mut self, f: Field, v: String) {
        let i = FIELDS.iter().position(|&x| x == f).unwrap();
        if self.common[i].as_deref() == Some(v.as_str()) {
            self.form.values.remove(&f);
        } else {
            self.form.values.insert(f, v);
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(input) = &mut self.input {
            input.insert(&text.replace(['\n', '\r'], " "));
        }
    }

    /// Poll the background save. Returns true while something animates.
    pub fn tick(&mut self) -> bool {
        let Some((rx, _)) = &self.saving else { return false };
        match rx.try_recv() {
            Ok(Ok(report)) => {
                self.saving = None;
                self.closed = Some(Some(report));
            }
            Ok(Err(e)) => {
                self.saving = None;
                self.message = Some(format!("Save failed: {e}"));
            }
            Err(_) => {}
        }
        true
    }

    fn save(&mut self) {
        let edits = match self.form.build(&self.tracks) {
            Ok(e) => e,
            Err(msg) => {
                self.message = Some(msg);
                return;
            }
        };
        if edits.is_empty() {
            self.closed = Some(None);
            return;
        }
        let (tx, rx) = channel();
        let (root, write_files) = (self.root.clone(), self.write_files);
        std::thread::spawn(move || {
            tx.send(edit::save(&root, &edits, write_files).map_err(|e| format!("{e:#}"))).ok();
        });
        self.saving = Some((rx, Instant::now()));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.saving.is_some() {
            return;
        }
        self.message = None;

        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Enter | KeyCode::Tab => {
                    let v = input.text();
                    self.input = None;
                    let f = self.field();
                    if f.kind() == Kind::Number && !v.trim().is_empty() && v.trim().parse::<u32>().is_err() {
                        self.message = Some(format!("{} must be a number", f.label()));
                        return;
                    }
                    self.set(f, v);
                    if key.code == KeyCode::Tab {
                        self.sel = (self.sel + 1) % FIELDS.len();
                    }
                }
                KeyCode::Esc => self.input = None,
                _ => {
                    input.key(key);
                }
            }
            return;
        }

        if self.confirm_discard {
            self.confirm_discard = false;
            if key.code == KeyCode::Esc {
                self.closed = Some(None);
            }
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let f = self.field();
        match key.code {
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.change_count() == 0 {
                    self.closed = Some(None);
                } else {
                    self.confirm_discard = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.sel = (self.sel + 1) % FIELDS.len(),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.sel = (self.sel + FIELDS.len() - 1) % FIELDS.len()
            }
            KeyCode::Enter if matches!(f.kind(), Kind::Text | Kind::Number) => {
                self.input = Some(Input::new(&self.shown(self.sel).unwrap_or_default()));
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
                if f.kind() == Kind::Toggle =>
            {
                let on = self.shown(self.sel).as_deref() == Some("yes");
                self.set(f, if on { "no" } else { "yes" }.into());
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') if f.kind() == Kind::Stars => {
                let cur: i32 = self.shown(self.sel).and_then(|v| v.parse().ok()).unwrap_or(0);
                let up = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
                self.set(f, (cur + if up { 1 } else { -1 }).clamp(0, 5).to_string());
            }
            KeyCode::Char(c @ '0'..='5') if f.kind() == Kind::Stars => self.set(f, c.to_string()),
            KeyCode::Char('u') | KeyCode::Backspace => {
                self.form.values.remove(&f);
            }
            KeyCode::Char('n') => self.form.auto_number = !self.form.auto_number,
            KeyCode::Char('x') => self.form.clean = !self.form.clean,
            KeyCode::Char('f') => self.write_files = !self.write_files,
            KeyCode::Char('t') => {
                self.set(Field::TrackTotal, self.tracks.len().to_string());
                let discs = self.tracks.iter().map(|t| t.disc_no).max().unwrap_or(0);
                if discs > 0 {
                    self.set(Field::DiscTotal, discs.to_string());
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn draw(&mut self, f: &mut Frame, screen: Rect) {
        // Dim whatever is behind the pop-up.
        f.buffer_mut().set_style(screen, Style::new().fg(DIM).remove_modifier(Modifier::BOLD));

        let w = screen.width.saturating_sub(4).min(86);
        let h = screen.height.saturating_sub(2).min(FIELDS.len() as u16 + 6);
        let area = Rect {
            x: screen.x + (screen.width - w) / 2,
            y: screen.y + (screen.height - h) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, area);
        let n = self.tracks.len();
        let what = if n == 1 { "1 track".to_string() } else { format!("{n} tracks") };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Line::from(vec![
                Span::styled(" Edit ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
                Span::styled(format!(" {what} · {} ", self.title), Style::new().fg(Color::White)),
            ]));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let [body, _, status, keys] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let [cover_col, fields] = Layout::horizontal([Constraint::Length(20), Constraint::Min(0)]).areas(body);

        let fs_cover = Rect { x: cover_col.x + 1, y: cover_col.y + 1, width: 18, height: 9.min(cover_col.height) };
        match &mut self.cover {
            Some(proto) => f.render_stateful_widget(StatefulImage::default().resize(Resize::Fit(None)), fs_cover, proto),
            None => f.render_widget(Paragraph::new("\n\n\n  no artwork").fg(DIM), fs_cover),
        }

        let mut lines = Vec::with_capacity(FIELDS.len() + 1);
        lines.push(Line::from(""));
        for (i, &field) in FIELDS.iter().enumerate() {
            let selected = i == self.sel;
            let changed = self.form.values.contains_key(&field)
                || (self.form.auto_number && matches!(field, Field::TrackNo | Field::TrackTotal));
            let marker = Span::styled(if selected { " ▸ " } else { "   " }, Style::new().fg(ACCENT));
            let label = Span::styled(
                format!("{:<14}", field.label()),
                Style::new().fg(if selected { Color::White } else { Color::Gray }),
            );
            let mut row = vec![marker, label];
            if selected && let Some(input) = &self.input {
                row.extend(input.spans());
            } else {
                let value_style = if changed { Style::new().fg(CHANGED) } else { Style::new().fg(Color::White) };
                let shown = self.shown(i);
                let text = match field {
                    Field::TrackNo if self.form.auto_number => format!("1–{n} (auto)"),
                    Field::Rating => match shown {
                        Some(v) => {
                            let s: usize = v.parse().unwrap_or(0);
                            format!("{}{}", "★".repeat(s), "☆".repeat(5 - s.min(5)))
                        }
                        None => "‹mixed›".into(),
                    },
                    _ => shown.unwrap_or_else(|| "‹mixed›".into()),
                };
                let style = if self.shown(i).is_none() && !changed { Style::new().fg(DIM).italic() } else { value_style };
                row.push(Span::styled(text, style));
                if changed {
                    row.push(Span::styled(" ●", Style::new().fg(CHANGED)));
                }
            }
            lines.push(Line::from(row));
        }
        f.render_widget(Paragraph::new(lines), fields);

        let toggle = |on: bool, key: &str, label: &str| -> Vec<Span<'static>> {
            vec![
                Span::styled(format!("[{key}]"), Style::new().fg(ACCENT)),
                Span::styled(format!(" {label}  "), if on { Style::new().fg(CHANGED).bold() } else { Style::new().fg(DIM) }),
            ]
        };
        let status_line = if let Some((_, started)) = &self.saving {
            let frame = SPINNER[(started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
            Line::from(format!(" {frame} Saving {what}…")).fg(ACCENT)
        } else if self.confirm_discard {
            Line::from(format!(" Discard {} change(s)? esc again to discard, any other key keeps editing", self.change_count()))
                .fg(Color::Red)
        } else if let Some(msg) = &self.message {
            Line::from(format!(" {msg}")).fg(Color::Red)
        } else {
            let count = self.change_count();
            let mut spans = vec![Span::styled(
                format!(" ● {count} change{}   ", if count == 1 { "" } else { "s" }),
                Style::new().fg(if count > 0 { CHANGED } else { DIM }),
            )];
            spans.extend(toggle(self.form.auto_number, "n", "auto-number"));
            spans.extend(toggle(false, "t", "fill totals"));
            spans.extend(toggle(self.form.clean, "x", "clean"));
            spans.extend(toggle(self.write_files, "f", "tags→files"));
            Line::from(spans)
        };
        f.render_widget(status_line, status);

        let key_help: &[(&str, &str)] = if self.input.is_some() {
            &[("enter", "ok"), ("tab", "ok + next"), ("esc", "undo field"), ("ctrl+u", "clear")]
        } else {
            &[("↑↓", "field"), ("enter", "edit"), ("←→", "stars/toggle"), ("u", "revert"), ("ctrl+s", "save"), ("esc", "cancel")]
        };
        let help: Vec<Span> = key_help
            .iter()
            .flat_map(|(k, d)| [Span::styled(format!(" {k} "), Style::new().fg(ACCENT)), Span::styled(format!("{d} "), Style::new().fg(DIM))])
            .collect();
        f.render_widget(Line::from(help), keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn popup_snapshot() {
        let Ok(root) = std::env::var("RPOD_TEST_IPOD") else { return };
        let db = crate::itunesdb::read(&std::path::Path::new(&root).join("iPod_Control/iTunes/iTunesDB")).unwrap();
        let album = db.tracks[2].album.clone();
        let tracks: Vec<Track> = db.tracks.iter().filter(|t| t.album == album).cloned().collect();
        let mut view = EditView::new(root.into(), format!("{album} · {}", tracks[0].artist), tracks, None, true);
        let press = |v: &mut EditView, c: KeyCode| v.on_key(KeyEvent::from(c));
        for _ in 0..4 {
            press(&mut view, KeyCode::Down); // → Genre
        }
        press(&mut view, KeyCode::Enter);
        for _ in 0..20 {
            press(&mut view, KeyCode::Backspace);
        }
        for c in "Power Pop".chars() {
            press(&mut view, KeyCode::Char(c));
        }
        press(&mut view, KeyCode::Enter);
        press(&mut view, KeyCode::Down); // Year
        press(&mut view, KeyCode::Enter);
        let mut term = Terminal::new(TestBackend::new(110, 28)).unwrap();
        term.draw(|f| view.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        let text: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect();
        println!("{}", text.join("\n"));
        assert!(text.iter().any(|l| l.contains("Power Pop ●")));
    }
}
