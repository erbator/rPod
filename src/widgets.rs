//! Small reusable TUI pieces.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// A single-line text input with a cursor.
pub struct Input {
    chars: Vec<char>,
    cur: usize,
}

impl Input {
    pub fn new(s: &str) -> Self {
        let chars: Vec<char> = s.chars().collect();
        Self { cur: chars.len(), chars }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn insert(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.chars.insert(self.cur, c);
            self.cur += 1;
        }
    }

    /// Returns false for keys it doesn't handle.
    pub fn key(&mut self, key: KeyEvent) -> bool {
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

    pub fn spans(&self) -> Vec<Span<'static>> {
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
