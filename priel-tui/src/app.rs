// SPDX-License-Identifier: GPL-3.0-or-later
//
// priel — hi-res terminal client for TIDAL
// Copyright (C) 2026 Guy Boldon
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Application state + input handling (VIM keys + first-class mouse), multiple
//! views (favorites / playlists / playlist tracks / search) and the gapless
//! play-queue orchestration.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use rand::Rng;
use ratatui::layout::Rect;

use priel_core::{Playlist, Track};
use priel_player::{PlaybackStatus, Player};

use crate::worker::{self, FromWorker, ToWorker, Worker};

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum View {
    Favorites,
    Playlists,
    PlaylistTracks,
    Search,
}

/// A clickable region recorded by the renderer. Mouse support is a headline
/// feature, so every control that has a key binding also has a hit box: the
/// renderer knows the geometry, and only the renderer should have to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    View(View),
    PlayPause,
    Prev,
    Next,
    Shuffle,
    VolUp,
    VolDown,
    Quit,
}

#[derive(PartialEq)]
pub enum Mode {
    Normal,
    Filter, // local filter of the current list
    Search, // editing the global TIDAL search query
}

/// Source-side metadata (from the TIDAL API — authoritative).
#[derive(Default, Clone)]
pub struct StreamMeta {
    pub bit_depth: u32,
    pub sample_rate: u32,
    pub codec: String,
    pub quality: String,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent UI flags; grouping them would not simplify any call site"
)]
pub struct App {
    pub player: Player,
    worker: Worker,

    // View data.
    pub view: View,
    pub favorites: Vec<Track>,
    pub playlists: Vec<Playlist>,
    pub playlist_tracks: Vec<Track>,
    pub open_playlist: Option<(String, String)>, // (uuid, title)
    pub search_tracks: Vec<Track>,
    pub search_query: String,

    pub selected: usize,
    pub list_offset: usize,

    pub now_playing: Option<Track>,
    pub now_meta: StreamMeta,
    pub status: PlaybackStatus,

    // Play queue + gapless pipeline state.
    pub queue: Vec<Track>,
    pub queue_pos: usize,
    expected_id: u64,
    current_target: Option<u64>,
    next_intended: Option<u64>,
    metas: HashMap<u64, StreamMeta>,
    advanced: bool,

    pub mode: Mode,
    pub filter: String,
    pub shuffle: bool,

    pub notice: Option<String>,
    pub loading: bool,
    pub frame: usize,
    pub should_quit: bool,

    pub list_inner: Rect,
    pub progress_rect: Rect,
    /// Clickable regions, rebuilt by the renderer every frame.
    pub hits: Vec<(Rect, Hit)>,
    last_click: Option<(u16, Instant)>,
    dirty: bool,
    last_sig: RenderSig,
}

/// Snapshot of the render-relevant state that moves on its own.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a change-detection snapshot; the fields mirror PlaybackStatus"
)]
struct RenderSig {
    position: u64,
    duration: u64,
    paused: bool,
    playing: bool,
    loaded: bool,
    volume: u32,
    current_id: u64,
    has_next: bool,
    cache_secs: u32,
    sample_rate: u32,
    spinner: usize,
}

impl App {
    pub fn new(device: Option<String>, token_path: String) -> anyhow::Result<Self> {
        let player = Player::new(device)?;
        let worker = worker::spawn(token_path);
        Ok(Self {
            player,
            worker,
            view: View::Favorites,
            favorites: Vec::new(),
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            open_playlist: None,
            search_tracks: Vec::new(),
            search_query: String::new(),
            selected: 0,
            list_offset: 0,
            now_playing: None,
            now_meta: StreamMeta::default(),
            status: PlaybackStatus::default(),
            queue: Vec::new(),
            queue_pos: 0,
            expected_id: 0,
            current_target: None,
            next_intended: None,
            metas: HashMap::new(),
            advanced: false,
            mode: Mode::Normal,
            filter: String::new(),
            shuffle: false,
            notice: Some("Loading favorites…".into()),
            loading: true,
            frame: 0,
            should_quit: false,
            list_inner: Rect::default(),
            progress_rect: Rect::default(),
            hits: Vec::new(),
            last_click: None,
            dirty: true,
            last_sig: RenderSig::default(),
        })
    }

    pub fn start(&self) {
        let _ = self.worker.tx.send(ToWorker::LoadFavorites);
    }

    /// Track slice backing the current view (empty for the Playlists list).
    pub fn current_tracks(&self) -> &[Track] {
        match self.view {
            View::Favorites => &self.favorites,
            View::PlaylistTracks => &self.playlist_tracks,
            View::Search => &self.search_tracks,
            View::Playlists => &[],
        }
    }

    /// Indices into the current view's items matching the local filter.
    ///
    /// Callers on the render path must call this **once per frame** and reuse
    /// the result: it allocates and walks the whole list, so calling it per row
    /// is quadratic.
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        if self.view == View::Playlists {
            self.playlists
                .iter()
                .enumerate()
                .filter(|(_, p)| row_matches(&p.title, "", &f))
                .map(|(i, _)| i)
                .collect()
        } else {
            self.current_tracks()
                .iter()
                .enumerate()
                .filter(|(_, t)| row_matches(&t.title, &t.artist, &f))
                .map(|(i, _)| i)
                .collect()
        }
    }

    fn clamp_selection(&mut self) {
        let n = self.visible().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    pub fn drain_worker(&mut self) {
        while let Ok(msg) = self.worker.rx.try_recv() {
            self.dirty = true;
            match msg {
                FromWorker::Favorites(t) => {
                    self.favorites = t;
                    self.loading = false;
                    self.notice = Some(format!("{} favorites", self.favorites.len()));
                    self.clamp_selection();
                }
                FromWorker::Playlists(p) => {
                    self.playlists = p;
                    self.loading = false;
                    if self.view == View::Playlists {
                        self.clamp_selection();
                    }
                }
                FromWorker::PlaylistTracks(uuid, t) => {
                    if self.open_playlist.as_ref().is_some_and(|(u, _)| u == &uuid) {
                        self.playlist_tracks = t;
                        self.loading = false;
                        self.clamp_selection();
                    }
                }
                FromWorker::SearchResults(r) => {
                    self.search_tracks = r.tracks;
                    self.loading = false;
                    self.selected = 0;
                    self.notice = Some(format!("{} results", self.search_tracks.len()));
                }
                FromWorker::Resolved(id, r) => {
                    let meta = StreamMeta {
                        bit_depth: r.bit_depth,
                        sample_rate: r.sample_rate,
                        codec: r.codec.clone(),
                        quality: r.quality.clone(),
                    };
                    self.metas.insert(id, meta);
                    if self.current_target == Some(id) {
                        self.player.play_now(id, r.source.clone());
                        self.current_target = None;
                        self.expected_id = id;
                        self.advanced = false;
                        self.now_meta = self.metas.get(&id).cloned().unwrap_or_default();
                        self.schedule_next();
                    } else if self.next_intended == Some(id) {
                        self.player.append_next(id, r.source.clone());
                    }
                }
                FromWorker::Error(e) => {
                    self.notice = Some(format!("⚠ {e}"));
                    self.loading = false;
                }
            }
        }
    }

    /// Mark the screen as needing a redraw on the next loop iteration.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Take the pending-redraw flag. The loop draws only when this is true, so
    /// an idle or paused player costs no rendering at all.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Everything the view derives from state that changes without user input.
    /// Seconds are whole because that is the finest granularity the progress bar
    /// and its label can show.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "display granularity: whole seconds and whole percent"
    )]
    fn render_sig(&self) -> RenderSig {
        let s = &self.status;
        RenderSig {
            position: s.position as u64,
            duration: s.duration as u64,
            paused: s.paused,
            playing: s.playing,
            loaded: s.loaded,
            volume: s.volume as u32,
            current_id: s.current_id,
            has_next: s.has_next,
            cache_secs: s.cache_secs as u32,
            sample_rate: s.sample_rate,
            // While a spinner is on screen it must keep animating.
            spinner: if self.is_resolving() || self.is_buffering() {
                self.frame
            } else {
                0
            },
        }
    }

    pub fn refresh(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.status = self.player.status();

        let sig = self.render_sig();
        if sig != self.last_sig {
            self.last_sig = sig;
            self.dirty = true;
        }

        // Clear the advance guard only once real audio is flowing on a settled
        // track. (Not in load_fresh — that caused the fallback to re-fire every
        // tick while `ended` was still true, running away through the queue.)
        if self.status.playing && !self.status.ended {
            self.advanced = false;
        }

        let cur = self.status.current_id;
        if cur != 0 && cur != self.expected_id {
            // mpv advanced to a track we didn't explicitly load (gapless).
            self.expected_id = cur;
            self.advanced = true; // we just advanced — suppress the end-fallback
            if let Some(p) = self.queue.iter().position(|t| t.id == cur) {
                self.queue_pos = p;
                self.now_playing = Some(self.queue[p].clone());
            }
            self.now_meta = self.metas.get(&cur).cloned().unwrap_or_default();
            self.next_intended = None;
            self.dirty = true;
            self.schedule_next();
        }

        // Keep the next preloaded once the current has loaded.
        if self.now_playing.is_some()
            && self.current_target.is_none()
            && !self.status.has_next
            && self.next_intended.is_none()
        {
            self.schedule_next();
        }

        // Fallback: playback genuinely stopped (idle) with no preloaded next —
        // end of queue, or a failed preload. Guarded with `!playing` so it never
        // fires during a healthy gapless transition (where audio IS playing).
        if self.status.ended
            && !self.status.has_next
            && !self.advanced
            && self.now_playing.is_some()
            && !self.status.playing
            && !self.status.paused
            && self.current_target.is_none()
        {
            self.advanced = true;
            self.advance_fresh();
        }
    }

    // ---- views ----

    fn switch_view(&mut self, v: View) {
        self.view = v;
        self.selected = 0;
        self.list_offset = 0;
        self.filter.clear();
        match v {
            View::Playlists if self.playlists.is_empty() => {
                self.loading = true;
                let _ = self.worker.tx.send(ToWorker::LoadPlaylists);
            }
            View::Search if self.search_tracks.is_empty() => {
                self.mode = Mode::Search; // start typing a query
            }
            _ => {}
        }
    }

    fn cycle_view(&mut self) {
        let next = match self.view {
            View::Favorites => View::Playlists,
            View::Playlists | View::PlaylistTracks => View::Search,
            View::Search => View::Favorites,
        };
        self.switch_view(next);
    }

    fn open_selected_playlist(&mut self) {
        let vis = self.visible();
        if let Some(&idx) = vis.get(self.selected)
            && let Some(p) = self.playlists.get(idx).cloned()
        {
            self.open_playlist = Some((p.uuid.clone(), p.title.clone()));
            self.view = View::PlaylistTracks;
            self.playlist_tracks.clear();
            self.selected = 0;
            self.list_offset = 0;
            self.filter.clear();
            self.loading = true;
            let _ = self.worker.tx.send(ToWorker::LoadPlaylistTracks(p.uuid));
        }
    }

    fn on_enter(&mut self) {
        match self.view {
            View::Playlists => self.open_selected_playlist(),
            _ => self.play_selected(),
        }
    }

    // ---- navigation ----

    fn move_down(&mut self, by: usize) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + by).min(n - 1);
        }
    }
    fn move_up(&mut self, by: usize) {
        self.selected = self.selected.saturating_sub(by);
    }
    fn half_page(&self) -> usize {
        (self.list_inner.height as usize / 2).max(1)
    }
    fn full_page(&self) -> usize {
        (self.list_inner.height as usize).max(1)
    }

    // ---- queue + gapless playback ----

    fn start_queue_at(&mut self, vis_index: usize) {
        let vis = self.visible();
        if vis.is_empty() {
            return;
        }
        let tracks: Vec<Track> = {
            let items = self.current_tracks();
            if items.is_empty() {
                return;
            }
            vis.iter().filter_map(|&i| items.get(i).cloned()).collect()
        };
        self.queue = tracks;
        let p = vis_index.min(self.queue.len() - 1);
        self.load_fresh(p);
    }

    fn load_fresh(&mut self, pos: usize) {
        if pos >= self.queue.len() {
            return;
        }
        self.queue_pos = pos;
        self.next_intended = None;
        let t = self.queue[pos].clone();
        self.current_target = Some(t.id);
        self.expected_id = t.id;
        self.now_playing = Some(t.clone());
        self.now_meta = StreamMeta::default();
        let _ = self.worker.tx.send(ToWorker::Resolve(t.id));
    }

    fn schedule_next(&mut self) {
        if self.queue.is_empty() {
            self.next_intended = None;
            return;
        }
        let next = if self.shuffle {
            Some(self.rand_other())
        } else if self.queue_pos + 1 < self.queue.len() {
            Some(self.queue_pos + 1)
        } else {
            None
        };
        match next {
            Some(p) => {
                let id = self.queue[p].id;
                self.next_intended = Some(id);
                let _ = self.worker.tx.send(ToWorker::Resolve(id));
            }
            None => self.next_intended = None,
        }
    }

    fn advance_fresh(&mut self) {
        if self.shuffle {
            let p = self.rand_other();
            self.load_fresh(p);
        } else if self.queue_pos + 1 < self.queue.len() {
            self.load_fresh(self.queue_pos + 1);
        }
    }

    fn rand_other(&self) -> usize {
        if self.queue.len() <= 1 {
            return self.queue_pos.min(self.queue.len().saturating_sub(1));
        }
        let mut r = rand::thread_rng().gen_range(0..self.queue.len());
        if r == self.queue_pos {
            r = (r + 1) % self.queue.len();
        }
        r
    }

    fn play_selected(&mut self) {
        self.start_queue_at(self.selected);
    }

    fn user_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.status.has_next {
            self.player.skip_next();
        } else if self.shuffle {
            let p = self.rand_other();
            self.load_fresh(p);
        } else if self.queue_pos + 1 < self.queue.len() {
            self.load_fresh(self.queue_pos + 1);
        }
    }

    fn user_prev(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.status.position > 3.0 {
            self.player.seek(0.0);
            return;
        }
        let p = if self.shuffle {
            self.rand_other()
        } else {
            self.queue_pos.saturating_sub(1)
        };
        self.load_fresh(p);
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.notice = Some(
            if self.shuffle {
                "Shuffle ON"
            } else {
                "Shuffle OFF"
            }
            .into(),
        );
        if self.shuffle && self.now_playing.is_none() {
            // Build a queue from the current track view and start randomly.
            let tracks: Vec<Track> = {
                let vis = self.visible();
                let items = self.current_tracks();
                vis.iter().filter_map(|&i| items.get(i).cloned()).collect()
            };
            if !tracks.is_empty() {
                self.queue = tracks;
                let p = rand::thread_rng().gen_range(0..self.queue.len());
                self.load_fresh(p);
            }
        }
    }

    fn volume_step(&self, delta: f64) {
        self.player.set_volume(self.status.volume + delta);
    }

    // ---- input ----

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.dirty = true;
        match self.mode {
            Mode::Filter => self.on_key_filter(key),
            Mode::Search => self.on_key_search(key),
            Mode::Normal => self.on_key_normal(key),
        }
    }

    fn on_key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.clamp_selection();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let q = self.search_query.trim().to_string();
                if !q.is_empty() {
                    self.loading = true;
                    self.search_tracks.clear();
                    self.selected = 0;
                    let _ = self.worker.tx.send(ToWorker::Search(q));
                }
            }
            KeyCode::Backspace => {
                self.search_query.pop();
            }
            KeyCode::Char(c) => self.search_query.push(c),
            _ => {}
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => self.move_down(self.half_page()),
                KeyCode::Char('u') => self.move_up(self.half_page()),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.view == View::PlaylistTracks {
                    self.switch_view(View::Playlists);
                }
            }
            KeyCode::Tab => self.cycle_view(),
            KeyCode::Char('1') => self.switch_view(View::Favorites),
            KeyCode::Char('2') => self.switch_view(View::Playlists),
            KeyCode::Char('3') => self.switch_view(View::Search),
            // Re-edit the search query while in the Search view.
            KeyCode::Char('i') if self.view == View::Search => self.mode = Mode::Search,
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::Char('J') => self.move_down(self.full_page()),
            KeyCode::Char('K') => self.move_up(self.full_page()),
            KeyCode::Char('g') => self.selected = 0,
            KeyCode::Char('G') => self.selected = self.visible().len().saturating_sub(1),
            KeyCode::Enter => self.on_enter(),
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('n' | 'L') => self.user_next(),
            KeyCode::Char('p' | 'H') => self.user_prev(),
            KeyCode::Char('h') | KeyCode::Left => self.player.seek_relative(-5.0),
            KeyCode::Char('l') | KeyCode::Right => self.player.seek_relative(5.0),
            KeyCode::Char('+' | '=') => self.volume_step(5.0),
            KeyCode::Char('-') => self.volume_step(-5.0),
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.filter.clear();
                self.selected = 0;
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => self.move_down(1),
            MouseEventKind::ScrollUp => self.move_up(1),
            MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
            MouseEventKind::Drag(MouseButton::Left) if hit(self.progress_rect, m.column, m.row) => {
                self.seek_to_x(m.column);
            }
            // Everything else, motion in particular, changes nothing on screen.
            // Redrawing on it turns a mouse sweep into a render storm.
            _ => return,
        }
        self.dirty = true;
    }

    /// Act on a control the renderer registered a hit box for.
    fn dispatch(&mut self, h: Hit) {
        match h {
            Hit::View(v) => self.switch_view(v),
            Hit::PlayPause => self.player.toggle_pause(),
            Hit::Prev => self.user_prev(),
            Hit::Next => self.user_next(),
            Hit::Shuffle => self.toggle_shuffle(),
            Hit::VolUp => self.volume_step(5.0),
            Hit::VolDown => self.volume_step(-5.0),
            Hit::Quit => self.should_quit = true,
        }
    }

    fn on_click(&mut self, col: u16, row: u16) {
        if let Some(h) = self
            .hits
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, h)| *h)
        {
            self.dispatch(h);
            return;
        }
        if hit(self.progress_rect, col, row) {
            self.seek_to_x(col);
            return;
        }
        if hit(self.list_inner, col, row) {
            let vis_len = self.visible().len();
            let vi = self.list_offset + (row - self.list_inner.y) as usize;
            if vi < vis_len {
                let now = Instant::now();
                let is_double = matches!(
                    self.last_click,
                    Some((prow, t)) if prow == row && now.duration_since(t) < Duration::from_millis(400)
                );
                self.selected = vi;
                if is_double {
                    self.on_enter();
                    self.last_click = None;
                } else {
                    self.last_click = Some((row, now));
                }
            }
        }
    }

    fn seek_to_x(&self, col: u16) {
        let pr = self.progress_rect;
        if pr.width == 0 || self.status.duration <= 0.0 {
            return;
        }
        let rel = f64::from(col.saturating_sub(pr.x)) / f64::from(pr.width);
        self.player.seek(rel.clamp(0.0, 1.0) * self.status.duration);
    }

    pub fn queue_indicator(&self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(format!("{}/{}", self.queue_pos + 1, self.queue.len()))
        }
    }

    pub fn is_resolving(&self) -> bool {
        self.current_target.is_some()
    }

    pub fn is_buffering(&self) -> bool {
        self.now_playing.is_some()
            && self.current_target.is_none()
            && !self.status.playing
            && !self.status.paused
            && !self.status.ended
    }

    pub fn spinner(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        FRAMES[self.frame % FRAMES.len()]
    }
}

fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

/// Does a row match the local filter? `filter_lower` must already be lowercased
/// by the caller, which is what keeps this off the per-row allocation path.
/// An empty filter matches everything; `secondary` may be empty for item kinds
/// that only have one searchable field.
fn row_matches(primary: &str, secondary: &str, filter_lower: &str) -> bool {
    filter_lower.is_empty()
        || primary.to_lowercase().contains(filter_lower)
        || (!secondary.is_empty() && secondary.to_lowercase().contains(filter_lower))
}

#[cfg(test)]
mod tests {
    use super::row_matches;

    #[test]
    fn empty_filter_matches_every_row() {
        // Goal: an unfiltered list must show everything, including empty fields.
        assert!(row_matches("Kind of Blue", "Miles Davis", ""));
        assert!(row_matches("", "", ""));
    }

    #[test]
    fn matching_is_case_insensitive_on_both_fields() {
        // Goal: pin the documented semantics - either field may match, and the
        // caller-supplied filter is already lowercase while the row data is not.
        assert!(row_matches("Kind of Blue", "Miles Davis", "blue"));
        assert!(row_matches("Kind of Blue", "Miles Davis", "miles"));
        assert!(row_matches("KIND OF BLUE", "", "kind"));
    }

    #[test]
    fn substrings_match_anywhere_but_non_matches_do_not() {
        // Goal: it is a `contains` filter, not a prefix one, and it really can
        // reject - a filter that always matched would look identical in the UI.
        assert!(row_matches("Kind of Blue", "Miles Davis", "of b"));
        assert!(!row_matches("Kind of Blue", "Miles Davis", "coltrane"));
    }

    #[test]
    fn empty_secondary_field_is_not_searched() {
        // Goal: playlists pass "" as the second field; that must not turn into a
        // match for every possible filter via the empty-string-contains rule.
        assert!(!row_matches("Evening", "", "z"));
    }
}
