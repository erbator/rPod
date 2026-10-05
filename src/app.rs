//! TUI state: tabs of drill-down columns, filtering, and cover art caching.

use crate::coverui::CoverPicker;
use crate::device::Ipod;
use crate::downloadui::DownloadView;
use crate::editui::EditView;
use crate::fixui::{AlbumRow, FixView};
use crate::import;
use crate::importui::ImportView;
use crate::library::Index;
use crate::player::Player;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Recently shown covers kept ready, so revisiting an album sends nothing.
const ART_CACHE: usize = 48;
/// A new cover loads once scrolling pauses for this long; holding a key
/// down never pushes images to the terminal.
const ART_DELAY: Duration = Duration::from_millis(70);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Artists,
    Albums,
    Songs,
    Playlists,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Artists, Tab::Albums, Tab::Songs, Tab::Playlists];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Artists => "Artists",
            Tab::Albums => "Albums",
            Tab::Songs => "Songs",
            Tab::Playlists => "Playlists",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Artist(usize),
    Album(usize),
    Playlist(usize),
    Track(usize),
}

/// How a track column is laid out.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    Names,
    /// Inside an album: track number, title, length.
    AlbumTracks,
    /// Mixed tracks: title, artist, album, length.
    Tracks,
}

pub struct Column {
    pub title: String,
    pub kind: ColKind,
    all: Vec<Item>,
    pub items: Vec<Item>,
    pub state: TableState,
    pub filter: String,
}

impl Column {
    fn new(title: impl Into<String>, kind: ColKind, items: Vec<Item>) -> Self {
        let mut state = TableState::default();
        if !items.is_empty() {
            state.select(Some(0));
        }
        Self { title: title.into(), kind, all: items.clone(), items, state, filter: String::new() }
    }

    pub fn selected(&self) -> Option<Item> {
        self.items.get(self.state.selected()?).copied()
    }
}

pub struct App {
    pub ipod: Ipod,
    pub index: Index,
    pub tab: Tab,
    pub cols: Vec<Column>,
    pub focus: usize,
    pub filtering: bool,
    pub quit: bool,
    pub picker: Picker,
    /// The shown cover, keyed by album index (all of an album's tracks
    /// share one image). `None` inside means the album has no art.
    pub art: Option<(usize, Option<StatefulProtocol>)>,
    /// Most recent first.
    art_cache: Vec<(usize, Option<StatefulProtocol>)>,
    pub art_pending: Option<(usize, Instant)>,
    pub import: Option<ImportView>,
    pub edit: Option<EditView>,
    pub cover: Option<CoverPicker>,
    pub fix: Option<FixView>,
    pub download: Option<DownloadView>,
    pub player: Player,
    /// The fix-covers row the open cover picker belongs to.
    picker_for_fix: Option<usize>,
    /// The open cover picker is choosing a cover for the Add music queue.
    picker_for_import: bool,
    /// Tracks marked with space for batch editing.
    pub marked: HashSet<usize>,
    pub status: Option<String>,
}

impl App {
    pub fn new(ipod: Ipod, picker: Picker) -> Self {
        let index = Index::build(&ipod.db.tracks);
        let player = Player::new(ipod.root.clone(), import::Settings::load().volume);
        let mut app = Self {
            ipod,
            index,
            tab: Tab::Artists,
            cols: Vec::new(),
            focus: 0,
            filtering: false,
            quit: false,
            picker,
            art: None,
            art_cache: Vec::new(),
            art_pending: None,
            import: None,
            edit: None,
            cover: None,
            fix: None,
            download: None,
            player,
            picker_for_fix: None,
            picker_for_import: false,
            marked: HashSet::new(),
            status: None,
        };
        app.set_tab(Tab::Artists);
        app
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.marked.clear();
        self.tab = tab;
        self.focus = 0;
        let root = match tab {
            Tab::Artists => Column::new(
                "Artists",
                ColKind::Names,
                (0..self.index.artists.len()).map(Item::Artist).collect(),
            ),
            Tab::Albums => Column::new(
                "Albums",
                ColKind::Names,
                (0..self.index.albums.len()).map(Item::Album).collect(),
            ),
            Tab::Songs => Column::new(
                "Songs",
                ColKind::Tracks,
                self.index.songs.iter().map(|&i| Item::Track(i)).collect(),
            ),
            Tab::Playlists => Column::new(
                "Playlists",
                ColKind::Names,
                (0..self.ipod.db.playlists.len()).map(Item::Playlist).collect(),
            ),
        };
        self.cols = vec![root];
        self.rebuild_from(0);
    }

    /// Recompute every column to the right of `col` from its selection.
    fn rebuild_from(&mut self, col: usize) {
        self.cols.truncate(col + 1);
        while let Some(next) = self.cols.last().and_then(|c| c.selected()).and_then(|it| self.children(it)) {
            self.cols.push(next);
        }
        self.refresh_art();
    }

    fn children(&self, item: Item) -> Option<Column> {
        let ix = &self.index;
        Some(match item {
            Item::Artist(a) => Column::new(
                ix.artists[a].name.clone(),
                ColKind::Names,
                ix.artists[a].albums.iter().map(|&i| Item::Album(i)).collect(),
            ),
            Item::Album(a) => Column::new(
                ix.albums[a].title.clone(),
                ColKind::AlbumTracks,
                ix.albums[a].tracks.iter().map(|&i| Item::Track(i)).collect(),
            ),
            Item::Playlist(p) => {
                let pl = &self.ipod.db.playlists[p];
                Column::new(
                    pl.name.clone(),
                    ColKind::Tracks,
                    pl.items.iter().filter_map(|id| ix.by_id.get(id)).map(|&i| Item::Track(i)).collect(),
                )
            }
            Item::Track(_) => return None,
        })
    }

    /// The track whose details and cover are shown: the focused one, or the
    /// first track under the focused artist/album/playlist.
    pub fn shown_track(&self) -> Option<usize> {
        let mut item = self.cols.get(self.focus)?.selected()?;
        loop {
            item = match item {
                Item::Track(t) => return Some(t),
                Item::Artist(a) => Item::Album(*self.index.artists[a].albums.first()?),
                Item::Album(a) => Item::Track(*self.index.albums[a].tracks.first()?),
                Item::Playlist(p) => {
                    let id = self.ipod.db.playlists[p].items.first()?;
                    Item::Track(*self.index.by_id.get(id)?)
                }
            };
        }
    }

    fn refresh_art(&mut self) {
        let want = self.shown_track().map(|t| self.index.album_of[t]);
        if want == self.art.as_ref().map(|(k, _)| *k) {
            self.art_pending = None;
            return;
        }
        let Some(key) = want else {
            self.stash_art();
            self.art_pending = None;
            return;
        };
        if let Some(pos) = self.art_cache.iter().position(|(k, _)| *k == key) {
            let entry = self.art_cache.remove(pos);
            self.stash_art();
            self.art = Some(entry);
            self.art_pending = None;
        } else {
            self.art_pending = Some((key, Instant::now() + ART_DELAY));
        }
    }

    fn stash_art(&mut self) {
        if let Some(cur) = self.art.take() {
            self.art_cache.insert(0, cur);
            self.art_cache.truncate(ART_CACHE);
        }
    }

    fn load_pending_art(&mut self) -> bool {
        let Some((key, due)) = self.art_pending else { return false };
        if Instant::now() < due {
            return false;
        }
        self.art_pending = None;
        let img = self.index.albums[key]
            .tracks
            .iter()
            .find_map(|&t| self.ipod.art.best_thumb(self.ipod.db.tracks[t].dbid))
            .and_then(|th| self.ipod.art.load(th).ok());
        let proto = img.map(|i| self.picker.new_resize_protocol(i));
        self.stash_art();
        self.art = Some((key, proto));
        true
    }

    /// When the event loop must wake up next even without input.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.art_pending.map(|(_, due)| due)
    }

    fn open_import(&mut self) -> &mut ImportView {
        let on_ipod = self.ipod.db.tracks.iter().map(import::dup_key).collect();
        let root = self.ipod.root.clone();
        self.import.get_or_insert_with(|| ImportView::new(root, on_ipod))
    }

    /// Dropped files (Kitty pastes their paths) open the Add music screen.
    pub fn on_paste(&mut self, text: &str) {
        if let Some(view) = &mut self.cover {
            view.on_paste(text);
        } else if let Some(view) = &mut self.edit {
            view.on_paste(text);
        } else if let Some(view) = &mut self.download {
            view.on_paste(text);
        } else {
            self.open_import().on_paste(text);
        }
    }

    /// Albums with tracks lacking cover art, for "fix missing covers".
    fn open_fix(&mut self) {
        let rows: Vec<AlbumRow> = self
            .index
            .albums
            .iter()
            .filter_map(|a| {
                let missing: Vec<crate::itunesdb::Track> = a
                    .tracks
                    .iter()
                    .map(|&t| &self.ipod.db.tracks[t])
                    .filter(|t| !self.ipod.art.by_track.contains_key(&t.dbid))
                    .cloned()
                    .collect();
                (!missing.is_empty()).then(|| AlbumRow::new(a.title.clone(), a.artist.clone(), missing))
            })
            .collect();
        if rows.is_empty() {
            self.status = Some("Every album already has a cover.".into());
            return;
        }
        let write_files = import::Settings::load().embed_covers;
        self.fix = Some(FixView::new(self.ipod.root.clone(), rows, self.picker.clone(), write_files));
    }

    fn open_cover_picker(&mut self, title: String, tracks: Vec<crate::itunesdb::Track>) {
        if tracks.is_empty() {
            return;
        }
        let write_files = import::Settings::load().embed_covers;
        self.cover = Some(CoverPicker::new(self.ipod.root.clone(), title, tracks, self.picker.clone(), write_files));
    }

    /// Whether `c` has something to put a cover on: marked tracks, or a
    /// focused album, song or playlist. An artist spans several albums.
    pub fn can_pick_cover(&self) -> bool {
        !self.marked.is_empty() || !matches!(self.cols.get(self.focus).and_then(|c| c.selected()), Some(Item::Artist(_)))
    }

    /// The tracks `i` edits: the marked ones, else everything under the
    /// focused row. Returns them with a title for the editor.
    fn edit_targets(&self) -> Option<(String, Vec<usize>)> {
        if !self.marked.is_empty() {
            let mut tracks: Vec<usize> = self.marked.iter().copied().collect();
            let order: std::collections::HashMap<usize, usize> =
                self.index.songs.iter().enumerate().map(|(pos, &t)| (t, pos)).collect();
            tracks.sort_by_key(|t| order.get(t).copied().unwrap_or(usize::MAX));
            return Some(("marked tracks".into(), tracks));
        }
        let ix = &self.index;
        Some(match self.cols.get(self.focus)?.selected()? {
            Item::Track(t) => (self.ipod.db.tracks[t].title.clone(), vec![t]),
            Item::Album(a) => (format!("{} · {}", ix.albums[a].title, ix.albums[a].artist), ix.albums[a].tracks.clone()),
            Item::Artist(a) => (
                ix.artists[a].name.clone(),
                ix.artists[a].albums.iter().flat_map(|&al| ix.albums[al].tracks.iter().copied()).collect(),
            ),
            Item::Playlist(p) => {
                let pl = &self.ipod.db.playlists[p];
                (pl.name.clone(), pl.items.iter().filter_map(|id| ix.by_id.get(id).copied()).collect())
            }
        })
    }

    /// Play the focused column from the selected song on: the rest of the
    /// album, playlist or list is the queue.
    fn play_selected(&mut self) {
        let col = &self.cols[self.focus];
        let Some(Item::Track(sel)) = col.selected() else { return };
        if crate::export::is_video(&self.ipod.db.tracks[sel]) {
            self.status = Some("Videos don't play in rPod.".into());
            return;
        }
        let songs: Vec<usize> = col
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Track(t) if !crate::export::is_video(&self.ipod.db.tracks[*t]) => Some(*t),
                _ => None,
            })
            .collect();
        let start = songs.iter().position(|&t| t == sel).unwrap_or(0);
        let queue = songs.iter().map(|&t| self.ipod.db.tracks[t].clone()).collect();
        self.player.play(queue, start);
    }

    /// Handle `code` if it's one of the player's keys.
    fn player_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('p') => self.player.toggle_pause(),
            KeyCode::Char('>') => self.player.next(),
            KeyCode::Char('<') => self.player.prev(),
            KeyCode::Char(']') => self.player.seek(10),
            KeyCode::Char('[') => self.player.seek(-10),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5),
            KeyCode::Char('-') => self.change_volume(-5),
            KeyCode::Char('z') => self.player.toggle_shuffle(),
            KeyCode::Char('r') => self.player.cycle_repeat(),
            _ => return false,
        }
        true
    }

    fn change_volume(&mut self, delta: i8) {
        self.player.change_volume(delta);
        let volume = self.player.volume;
        import::Settings::update(|s| s.volume = volume);
    }

    /// `d` downloads the selection (like `i` edits it), `S` the whole library.
    fn open_download(&mut self, everything: bool) {
        let (title, tracks) = if everything {
            ("Sync all music to PC".to_string(), self.index.songs.clone())
        } else {
            let Some((title, tracks)) = self.edit_targets() else { return };
            (format!("Download {title}"), tracks)
        };
        if tracks.is_empty() {
            return;
        }
        let tracks = tracks.iter().map(|&t| self.ipod.db.tracks[t].clone()).collect();
        self.download = Some(DownloadView::new(self.ipod.root.clone(), title, everything, tracks));
    }

    fn open_editor(&mut self) {
        let Some((title, tracks)) = self.edit_targets() else { return };
        if tracks.is_empty() {
            return;
        }
        let cover = tracks
            .iter()
            .find_map(|&t| self.ipod.art.best_thumb(self.ipod.db.tracks[t].dbid))
            .and_then(|th| self.ipod.art.load(th).ok())
            .map(|img| self.picker.new_resize_protocol(img));
        let tracks = tracks.iter().map(|&t| self.ipod.db.tracks[t].clone()).collect();
        let write_files = import::Settings::load().write_tags;
        self.edit = Some(EditView::new(self.ipod.root.clone(), title, tracks, cover, write_files));
    }

    /// Poll background work and the player. Returns true if a redraw is needed.
    pub fn tick(&mut self) -> bool {
        let mut playing = self.player.tick();
        if let Some(e) = self.player.error.take() {
            self.status = Some(e);
            playing = true;
        }
        self.tick_views() | playing
    }

    fn tick_views(&mut self) -> bool {
        let art = self.load_pending_art();
        if let Some(view) = &mut self.cover {
            let changed = view.tick();
            if let Some(result) = view.closed.take() {
                self.cover = None;
                // The picker's f key may have changed the embed setting.
                if let Some(fix) = &mut self.fix {
                    fix.write_files = import::Settings::load().embed_covers;
                }
                let fix_row = self.picker_for_fix.take();
                if std::mem::take(&mut self.picker_for_import) {
                    if let (Some(view), Some((_, image))) = (&mut self.import, result) {
                        view.set_picked_cover(image);
                    }
                    return true;
                }
                if let Some((report, image)) = result {
                    if let (Some(fix), Some(row)) = (&mut self.fix, fix_row) {
                        fix.mark_done(row);
                    }
                    if let Some(edit) = &mut self.edit {
                        let proto = image::load_from_memory(&image)
                            .ok()
                            .map(|img| self.picker.new_resize_protocol(img.thumbnail(400, 400)));
                        edit.set_cover(proto);
                    }
                    self.reload();
                    self.status = Some(match report.file_errors.len() {
                        0 => format!("New cover on {} track(s).", report.tracks),
                        n => format!("New cover on {} track(s); {n} file(s) failed: {}", report.tracks, report.file_errors[0]),
                    });
                }
                return true;
            }
            return changed || art;
        }
        if let Some(view) = &mut self.edit {
            let animating = view.tick();
            if let Some(result) = view.closed.take() {
                let write_files = view.write_files;
                self.edit = None;
                if import::Settings::load().write_tags != write_files {
                    import::Settings::update(|s| s.write_tags = write_files);
                }
                if let Some(report) = result {
                    self.marked.clear();
                    self.reload();
                    self.status = Some(match report.file_errors.len() {
                        0 => format!("Saved {} track(s).", report.tracks),
                        n => format!("Saved {} track(s); {n} file tag write(s) failed: {}", report.tracks, report.file_errors[0]),
                    });
                }
                return true;
            }
            return animating || art;
        }
        if let Some(view) = &mut self.fix {
            let changed = view.tick();
            if let Some(result) = view.closed.take() {
                self.fix = None;
                if let Some(report) = result {
                    self.reload();
                    self.status = Some(match report.file_errors.len() {
                        0 => format!("Added covers to {} track(s).", report.tracks),
                        n => format!("Added covers to {} track(s); {n} file(s) failed: {}", report.tracks, report.file_errors[0]),
                    });
                }
                return true;
            }
            return changed || art;
        }
        if let Some(view) = &mut self.download {
            let changed = view.tick();
            if view.closed {
                self.download = None;
                return true;
            }
            return changed || art;
        }
        let Some(view) = &mut self.import else { return art };
        let changed = view.tick() || art;
        if let Some(library_changed) = view.closed {
            self.import = None;
            if library_changed {
                self.reload();
            }
            return true;
        }
        changed
    }

    /// Re-read the iPod, keeping the view, focus and selections where they were.
    fn reload(&mut self) {
        match Ipod::open(&self.ipod.root) {
            Ok(ipod) => {
                self.status = Some(format!("Library reloaded: {} songs.", ipod.db.tracks.len()));
                self.index = Index::build(&ipod.db.tracks);
                self.ipod = ipod;
                self.art = None;
                self.art_cache.clear();
                let selections: Vec<(Option<usize>, usize)> =
                    self.cols.iter().map(|c| (c.state.selected(), c.state.offset())).collect();
                let focus = self.focus;
                self.set_tab(self.tab);
                for (i, (sel, offset)) in selections.into_iter().enumerate() {
                    let Some(col) = self.cols.get_mut(i) else { break };
                    if let Some(sel) = sel.filter(|_| !col.items.is_empty()) {
                        col.state.select(Some(sel.min(col.items.len() - 1)));
                        *col.state.offset_mut() = offset;
                        self.rebuild_from(i);
                    }
                }
                self.focus = focus.min(self.cols.len().saturating_sub(1));
                self.refresh_art();
            }
            Err(e) => self.status = Some(format!("Couldn't reload the iPod: {e:#}")),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        if let Some(view) = &mut self.cover {
            view.on_key(key);
            self.tick();
            return;
        }
        if let Some(view) = &mut self.edit {
            view.on_key(key);
            if std::mem::take(&mut view.wants_cover) {
                let (title, tracks) = (view.title().to_string(), view.tracks().to_vec());
                self.open_cover_picker(title, tracks);
            }
            self.tick();
            return;
        }
        // Full screens leave the player's keys free, except while typing.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let typing = self.download.as_ref().is_some_and(|v| v.typing()) || self.import.as_ref().is_some_and(|v| v.typing());
        let full_screen = self.fix.is_some() || self.download.is_some() || self.import.is_some();
        if full_screen && !typing && !ctrl && self.player_key(key.code) {
            return;
        }
        if let Some(view) = &mut self.fix {
            view.on_key(key);
            if let Some(row) = view.wants_picker.take() {
                if let Some((title, tracks)) = view.row_tracks(row).map(|(t, tr)| (t.to_string(), tr.to_vec())) {
                    self.picker_for_fix = Some(row);
                    self.open_cover_picker(title, tracks);
                }
            }
            self.tick();
            return;
        }
        if let Some(view) = &mut self.download {
            view.on_key(key);
            self.tick();
            return;
        }
        if let Some(view) = &mut self.import {
            view.on_key(key);
            if let Some((title, tracks)) = view.wants_picker.take() {
                self.open_cover_picker(title, tracks);
                if let Some(p) = &mut self.cover {
                    p.choose_only = true;
                    self.picker_for_import = true;
                }
            }
            self.tick();
            return;
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        if !ctrl && self.player_key(key.code) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('1') => self.set_tab(Tab::Artists),
            KeyCode::Char('2') => self.set_tab(Tab::Albums),
            KeyCode::Char('3') => self.set_tab(Tab::Songs),
            KeyCode::Char('4') => self.set_tab(Tab::Playlists),
            KeyCode::Tab => {
                let i = Tab::ALL.iter().position(|&t| t == self.tab).unwrap_or(0);
                self.set_tab(Tab::ALL[(i + 1) % Tab::ALL.len()]);
            }
            KeyCode::BackTab => {
                let i = Tab::ALL.iter().position(|&t| t == self.tab).unwrap_or(0);
                self.set_tab(Tab::ALL[(i + Tab::ALL.len() - 1) % Tab::ALL.len()]);
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(20),
            KeyCode::PageUp => self.move_by(-20),
            KeyCode::Char('d') if ctrl => self.move_by(20),
            KeyCode::Char('u') if ctrl => self.move_by(-20),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter if matches!(self.cols[self.focus].selected(), Some(Item::Track(_))) => self.play_selected(),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                if self.focus + 1 < self.cols.len() {
                    self.focus += 1;
                    self.refresh_art();
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => {
                if self.focus > 0 {
                    self.focus -= 1;
                    self.refresh_art();
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
            }
            KeyCode::Char('a') => {
                self.open_import();
            }
            KeyCode::Char('i') => self.open_editor(),
            KeyCode::Char('C') => self.open_fix(),
            KeyCode::Char('d') if !ctrl => self.open_download(false),
            KeyCode::Char('S') => self.open_download(true),
            KeyCode::Char('c') if !ctrl && self.can_pick_cover() => {
                if let Some((title, tracks)) = self.edit_targets() {
                    let tracks = tracks.iter().map(|&t| self.ipod.db.tracks[t].clone()).collect();
                    self.open_cover_picker(title, tracks);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(Item::Track(t)) = self.cols[self.focus].selected() {
                    if !self.marked.remove(&t) {
                        self.marked.insert(t);
                    }
                    self.move_by(1);
                }
            }
            KeyCode::Char('e') => {
                // An open song file would keep the iPod from unmounting.
                self.player.stop();
                self.status = Some(match crate::device::eject(&self.ipod.root) {
                    Ok(()) => "iPod ejected — safe to unplug.".into(),
                    Err(e) => format!("Eject failed: {e:#}"),
                });
            }
            _ => {}
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        let col = &mut self.cols[self.focus];
        match key.code {
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filtering = false;
                col.filter.clear();
            }
            KeyCode::Backspace => {
                col.filter.pop();
            }
            KeyCode::Char(c) => col.filter.push(c),
            _ => return,
        }
        self.apply_filter();
    }

    fn apply_filter(&mut self) {
        let needle = self.cols[self.focus].filter.to_lowercase();
        let all = self.cols[self.focus].all.clone();
        let items: Vec<Item> = if needle.is_empty() {
            all
        } else {
            all.into_iter().filter(|&it| self.search_text(it).to_lowercase().contains(&needle)).collect()
        };
        let col = &mut self.cols[self.focus];
        col.state.select((!items.is_empty()).then_some(0));
        col.items = items;
        self.rebuild_from(self.focus);
    }

    fn search_text(&self, item: Item) -> String {
        match item {
            Item::Artist(a) => self.index.artists[a].name.clone(),
            Item::Album(a) => format!("{} {}", self.index.albums[a].title, self.index.albums[a].artist),
            Item::Playlist(p) => self.ipod.db.playlists[p].name.clone(),
            Item::Track(t) => {
                let t = &self.ipod.db.tracks[t];
                format!("{} {} {}", t.title, t.artist, t.album)
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        let col = &mut self.cols[self.focus];
        if col.items.is_empty() {
            return;
        }
        let cur = col.state.selected().unwrap_or(0) as isize;
        let next = cur.saturating_add(delta).clamp(0, col.items.len() as isize - 1) as usize;
        if Some(next) != col.state.selected() {
            col.state.select(Some(next));
            self.rebuild_from(self.focus);
        }
    }
}
