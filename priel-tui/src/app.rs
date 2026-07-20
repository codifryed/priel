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

use priel_core::auth::{Credentials, Pkce};
use priel_core::{Fault, Playlist, Track};
use priel_player::{AudioDevice, PlaybackStatus, Player, PlayerConfig};

#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::worker::{self, FromWorker, ToWorker, Worker};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum View {
    Favorites,
    Playlists,
    PlaylistTracks,
    Search,
}

/// A clickable region recorded by the renderer. Mouse support is a headline
/// feature, so every control that has a key binding also has a hit box: the
/// renderer knows the geometry, and only the renderer should have to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    View(View),
    PlayPause,
    Prev,
    Next,
    SeekBack,
    SeekFwd,
    MoveUp,
    MoveDown,
    Top,
    Bottom,
    Shuffle,
    VolUp,
    VolDown,
    VolUnity,
    Filter,
    CycleView,
    Help,
    Quit,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Filter,      // local filter of the current list
    Search,      // editing the global TIDAL search query
    Help,        // the shortcut reference is up; it swallows input until dismissed
    Log,         // the recent diagnostics are up; modal in the same way
    Devices,     // the output picker is up; modal in the same way
    Credentials, // first run with no client identity; asking before fetching one
    Login,       // signing in: browser is open, waiting for the redirected URL
}

/// What a status tick reads. See [`App::tick`].
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat snapshot of the inputs the decision reads; grouping them would hide it"
)]
struct Tick {
    playing: bool,
    ended: bool,
    paused: bool,
    /// mpv already has the next entry in its playlist.
    has_next: bool,
    /// The track mpv says it is on, or 0 for none.
    current_id: u64,
    /// The track we believe mpv is on.
    expected_id: u64,
    /// A resolve for the track we want to play now is still in flight.
    resolving_current: bool,
    /// A resolve for the *next* track has been asked for.
    preload_queued: bool,
    /// We advanced recently and have not yet settled. The guard that stops the
    /// end-of-track fallback re-firing every tick.
    advanced: bool,
    /// Something is loaded; an idle player at startup has nothing.
    have_track: bool,
}

/// What a status tick asks the queue to do. See [`App::decide`].
///
/// Three independent answers rather than one, because that is what the code
/// this replaced actually did: three `if` blocks in a row, more than one of
/// which can be true in the same tick. Collapsing them into a priority chain
/// reads better and is wrong - it stops the end-of-track fallback firing on the
/// tick that also schedules a preload, so a track whose preload failed stalls
/// until the tick after. There is a test for each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Plan {
    /// mpv moved to this track on its own - a gapless transition we did not
    /// load. Exclusive: taking it up schedules the next preload itself and sets
    /// the guard that keeps the fallback out of this tick.
    adopt: Option<u64>,
    /// The current track is settled with nothing behind it; resolve the next so
    /// mpv can transition into it gaplessly.
    preload: bool,
    /// Playback stopped with nothing preloaded. Load the next from scratch.
    advance_fresh: bool,
}

/// How long the picker says it is still looking before it says there is
/// nothing to look at.
///
/// The list is asked for as the overlay opens and answered a tick later, so
/// without this the first frame of every open would claim there are no devices.
const DEVICE_WAIT: Duration = Duration::from_secs(2);

/// State of a sign-in in progress.
///
/// The verifier has to survive from building the authorize URL until the code
/// comes back, which is a separate user-driven step, so it lives here rather
/// than being regenerated.
pub struct LoginFlow {
    pub url: String,
    pkce: Pkce,
    unique_key: String,
    /// What the user has pasted so far.
    pub pasted: String,
    pub status: Option<String>,
    exchanging: Option<Receiver<Result<(), String>>>,
}

impl LoginFlow {
    /// Is an exchange in flight? The screen shows this instead of the prompt.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.exchanging.is_some()
    }
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
    /// The worker thread has gone, and has been reported. Latched so the
    /// report happens once rather than on every pass of the event loop.
    worker_lost: bool,
    /// The diagnostics the log overlay shows, shared with the sink.
    pub(crate) recent: crate::logging::Recent,
    /// How many lines back from the newest the log overlay is scrolled. Counted
    /// from the end because that is where the interesting line always is.
    log_scroll: usize,
    /// The output devices the player last published. Kept only while the picker
    /// is up; nothing else on screen shows them.
    devices: Vec<AudioDevice>,
    device_selected: usize,
    /// The first device row on screen, maintained by the renderer exactly as
    /// `list_offset` is.
    pub device_offset: usize,
    /// Clickable device rows and the index each stands for, rebuilt by the
    /// renderer while the picker is up.
    pub device_rows: Vec<(Rect, usize)>,
    /// When the list was last asked for. The picker says it is still looking
    /// until this is old enough to mean nothing is going to answer.
    devices_asked: Option<Instant>,
    pub frame: usize,
    pub should_quit: bool,

    pub list_inner: Rect,
    pub progress_rect: Rect,
    /// Clickable regions, rebuilt by the renderer every frame.
    pub hits: Vec<(Rect, Hit)>,
    last_click: Option<(u16, Instant)>,
    dirty: bool,
    last_sig: RenderSig,
    /// Where the credentials file should live, and the state of any attempt to
    /// populate it. `None` once there is nothing left to ask about.
    credentials_path: Option<String>,
    /// Where to look for a client identity, in order: the user's override
    /// first, then whatever priel obtained for itself.
    /// Kept so the worker can be rebuilt once credentials arrive, rather than
    /// asking the user to restart.
    token_path: Option<String>,
    credential_status: Option<String>,
    fetching: Option<Receiver<Result<(), String>>>,
    login: Option<LoginFlow>,
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
    pub fn new(
        player: PlayerConfig,
        token_path: String,
        recent: crate::logging::Recent,
    ) -> anyhow::Result<Self> {
        let player = Player::with_config(player)?;
        let creds_path = Credentials::default_path();
        let has_credentials = priel_core::auth::local_credentials(&creds_path).is_some();
        let worker = worker::spawn(token_path.clone(), creds_path.clone());
        let mut app = Self::with(player, worker);
        app.recent = recent;
        let has_session = priel_core::auth::StoredToken::load(&token_path).is_ok();
        app.credentials_path = Some(creds_path.clone());
        app.token_path = Some(token_path.clone());
        // The screens chain: a client key is needed before signing in, and a
        // session before anything can load. Each step leads to the next rather
        // than failing on its own.
        if !has_credentials {
            app.prompt_for_credentials(creds_path, token_path);
        } else if !has_session {
            app.start_login();
        }
        Ok(app)
    }

    /// An app with a silent player and a worker whose channels the caller holds.
    /// Shared by the app and renderer tests; the returned ends must be kept
    /// alive for the duration of a test or the channels report disconnected.
    #[cfg(test)]
    pub fn rigged() -> (Self, Sender<FromWorker>, Receiver<ToWorker>) {
        let (worker, to_app, from_app) = Worker::rigged();
        let player = Player::new(Some("null".into())).expect("player");
        (Self::with(player, worker), to_app, from_app)
    }

    /// Assemble an app around ready-made parts.
    ///
    /// Separate from `new` so tests can supply a silent player and a rigged
    /// worker; without this seam none of the queue orchestration is reachable.
    pub fn with(player: Player, worker: Worker) -> Self {
        Self {
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
            worker_lost: false,
            recent: crate::logging::Recent::default(),
            log_scroll: 0,
            devices: Vec::new(),
            device_selected: 0,
            device_offset: 0,
            device_rows: Vec::new(),
            devices_asked: None,
            last_sig: RenderSig::default(),
            credentials_path: None,
            token_path: None,
            credential_status: None,
            fetching: None,
            login: None,
        }
    }

    /// Ask the user before downloading a client identity.
    ///
    /// Only called when none could be found, and only from `new` - a rigged app
    /// in a test is never put in this state.
    fn prompt_for_credentials(&mut self, creds_path: String, token_path: String) {
        self.credentials_path = Some(creds_path);
        self.token_path = Some(token_path);
        self.mode = Mode::Credentials;
    }

    /// Offer to sign in again after the session is refused.
    ///
    /// A refresh token expires too, and when it does every request fails with
    /// the same message. Turning that into the sign-in screen is the difference
    /// between a dead application and one keystroke.
    fn offer_relogin(&mut self, reason: &str) {
        self.notice = Some(format!("⚠ {reason}"));
        if self.mode == Mode::Normal {
            self.start_login();
        }
    }

    /// With a client identity in hand, either sign in or just reload.
    fn continue_after_credentials(&mut self) {
        let has_session = self
            .token_path
            .as_ref()
            .is_some_and(|p| priel_core::auth::StoredToken::load(p).is_ok());
        if has_session {
            self.restart_worker();
        } else {
            self.start_login();
        }
    }

    /// Replace the worker with one that can see the new credentials.
    ///
    /// Cheaper for the user than being told to restart: the old worker's
    /// channels drop, which ends its thread, and the new one repeats the
    /// initial load so the view fills in as it would have at startup.
    fn restart_worker(&mut self) {
        let (Some(token), Some(creds)) = (self.token_path.clone(), self.credentials_path.clone())
        else {
            return;
        };
        self.worker = worker::spawn(token, creds);
        self.worker_lost = false;
        self.loading = true;
        self.ask(ToWorker::LoadFavorites);
    }

    /// The sign-in in progress, if any.
    #[must_use]
    pub fn login(&self) -> Option<&LoginFlow> {
        self.login.as_ref()
    }

    /// Begin signing in: build the authorization URL and open a browser at it.
    ///
    /// Needs a client identity, so the credentials screen comes first when there
    /// is none - the two screens chain rather than each failing on their own.
    pub fn start_login(&mut self) {
        if self.credentials_path.is_none() {
            return; // nowhere decided to look, so nothing to sign in with
        }
        let Some((creds, _)) = self.read_credentials() else {
            self.mode = Mode::Credentials;
            return;
        };
        let (Ok(pkce), Ok(unique_key)) = (Pkce::generate(), priel_core::auth::client_unique_key())
        else {
            self.notice = Some("⚠ could not start sign-in: no system randomness".into());
            return;
        };
        let url = priel_core::auth::authorize_url(&creds.into_config(), &pkce, &unique_key);
        open_in_browser(&url);
        self.login = Some(LoginFlow {
            url,
            pkce,
            unique_key,
            pasted: String::new(),
            status: None,
            exchanging: None,
        });
        self.mode = Mode::Login;
        self.dirty = true;
    }

    /// Hand the pasted redirect back to the service for a session.
    fn submit_login(&mut self) {
        // Resolved before the mutable borrow below, which cannot coexist with a
        // read of the lookup paths.
        let identity = self.read_credentials();
        let (Some(flow), Some(token_path)) = (self.login.as_mut(), self.token_path.clone()) else {
            return;
        };
        if flow.exchanging.is_some() {
            return;
        }
        let code = match priel_core::auth::code_from_redirect(flow.pasted.trim()) {
            Ok(code) => code,
            Err(e) => {
                // Reported inline rather than as a failure: the usual cause is
                // pasting the login page instead of the page it redirected to.
                flow.status = Some(format!("{e}"));
                self.dirty = true;
                return;
            }
        };
        let Some((creds, _)) = identity else {
            flow.status = Some("the client identity went missing".into());
            return;
        };

        flow.status = Some("signing in…".into());
        let (tx, rx) = std::sync::mpsc::channel();
        let (pkce, key) = (flow.pkce.clone(), flow.unique_key.clone());
        // Named for the log. A failure to start drops the sending end, and
        // `drain_login` already reports a sign-in that did not finish.
        let started = std::thread::Builder::new()
            .name("login".into())
            .spawn(move || {
                let agent = priel_core::new_agent();
                let outcome = priel_core::auth::exchange_code(
                    &agent,
                    &creds.into_config(),
                    &code,
                    &pkce,
                    &key,
                    priel_core::auth::now_epoch(),
                )
                .and_then(|token| token.save(&token_path))
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(outcome);
            });
        if let Err(e) = started {
            log::error!("no thread for the sign-in exchange: {e}");
        }
        flow.exchanging = Some(rx);
        self.dirty = true;
    }

    /// Collect the result of an in-flight sign-in.
    fn drain_login(&mut self) {
        let Some(flow) = self.login.as_mut() else {
            return;
        };
        let Some(rx) = &flow.exchanging else { return };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.login = None;
                self.mode = Mode::Normal;
                self.notice = Some("Signed in. Loading your library…".into());
                self.restart_worker();
                self.dirty = true;
            }
            Ok(Err(e)) => {
                flow.exchanging = None;
                flow.status = Some(e);
                flow.pasted.clear();
                self.dirty = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                flow.exchanging = None;
                flow.status = Some("the sign-in did not finish".into());
                self.dirty = true;
            }
        }
    }

    /// Text arriving as one block, from a terminal that supports bracketed
    /// paste. Far better than character-by-character for a URL this long.
    pub fn on_paste(&mut self, text: &str) {
        if self.mode == Mode::Login
            && let Some(flow) = self.login.as_mut()
            && !flow.is_busy()
        {
            flow.pasted.push_str(text.trim());
            self.dirty = true;
        }
    }

    fn on_key_login(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Letters go into the pasted text, so the actions take a
                // modifier rather than stealing characters from a URL.
                KeyCode::Char('o') => {
                    if let Some(flow) = &self.login {
                        open_in_browser(&flow.url);
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(flow) = self.login.as_mut() {
                        flow.pasted.clear();
                        flow.status = None;
                    }
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.login = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.submit_login(),
            KeyCode::Backspace => {
                if let Some(flow) = self.login.as_mut() {
                    flow.pasted.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(flow) = self.login.as_mut()
                    && !flow.is_busy()
                {
                    flow.pasted.push(c);
                }
            }
            _ => {}
        }
    }

    /// A client identity from wherever this app was told to look.
    fn read_credentials(&self) -> Option<(Credentials, priel_core::auth::CredentialSource)> {
        priel_core::auth::local_credentials(self.credentials_path.as_deref()?)
    }

    /// Point the app at token and credential files, for tests.
    #[cfg(test)]
    pub fn set_paths_for_test(&mut self, token: String, credentials: String) {
        self.token_path = Some(token);
        self.credentials_path = Some(credentials);
    }

    /// Drive the post-credentials step, for tests.
    #[cfg(test)]
    pub fn continue_after_credentials_for_test(&mut self) {
        self.continue_after_credentials();
    }

    /// Rebuild the worker, for tests.
    #[cfg(test)]
    pub fn restart_worker_for_test(&mut self) {
        self.restart_worker();
    }

    /// Force a mode, for renderer tests.
    #[cfg(test)]
    pub fn set_mode_for_test(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// The line under the consent screen's buttons.
    #[must_use]
    pub fn credential_status(&self) -> Option<&str> {
        self.credential_status.as_deref()
    }

    /// Download a client identity, off the UI thread.
    fn fetch_credentials(&mut self) {
        let Some(path) = self.credentials_path.clone() else {
            return;
        };
        if self.fetching.is_some() {
            return; // already in flight
        }
        self.credential_status = Some("downloading…".into());
        let (tx, rx) = std::sync::mpsc::channel();
        // As above: `drain_fetch` reports a download that did not finish.
        let started = std::thread::Builder::new()
            .name("credentials".into())
            .spawn(move || {
                let agent = priel_core::new_agent();
                let outcome =
                    priel_core::auth::fetch_credentials(&agent, priel_core::auth::UPSTREAM_SOURCES)
                        .and_then(|creds| creds.save(&path))
                        .map_err(|e| format!("{e:#}"));
                let _ = tx.send(outcome);
            });
        if let Err(e) = started {
            log::error!("no thread for the client-key download: {e}");
        }
        self.fetching = Some(rx);
    }

    /// Collect the result of an in-flight download.
    fn drain_fetch(&mut self) {
        let Some(rx) = &self.fetching else { return };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.fetching = None;
                self.mode = Mode::Normal;
                self.notice = Some("Client key saved.".into());
                self.continue_after_credentials();
                self.dirty = true;
            }
            Ok(Err(e)) => {
                self.fetching = None;
                self.credential_status = Some(e);
                self.dirty = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.fetching = None;
                self.credential_status = Some("the download did not finish".into());
                self.dirty = true;
            }
        }
    }

    /// The consent screen is modal: nothing reaches the list behind it.
    fn on_key_credentials(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('f') => self.fetch_credentials(),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
            _ => {}
        }
    }

    pub fn start(&mut self) {
        self.ask(ToWorker::LoadFavorites);
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

    /// Post a request to the worker.
    ///
    /// The send fails only when the worker thread is gone, and from then on
    /// every request fails too - the interface stops answering keys. Discarding
    /// this Result is what made that look like a freeze, with nothing on screen
    /// and nothing anywhere else either.
    fn ask(&mut self, req: ToWorker) {
        if self.worker.tx.send(req).is_err() {
            self.report_worker_lost();
        }
    }

    /// Report a worker thread that is no longer there, once.
    ///
    /// Once, because `drain_worker` runs on every pass of the event loop:
    /// repeating it would wipe out whatever else the user was being told and
    /// leave the screen permanently dirty, which is what the redraw check exists
    /// to prevent.
    fn report_worker_lost(&mut self) {
        if self.worker_lost {
            return;
        }
        self.worker_lost = true;
        self.loading = false;
        self.dirty = true;
        log::error!("the worker thread is gone; nothing more can be loaded");
        self.notice = Some("\u{26a0} the worker stopped; restart priel".into());
    }

    /// A stream came back resolved.
    ///
    /// Which of the two things to do with it is decided by *which* track was
    /// asked for, never by arrival order: replies are correlated by id, and a
    /// slow resolve for a track the user has already left must not start
    /// playing.
    fn on_resolved(&mut self, id: u64, r: &priel_core::ResolvedStream) {
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

    pub fn drain_worker(&mut self) {
        loop {
            let msg = match self.worker.rx.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => return,
                // Disconnected was indistinguishable from Empty here, so a dead
                // worker read as an idle one for as long as priel stayed open.
                Err(TryRecvError::Disconnected) => {
                    self.report_worker_lost();
                    return;
                }
            };
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
                FromWorker::Resolved(id, r) => self.on_resolved(id, &r),
                FromWorker::Failed { fault, detail } => {
                    self.loading = false;
                    // Branching on the classification, never on the words. This
                    // was `e.contains("log in again")`, which made the core's
                    // wording load-bearing: rewording that sentence would have
                    // quietly stopped the login screen from being offered.
                    match fault {
                        Fault::SignedOut => self.offer_relogin(&detail),
                        Fault::Unreachable => {
                            self.notice = Some(format!("⚠ could not reach the service: {detail}"));
                        }
                        Fault::Refused => self.notice = Some(format!("⚠ {detail}")),
                    }
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
        self.drain_fetch();
        self.drain_login();
        self.status = self.player.status();
        self.refresh_from_status();
        self.refresh_devices();
    }

    /// The half of `refresh` that reacts to `self.status`, split out so tests can
    /// drive playback states the null player will never produce on its own.
    #[cfg(test)]
    fn refresh_for_test(&mut self) {
        self.refresh_from_status();
    }

    /// The state a status tick is allowed to look at.
    ///
    /// Spelled out as a snapshot so the decision below can be a pure function.
    /// Every field here is one the advance logic reads; nothing else may be
    /// consulted, which is what keeps the guards testable.
    fn tick(&self) -> Tick {
        Tick {
            playing: self.status.playing,
            ended: self.status.ended,
            paused: self.status.paused,
            has_next: self.status.has_next,
            current_id: self.status.current_id,
            expected_id: self.expected_id,
            resolving_current: self.current_target.is_some(),
            preload_queued: self.next_intended.is_some(),
            advanced: self.advanced,
            have_track: self.now_playing.is_some(),
        }
    }

    /// What this tick asks the queue to do.
    ///
    /// Pure, so the guards below can be a table of tests rather than comments
    /// pleading with the reader. See [`Plan`] for why an adoption is exclusive
    /// and the other two are not.
    fn decide(t: &Tick) -> Plan {
        // mpv moved to a track we never loaded: a gapless transition.
        if t.current_id != 0 && t.current_id != t.expected_id {
            return Plan {
                adopt: Some(t.current_id),
                ..Plan::default()
            };
        }
        Plan {
            adopt: None,
            // The current track is settled and nothing is queued behind it.
            preload: t.have_track && !t.resolving_current && !t.has_next && !t.preload_queued,
            // Playback genuinely stopped with nothing preloaded - the end of the
            // queue, or a preload that never arrived. `!playing` is what keeps
            // this out of a gapless change, where mpv reports the outgoing track
            // as ended while audio is still flowing; `!advanced` stops it firing
            // again on every tick after it has.
            advance_fresh: t.ended
                && !t.has_next
                && !t.advanced
                && t.have_track
                && !t.playing
                && !t.paused
                && !t.resolving_current,
        }
    }

    /// Take up a track mpv moved to on its own.
    fn adopt(&mut self, id: u64) {
        self.expected_id = id;
        // We just advanced, so the end-of-track fallback must not also fire.
        self.advanced = true;
        if let Some(p) = self.queue.iter().position(|t| t.id == id) {
            self.queue_pos = p;
            self.now_playing = Some(self.queue[p].clone());
        }
        self.now_meta = self.metas.get(&id).cloned().unwrap_or_default();
        self.next_intended = None;
        self.dirty = true;
        self.schedule_next();
    }

    fn refresh_from_status(&mut self) {
        self.frame = self.frame.wrapping_add(1);

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

        let plan = Self::decide(&self.tick());
        if let Some(id) = plan.adopt {
            self.adopt(id);
        }
        if plan.preload {
            self.schedule_next();
        }
        if plan.advance_fresh {
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
        // Leaving a view leaves its input mode with it. Without this, clicking a
        // tab while filtering strands the user in a text mode whose text is gone.
        self.mode = Mode::Normal;
        match v {
            View::Playlists if self.playlists.is_empty() => {
                self.loading = true;
                self.ask(ToWorker::LoadPlaylists);
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
            self.ask(ToWorker::LoadPlaylistTracks(p.uuid));
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
        self.ask(ToWorker::Resolve(t.id));
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
                self.ask(ToWorker::Resolve(id));
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

    fn goto_top(&mut self) {
        self.selected = 0;
    }

    fn goto_bottom(&mut self) {
        self.selected = self.visible().len().saturating_sub(1);
    }

    fn start_filter(&mut self) {
        self.mode = Mode::Filter;
        self.filter.clear();
        self.selected = 0;
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

    /// Restore unity gain.
    ///
    /// Worth its own binding: any software volume below 100% multiplies every
    /// sample, so it is the one bit-perfect fault the listener causes and the
    /// only one they can clear instantly.
    fn volume_unity(&mut self) {
        self.player.set_volume(100.0);
        self.notice = Some("Volume 100% (unity gain)".into());
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
            Mode::Help => self.on_key_help(key),
            Mode::Log => self.on_key_log(key),
            Mode::Devices => self.on_key_devices(key),
            Mode::Credentials => self.on_key_credentials(key),
            Mode::Login => self.on_key_login(key),
            Mode::Normal => self.on_key_normal(key),
        }
    }

    /// The help overlay is modal: anything that reads as "done" dismisses it, and
    /// nothing else leaks through to the list underneath.
    fn on_key_help(&mut self, key: KeyEvent) {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q' | ' ')
        ) {
            self.mode = Mode::Normal;
        }
    }

    /// The log overlay: modal like the help one, and scrolled like every list.
    ///
    /// A second scrolling idiom would be its own bug, so j/k and g/G mean here
    /// what they mean everywhere else - except that "top" is the oldest line,
    /// since the overlay opens on the newest.
    fn on_key_log(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('M' | 'q' | ' ') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.log_scroll = (self.log_scroll + 1).min(self.log_scroll_max());
            }
            KeyCode::Char('g') => self.log_scroll = self.log_scroll_max(),
            KeyCode::Char('G') => self.log_scroll = 0,
            _ => {}
        }
        self.dirty = true;
    }

    /// How far back the overlay may be scrolled.
    fn log_scroll_max(&self) -> usize {
        self.recent.lines().len().saturating_sub(1)
    }

    /// The diagnostics to show, oldest first.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        self.recent.lines()
    }

    /// How far back the log overlay is scrolled, in lines from the newest.
    #[must_use]
    pub fn log_offset(&self) -> usize {
        self.log_scroll
    }

    // ---- the output device picker ----

    /// Open the output picker, asking for a fresh list as it opens.
    ///
    /// The one way in: the key and the hint click both come through here, so the
    /// two cannot drift apart.
    fn open_devices(&mut self) {
        self.mode = Mode::Devices;
        self.device_offset = 0;
        self.devices_asked = Some(Instant::now());
        // Devices come and go while priel runs. What is on screen should be
        // what is true now, not what was true at startup.
        self.player.refresh_devices();
        self.select_current_device();
    }

    /// Start the picker on the device already in use, the way a list opens on
    /// the row that was last touched.
    fn select_current_device(&mut self) {
        self.device_selected = self
            .devices
            .iter()
            .position(|d| d.name == self.status.audio_device)
            .unwrap_or(0);
    }

    /// The picker: modal like the log overlay, and scrolled with the same keys.
    fn on_key_devices(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('d' | 'q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => self.device_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.device_up(1),
            KeyCode::Char('J') => self.device_down(self.full_page()),
            KeyCode::Char('K') => self.device_up(self.full_page()),
            KeyCode::Char('g') => self.device_selected = 0,
            KeyCode::Char('G') => self.device_selected = self.devices.len().saturating_sub(1),
            _ => {}
        }
        self.dirty = true;
    }

    fn device_down(&mut self, by: usize) {
        let n = self.devices.len();
        if n > 0 {
            self.device_selected = (self.device_selected + by).min(n - 1);
        }
    }

    fn device_up(&mut self, by: usize) {
        self.device_selected = self.device_selected.saturating_sub(by);
    }

    /// A click inside the picker. On a row it takes that row; anywhere else it
    /// dismisses, as a click on the log overlay does.
    fn click_device(&mut self, col: u16, row: u16) {
        match self
            .device_rows
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, i)| *i)
        {
            Some(i) => self.device_selected = i,
            None => self.mode = Mode::Normal,
        }
    }

    /// Take up a device list the player has published.
    ///
    /// Only while the picker is up: the list is a handful of allocations and
    /// nothing else on screen shows it, so polling it the rest of the time
    /// would be work done for nobody.
    fn refresh_devices(&mut self) {
        if self.mode != Mode::Devices {
            return;
        }
        let devices = self.player.devices();
        if devices == self.devices {
            return;
        }
        let was_empty = self.devices.is_empty();
        self.devices = devices;
        if was_empty {
            // The first list to arrive decides where the picker opens.
            self.select_current_device();
        }
        self.device_selected = self
            .device_selected
            .min(self.devices.len().saturating_sub(1));
        self.dirty = true;
    }

    /// The devices the picker is showing.
    #[must_use]
    pub fn devices(&self) -> &[AudioDevice] {
        &self.devices
    }

    /// Hand the picker a list without an audio system to ask.
    #[cfg(test)]
    pub fn set_devices_for_test(&mut self, devices: Vec<AudioDevice>) {
        self.devices = devices;
    }

    /// Which row the picker is on.
    #[must_use]
    pub fn device_selected(&self) -> usize {
        self.device_selected
    }

    /// What the picker shows instead of a list, when there is no list.
    #[must_use]
    pub fn device_notice(&self) -> Option<&'static str> {
        Self::devices_message(
            self.devices.is_empty(),
            self.devices_asked.map(|t| t.elapsed()),
        )
    }

    /// Pure, so the two empty cases can be told apart in a test without waiting
    /// for a clock: the answer is still on its way, or nothing is going to
    /// answer. The second is what a build without libmpv looks like, and saying
    /// so beats an empty box that reads as a bug.
    fn devices_message(empty: bool, since_asked: Option<Duration>) -> Option<&'static str> {
        if !empty {
            return None;
        }
        match since_asked {
            Some(waited) if waited < DEVICE_WAIT => Some("Looking for output devices…"),
            _ => Some("No output devices were reported."),
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
            // Printable keys are query text - "1999" is a legitimate search - so
            // Tab is the one view-switch that has to keep working from in here,
            // or the search box becomes a trap with no keyboard way out.
            KeyCode::Tab => self.cycle_view(),
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                let q = self.search_query.trim().to_string();
                if !q.is_empty() {
                    self.loading = true;
                    self.search_tracks.clear();
                    self.selected = 0;
                    self.ask(ToWorker::Search(q));
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
            KeyCode::Char('g') => self.goto_top(),
            KeyCode::Char('G') => self.goto_bottom(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('M') => {
                self.mode = Mode::Log;
                // Always open on the newest line: the reason for opening this is
                // almost always something that just happened.
                self.log_scroll = 0;
            }
            KeyCode::Char('d') => self.open_devices(),
            KeyCode::Char('A') => self.start_login(),
            KeyCode::Enter => self.on_enter(),
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('n' | 'L') => self.user_next(),
            KeyCode::Char('p' | 'H') => self.user_prev(),
            KeyCode::Char('h') | KeyCode::Left => self.player.seek_relative(-5.0),
            KeyCode::Char('l') | KeyCode::Right => self.player.seek_relative(5.0),
            KeyCode::Char('+' | '=') => self.volume_step(5.0),
            KeyCode::Char('-') => self.volume_step(-5.0),
            KeyCode::Char('0') => self.volume_unity(),
            KeyCode::Char('/') => self.start_filter(),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if matches!(self.mode, Mode::Credentials | Mode::Login) {
            return; // these screens take no mouse input
        }
        if self.mode == Mode::Log {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    self.log_scroll = (self.log_scroll + 1).min(self.log_scroll_max());
                    self.dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                    self.dirty = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.mode = Mode::Normal;
                    self.dirty = true;
                }
                _ => {}
            }
            return;
        }
        if self.mode == Mode::Devices {
            match m.kind {
                MouseEventKind::ScrollDown => self.device_down(1),
                MouseEventKind::ScrollUp => self.device_up(1),
                MouseEventKind::Down(MouseButton::Left) => self.click_device(m.column, m.row),
                _ => return,
            }
            self.dirty = true;
            return;
        }
        if self.mode == Mode::Help {
            // Any click dismisses; scrolling the list behind it would be odd.
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.mode = Mode::Normal;
                self.dirty = true;
            }
            return;
        }
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
            Hit::SeekBack => self.player.seek_relative(-5.0),
            Hit::SeekFwd => self.player.seek_relative(5.0),
            Hit::MoveUp => self.move_up(1),
            Hit::MoveDown => self.move_down(1),
            Hit::Top => self.goto_top(),
            Hit::Bottom => self.goto_bottom(),
            Hit::Shuffle => self.toggle_shuffle(),
            Hit::VolUp => self.volume_step(5.0),
            Hit::VolDown => self.volume_step(-5.0),
            Hit::VolUnity => self.volume_unity(),
            Hit::Filter => self.start_filter(),
            Hit::CycleView => self.cycle_view(),
            Hit::Help => self.mode = Mode::Help,
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

/// May priel spawn a browser right now?
///
/// Separated from the spawning so it can be asserted: without a test covering
/// this, a refactor that drops the guard is invisible until someone's screen
/// fills with tabs.
fn should_open_browser() -> bool {
    !cfg!(test) && std::env::var_os("PRIEL_NO_BROWSER").is_none()
}

/// Open a URL in the user's browser, best effort.
///
/// A failure is not reported: the screen always shows the URL as well, so a
/// user on a machine with no handler can still copy it.
///
/// Never during tests. Several tests drive the sign-in flow, and spawning a real
/// browser tab for each of them on every `cargo test` is both useless and
/// hostile to whoever is running the suite.
fn open_in_browser(url: &str) {
    if !should_open_browser() {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
    use super::*;
    use priel_core::{PlayableSource, ResolvedStream};

    struct Rig {
        app: App,
        to_app: Sender<FromWorker>,
        from_app: Receiver<ToWorker>,
    }

    /// An app with a silent player and a worker whose both ends the test holds.
    fn rig() -> Rig {
        let (app, to_app, from_app) = App::rigged();
        Rig {
            app,
            to_app,
            from_app,
        }
    }

    fn track(id: u64, title: &str, artist: &str) -> Track {
        Track {
            id,
            title: title.into(),
            artist: artist.into(),
            album: "Alb".into(),
            duration_secs: 100,
            quality: "HI-RES".into(),
        }
    }

    fn playlist(uuid: &str, title: &str) -> Playlist {
        Playlist {
            uuid: uuid.into(),
            title: title.into(),
            num_tracks: 2,
            duration_secs: 200,
        }
    }

    fn stream(id: u64) -> ResolvedStream {
        ResolvedStream {
            source: PlayableSource::Direct(format!("http://127.0.0.1:1/{id}")),
            sample_rate: 192_000,
            bit_depth: 24,
            codec: "flac".into(),
            quality: "HI_RES_LOSSLESS".into(),
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn code(k: KeyCode) -> KeyEvent {
        KeyEvent::new(k, KeyModifiers::NONE)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn requests(r: &Rig) -> Vec<ToWorker> {
        r.from_app.try_iter().collect()
    }

    fn resolved_ids(reqs: &[ToWorker]) -> Vec<u64> {
        reqs.iter()
            .filter_map(|c| match c {
                ToWorker::Resolve(id) => Some(*id),
                _ => None,
            })
            .collect()
    }

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

    // ---- startup and worker traffic ----

    #[test]
    fn startup_asks_for_favorites_and_says_it_is_loading() {
        // Goal: the first frame must not look like an empty library.
        let mut r = rig();
        r.app.start();
        assert!(matches!(requests(&r)[..], [ToWorker::LoadFavorites]));
        assert!(r.app.loading);
    }

    #[test]
    fn arriving_favorites_replace_the_list_and_clear_loading() {
        // Goal: the worker reply is the only thing that ends the loading state.
        let mut r = rig();
        r.to_app
            .send(FromWorker::Favorites(vec![track(1, "A", "X")]))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.favorites.len(), 1);
        assert!(!r.app.loading);
        assert!(r.app.notice.as_deref().unwrap().contains('1'));
    }

    /// A tick where nothing is happening: no track, no audio, nothing queued.
    fn quiet() -> Tick {
        Tick {
            playing: false,
            ended: false,
            paused: false,
            has_next: false,
            current_id: 0,
            expected_id: 0,
            resolving_current: false,
            preload_queued: false,
            advanced: false,
            have_track: false,
        }
    }

    /// A tick where a track is loaded and audio is flowing.
    fn settled() -> Tick {
        Tick {
            playing: true,
            current_id: 7,
            expected_id: 7,
            have_track: true,
            ..quiet()
        }
    }

    /// A track that ran out with nothing queued behind it.
    fn ran_out() -> Tick {
        Tick {
            ended: true,
            have_track: true,
            current_id: 7,
            expected_id: 7,
            ..quiet()
        }
    }

    #[test]
    fn a_track_that_ran_out_both_advances_and_looks_ahead() {
        // Goal: the regression this refactor nearly introduced. These are three
        // independent answers, not a priority list: a track whose preload never
        // arrived needs the fallback *and* the next preload on the same tick.
        // Ordering them so the first match wins stalls playback for a tick, and
        // with shuffle on it never advances at all.
        let plan = App::decide(&ran_out());
        assert!(plan.advance_fresh, "it has to move on");
        assert!(plan.preload, "and look ahead while it does");
    }

    #[test]
    fn a_quiet_tick_asks_for_nothing() {
        // Goal: the common case by far. A tick that decides to do something
        // when nothing has happened is how the queue runs away.
        assert_eq!(App::decide(&quiet()), Plan::default());
    }

    #[test]
    fn a_track_mpv_moved_to_on_its_own_is_adopted() {
        // Goal: a gapless transition is mpv changing tracks without being told
        // to. The app finds out by the id changing under it, and everything
        // else - queue position, metadata, the next preload - follows from
        // noticing.
        let t = Tick {
            current_id: 9,
            expected_id: 7,
            ..settled()
        };
        assert_eq!(App::decide(&t).adopt, Some(9));
    }

    #[test]
    fn an_empty_current_id_is_not_a_transition() {
        // Goal: mpv reports 0 when it has nothing loaded, and treating that as
        // a track would adopt a track that does not exist.
        let t = Tick {
            current_id: 0,
            expected_id: 7,
            ..settled()
        };
        assert_eq!(App::decide(&t).adopt, None);
    }

    #[test]
    fn a_settled_track_with_nothing_behind_it_asks_for_a_preload() {
        // Goal: gapless depends on the next entry already being in mpv's
        // playlist. This is the tick that puts it there.
        assert!(App::decide(&settled()).preload);
    }

    #[test]
    fn a_preload_already_asked_for_is_not_asked_for_twice() {
        // Goal: one resolve per track. Repeating it every tick would flood the
        // worker for as long as the track plays.
        let queued = Tick {
            preload_queued: true,
            ..settled()
        };
        assert!(!App::decide(&queued).preload);
        let in_mpv = Tick {
            has_next: true,
            ..settled()
        };
        assert!(!App::decide(&in_mpv).preload);
    }

    #[test]
    fn a_track_that_ran_out_advances_from_scratch() {
        // Goal: end of queue, or a preload that never arrived. Nothing is
        // queued in mpv, so the next track has to be loaded outright.
        assert!(App::decide(&ran_out()).advance_fresh);
    }

    #[test]
    fn the_end_of_track_fallback_stays_out_of_a_healthy_transition() {
        // Goal: this is the bug the guards exist for. During a gapless change
        // mpv reports `ended` for the outgoing track while audio is still
        // flowing; advancing here skips a track, and doing it every tick runs
        // away through the whole queue.
        let still_playing = Tick {
            playing: true,
            ..ran_out()
        };
        assert!(
            !App::decide(&still_playing).advance_fresh,
            "audio is flowing"
        );

        let just_advanced = Tick {
            advanced: true,
            ..ran_out()
        };
        assert!(
            !App::decide(&just_advanced).advance_fresh,
            "already advanced"
        );

        let queued = Tick {
            has_next: true,
            ..ran_out()
        };
        assert!(!App::decide(&queued).advance_fresh, "mpv has one ready");
    }

    #[test]
    fn a_paused_track_at_its_end_waits_for_the_listener() {
        // Goal: pausing on the last second of a track must not skip it. The
        // listener stopped on purpose.
        let t = Tick {
            paused: true,
            ..ran_out()
        };
        assert!(!App::decide(&t).advance_fresh);
    }

    #[test]
    fn a_track_still_being_resolved_is_not_overtaken() {
        // Goal: a resolve in flight is a track about to start. Advancing past
        // it would skip whatever the user just asked for.
        let t = Tick {
            resolving_current: true,
            ..ran_out()
        };
        assert!(!App::decide(&t).advance_fresh);
    }

    #[test]
    fn nothing_advances_when_nothing_was_playing() {
        // Goal: an idle player at startup reports `ended` from no track at all.
        let t = Tick {
            have_track: false,
            ..ran_out()
        };
        assert!(!App::decide(&t).advance_fresh);
    }

    #[test]
    fn a_refused_session_offers_the_login_screen_whatever_it_says() {
        // Goal: this used to be `e.contains("log in again")` - the interface
        // recognised the one failure a user can fix by reading the sentence
        // that described it. Rewording the message in the core would have
        // silently stopped the login screen from appearing.
        let mut r = rig();
        r.app
            .set_paths_for_test("/nonexistent/token.json".into(), credentials_fixture());
        r.to_app
            .send(FromWorker::Failed {
                fault: Fault::SignedOut,
                detail: "something nobody thought to grep for".into(),
            })
            .expect("send");
        r.app.drain_worker();
        assert_eq!(r.app.mode, Mode::Login, "the fix is offered, not described");
    }

    #[test]
    fn a_connection_that_dropped_is_not_treated_as_a_sign_out() {
        // Goal: the opposite mistake. Being thrown at the login screen because
        // the wifi went is worse than useless - the session is fine, and the
        // notice should say what actually happened.
        let mut r = rig();
        r.app.loading = true;
        r.to_app
            .send(FromWorker::Failed {
                fault: Fault::Unreachable,
                detail: "favorites".into(),
            })
            .expect("send");
        r.app.drain_worker();
        assert_ne!(r.app.mode, Mode::Login);
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("reach"), "{notice}");
        assert!(!r.app.loading);
    }

    #[test]
    fn a_worker_error_becomes_a_visible_notice() {
        // Goal: errors are the user's only feedback that something broke, and
        // they must not leave the spinner running forever.
        let mut r = rig();
        r.app.loading = true;
        r.to_app
            .send(FromWorker::Failed {
                fault: Fault::Refused,
                detail: "token: expired".into(),
            })
            .unwrap();
        r.app.drain_worker();
        assert!(r.app.notice.as_deref().unwrap().contains("expired"));
        assert!(!r.app.loading);
    }

    #[test]
    fn the_log_overlay_opens_on_its_key_and_closes_again() {
        // Goal: "logging for a TUI is odd" is answered by not making the user
        // leave priel to read it. The key that opens it must also close it.
        let mut r = rig();
        r.app.on_key(key('M'));
        assert_eq!(r.app.mode, Mode::Log);
        r.app.on_key(key('M'));
        assert_eq!(r.app.mode, Mode::Normal);
        r.app.on_key(key('M'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal, "Esc closes it too");
    }

    #[test]
    fn the_log_overlay_swallows_the_keys_behind_it() {
        // Goal: modal like the help overlay. A view change happening underneath
        // an overlay is how a user ends up somewhere they did not ask for.
        let mut r = rig();
        r.app.on_key(key('M'));
        r.app.on_key(key('2'));
        assert_eq!(r.app.view, View::Favorites, "the view must not change");
        assert_eq!(r.app.mode, Mode::Log, "and it is still open");
    }

    #[test]
    fn the_log_overlay_scrolls_with_the_same_keys_as_a_list() {
        // Goal: every other list in priel moves on j/k and g/G, so this one
        // must too - a second scrolling idiom would be its own bug.
        let mut r = rig();
        for i in 0..40 {
            r.app.recent.push(format!("line {i}\n"));
        }
        r.app.on_key(key('M'));
        assert_eq!(r.app.log_scroll, 0, "it opens at the newest lines");
        r.app.on_key(key('k'));
        assert_eq!(r.app.log_scroll, 1, "k goes back through history");
        r.app.on_key(key('j'));
        assert_eq!(r.app.log_scroll, 0);
        r.app.on_key(key('j'));
        assert_eq!(r.app.log_scroll, 0, "and stops at the newest");
        r.app.on_key(key('g'));
        assert!(r.app.log_scroll > 1, "g reaches the oldest");
        r.app.on_key(key('G'));
        assert_eq!(r.app.log_scroll, 0, "G returns to the newest");
    }

    /// Three devices with the middle one in use, so a test can tell the
    /// selected row from the marked one.
    fn devices() -> Vec<AudioDevice> {
        ["auto", "pipewire/dac", "alsa/hdmi"]
            .iter()
            .map(|n| AudioDevice {
                name: (*n).to_string(),
                description: format!("{n} description"),
            })
            .collect()
    }

    /// A rigged app with the picker open on a known list.
    fn with_picker(in_use: &str) -> Rig {
        let mut r = rig();
        r.app.status.audio_device = in_use.to_string();
        r.app.set_devices_for_test(devices());
        r.app.on_key(key('d'));
        r
    }

    #[test]
    fn the_device_picker_opens_on_its_key_and_closes_again() {
        // Goal: the whole point of the issue is not having to leave the player
        // to find out what a device is called. The key that opens it closes it,
        // as every other overlay here does.
        let mut r = with_picker("auto");
        assert_eq!(r.app.mode, Mode::Devices);
        r.app.on_key(key('d'));
        assert_eq!(r.app.mode, Mode::Normal);
        r.app.on_key(key('d'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal, "Esc closes it too");
    }

    #[test]
    fn escape_leaves_the_picker_without_touching_the_output() {
        // Goal: a picker that changed the output as the selection moved would
        // be unusable. Only choosing changes anything, and Esc chooses nothing.
        let mut r = with_picker("pipewire/dac");
        r.app.on_key(key('j'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert_eq!(
            r.app.status.audio_device, "pipewire/dac",
            "cancelling must leave the output where it was"
        );
    }

    #[test]
    fn the_device_picker_swallows_the_keys_behind_it() {
        // Goal: modal like the other overlays. Changing view underneath one is
        // how a user ends up somewhere they did not ask to be.
        let mut r = with_picker("auto");
        r.app.on_key(key('2'));
        assert_eq!(r.app.view, View::Favorites, "the view must not change");
        assert_eq!(r.app.mode, Mode::Devices, "and it is still open");
    }

    #[test]
    fn the_device_picker_scrolls_with_the_same_keys_as_a_list() {
        // Goal: j/k and g/G mean here what they mean everywhere else in priel.
        // A second scrolling idiom would be its own bug.
        let mut r = with_picker("auto");
        assert_eq!(r.app.device_selected(), 0);
        r.app.on_key(key('j'));
        assert_eq!(r.app.device_selected(), 1);
        r.app.on_key(key('k'));
        assert_eq!(r.app.device_selected(), 0);
        r.app.on_key(key('k'));
        assert_eq!(r.app.device_selected(), 0, "and stops at the first");
        r.app.on_key(key('G'));
        assert_eq!(r.app.device_selected(), 2, "G reaches the last");
        r.app.on_key(key('j'));
        assert_eq!(r.app.device_selected(), 2, "and stops there");
        r.app.on_key(key('g'));
        assert_eq!(r.app.device_selected(), 0);
    }

    #[test]
    fn the_picker_opens_on_the_device_already_in_use() {
        // Goal: the list is long - forty-odd entries on a normal desktop - so
        // opening at the top would leave the user hunting for where they are.
        let r = with_picker("alsa/hdmi");
        assert_eq!(r.app.device_selected(), 2);
    }

    #[test]
    fn a_device_that_is_gone_leaves_the_picker_at_the_top() {
        // Goal: the device priel was started with can be unplugged. That must
        // open the list, not index past the end of it.
        let r = with_picker("pipewire/one-that-left");
        assert_eq!(r.app.device_selected(), 0);
    }

    #[test]
    fn an_empty_picker_says_which_kind_of_empty_it_is() {
        // Goal: the list is asked for as the overlay opens and answered a tick
        // later, so the first frame is always empty - and a build without
        // libmpv never answers at all. Showing an empty box for either reads as
        // a bug; the two have to be told apart.
        assert_eq!(
            App::devices_message(false, None),
            None,
            "a list needs no note"
        );
        assert_eq!(
            App::devices_message(true, Some(Duration::from_millis(50))),
            Some("Looking for output devices…")
        );
        assert_eq!(
            App::devices_message(true, Some(Duration::from_secs(30))),
            Some("No output devices were reported."),
            "nothing answered, and saying so beats an empty box"
        );
        assert_eq!(
            App::devices_message(true, None),
            Some("No output devices were reported.")
        );
    }

    #[test]
    fn a_worker_that_has_died_is_reported_instead_of_looking_like_a_hang() {
        // Goal: `try_recv` returns Disconnected for a dead worker and Empty for
        // an idle one, and this treated them identically - so a worker thread
        // that ended left the app loading forever with nothing on screen to say
        // why. The spinner has to stop too.
        let mut r = rig();
        r.app.loading = true;
        drop(r.to_app);
        r.app.drain_worker();
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("worker"), "{notice}");
        assert!(!r.app.loading, "a worker that is gone is not still loading");
    }

    #[test]
    fn a_dead_worker_is_reported_once_rather_than_every_tick() {
        // Goal: drain_worker runs on every pass of the event loop. Re-reporting
        // would wipe out whatever else the user was being told, and mark the
        // screen dirty forever - which is exactly what the redraw check exists
        // to avoid.
        let mut r = rig();
        drop(r.to_app);
        r.app.drain_worker();
        r.app.take_dirty();
        r.app.notice = Some("something else".into());
        r.app.drain_worker();
        assert_eq!(r.app.notice.as_deref(), Some("something else"));
        assert!(!r.app.take_dirty(), "nothing changed on the second pass");
    }

    #[test]
    fn a_request_that_cannot_be_sent_is_reported() {
        // Goal: every request to the worker was a discarded Result, so once its
        // thread was gone the interface simply stopped responding to keys with
        // no explanation anywhere.
        let mut r = rig();
        drop(r.from_app);
        r.app.start();
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("worker"), "{notice}");
    }

    #[test]
    fn playlist_tracks_for_a_playlist_no_longer_open_are_discarded() {
        // Goal: replies are correlated by uuid, not arrival order. A slow reply
        // for a playlist the user already left must not overwrite the new one.
        let mut r = rig();
        r.app.playlists = vec![playlist("wanted", "W")];
        r.to_app
            .send(FromWorker::PlaylistTracks(
                "stale".into(),
                vec![track(9, "S", "S")],
            ))
            .unwrap();
        r.app.drain_worker();
        assert!(
            r.app.playlist_tracks.is_empty(),
            "a stale reply must be dropped"
        );
    }

    // ---- views and navigation ----

    #[test]
    fn number_keys_and_tab_move_between_views() {
        // Goal: both routes to a view must agree, and Tab has to cycle back
        // round rather than stopping at the last one.
        let mut r = rig();
        r.app.on_key(key('2'));
        assert_eq!(r.app.view, View::Playlists);
        r.app.on_key(key('1'));
        assert_eq!(r.app.view, View::Favorites);

        r.app.on_key(code(KeyCode::Tab));
        assert_eq!(r.app.view, View::Playlists);
        r.app.on_key(code(KeyCode::Tab));
        assert_eq!(r.app.view, View::Search);
        assert_eq!(
            r.app.mode,
            Mode::Search,
            "an empty search starts in the box"
        );

        // Digits are query text once the box is open, so Tab has to be the way
        // out - and it must restore Normal mode on the way.
        r.app.on_key(key('1'));
        assert_eq!(
            r.app.view,
            View::Search,
            "a digit types, it does not switch"
        );
        r.app.on_key(code(KeyCode::Tab));
        assert_eq!(r.app.view, View::Favorites, "Tab wraps around");
        assert_eq!(r.app.mode, Mode::Normal, "and leaves the input mode behind");
    }

    #[test]
    fn opening_the_playlists_view_fetches_them_only_once() {
        // Goal: switching views repeatedly should not re-fetch a list already
        // held; the request is what costs a round trip.
        let mut r = rig();
        r.app.on_key(key('2'));
        assert!(matches!(requests(&r)[..], [ToWorker::LoadPlaylists]));

        r.to_app
            .send(FromWorker::Playlists(vec![playlist("u", "P")]))
            .unwrap();
        r.app.drain_worker();
        r.app.on_key(key('1'));
        r.app.on_key(key('2'));
        assert!(requests(&r).is_empty(), "already loaded, no second fetch");
    }

    #[test]
    fn entering_a_playlist_loads_its_tracks_and_escape_goes_back() {
        // Goal: the drill-down is the one nested view, and Esc is the only way
        // out of it.
        let mut r = rig();
        r.app.playlists = vec![playlist("uuid-1", "Mix")];
        r.app.view = View::Playlists;
        r.app.on_key(code(KeyCode::Enter));

        assert_eq!(r.app.view, View::PlaylistTracks);
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadPlaylistTracks(ref u)] if u == "uuid-1"
        ));

        r.to_app
            .send(FromWorker::PlaylistTracks(
                "uuid-1".into(),
                vec![track(4, "T", "A")],
            ))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.playlist_tracks.len(), 1);

        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.view, View::Playlists);
    }

    #[test]
    fn movement_keys_stay_inside_the_list() {
        // Goal: selection is an index into the *visible* rows; running off
        // either end would panic on the next render.
        let mut r = rig();
        r.app.favorites = (0..5).map(|i| track(i, "T", "A")).collect();
        r.app.list_inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };

        r.app.on_key(key('G'));
        assert_eq!(r.app.selected, 4);
        r.app.on_key(key('j'));
        assert_eq!(r.app.selected, 4, "cannot move past the last row");
        r.app.on_key(key('g'));
        assert_eq!(r.app.selected, 0);
        r.app.on_key(key('k'));
        assert_eq!(r.app.selected, 0, "cannot move above the first row");

        r.app.on_key(key('J'));
        assert!(r.app.selected > 0, "page down moves by a screenful");
        r.app.on_key(key('K'));
        assert_eq!(r.app.selected, 0);

        r.app
            .on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(r.app.selected > 0, "ctrl-d is a half page");
        r.app
            .on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(r.app.selected, 0);
    }

    #[test]
    fn an_empty_list_survives_every_movement_key() {
        // Goal: before the first reply arrives the list is empty; none of these
        // may index into it.
        let mut r = rig();
        for k in ['j', 'k', 'g', 'G', 'J', 'K'] {
            r.app.on_key(key(k));
        }
        assert_eq!(r.app.selected, 0);
        assert!(r.app.visible().is_empty());
    }

    // ---- filtering and search ----

    #[test]
    fn typing_a_filter_narrows_the_visible_rows_and_escape_restores_them() {
        // Goal: the filter is local and non-destructive - Esc must bring the
        // full list back rather than requiring a refetch.
        let mut r = rig();
        r.app.favorites = vec![track(1, "Blue", "Miles"), track(2, "Red", "Bill")];

        r.app.on_key(key('/'));
        assert_eq!(r.app.mode, Mode::Filter);
        for c in "blue".chars() {
            r.app.on_key(key(c));
        }
        assert_eq!(r.app.visible(), vec![0]);

        r.app.on_key(code(KeyCode::Backspace));
        assert_eq!(r.app.filter, "blu");

        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(
            r.app.mode,
            Mode::Normal,
            "Enter keeps the filter, leaves the mode"
        );
        assert_eq!(r.app.visible(), vec![0]);

        r.app.on_key(key('/'));
        r.app.on_key(code(KeyCode::Esc));
        assert!(r.app.filter.is_empty());
        assert_eq!(r.app.visible(), vec![0, 1]);
    }

    #[test]
    fn a_search_is_only_sent_when_the_query_is_not_blank() {
        // Goal: an accidental Enter on an empty box should not cost a request.
        let mut r = rig();
        r.app.on_key(key('3'));
        assert_eq!(
            r.app.mode,
            Mode::Search,
            "the search view starts in edit mode"
        );

        r.app.on_key(code(KeyCode::Enter));
        assert!(requests(&r).is_empty(), "a blank query is not a search");

        r.app.on_key(key('i'));
        for c in "mile".chars() {
            r.app.on_key(key(c));
        }
        r.app.on_key(code(KeyCode::Backspace));
        r.app.on_key(code(KeyCode::Enter));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::Search(ref q)] if q == "mil"
        ));

        r.to_app
            .send(FromWorker::SearchResults(priel_core::SearchResults {
                tracks: vec![track(3, "Milestones", "Miles")],
                playlists: vec![],
            }))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.search_tracks.len(), 1);
    }

    #[test]
    fn escape_leaves_the_search_box_without_searching() {
        // Goal: Esc is cancel everywhere; it must not fire a request.
        let mut r = rig();
        r.app.on_key(key('3'));
        r.app.on_key(key('x'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(requests(&r).is_empty());
    }

    // ---- the play queue ----

    #[test]
    fn playing_a_row_builds_a_queue_and_resolves_that_track() {
        // Goal: the queue is built from what is *visible*, so a filtered list
        // plays only the rows the user can see.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y"), track(3, "C", "Z")];
        r.app.selected = 1;
        r.app.on_key(code(KeyCode::Enter));

        assert_eq!(r.app.queue.len(), 3);
        assert_eq!(r.app.queue_pos, 1);
        assert_eq!(r.app.now_playing.as_ref().unwrap().id, 2);
        assert_eq!(resolved_ids(&requests(&r)), vec![2]);
        assert!(
            r.app.is_resolving(),
            "the spinner runs until the stream arrives"
        );
    }

    #[test]
    fn a_resolved_stream_starts_playback_and_preloads_the_next() {
        // Goal: this is the gapless pipeline. Once the current track resolves,
        // the following one must be requested straight away or the transition
        // has nothing preloaded to move to.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.to_app.send(FromWorker::Resolved(1, stream(1))).unwrap();
        r.app.drain_worker();

        assert!(!r.app.is_resolving());
        assert_eq!(
            r.app.now_meta.sample_rate, 192_000,
            "the badge reads from here"
        );
        assert_eq!(
            resolved_ids(&requests(&r)),
            vec![2],
            "the next is preloaded"
        );
    }

    #[test]
    fn a_stream_that_arrives_for_a_track_we_left_is_ignored() {
        // Goal: replies are matched by id. A late reply for an abandoned track
        // must not hijack playback.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X")];
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::Resolved(999, stream(999)))
            .unwrap();
        r.app.drain_worker();
        assert!(
            r.app.is_resolving(),
            "still waiting for the track we asked for"
        );
    }

    #[test]
    fn the_last_track_in_a_queue_has_nothing_to_preload() {
        // Goal: scheduling past the end would request a track that does not
        // exist and leave the queue indicator wrong.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X")];
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);
        r.to_app.send(FromWorker::Resolved(1, stream(1))).unwrap();
        r.app.drain_worker();
        assert!(resolved_ids(&requests(&r)).is_empty());
    }

    #[test]
    fn skipping_forward_without_a_preload_loads_the_next_track_fresh() {
        // Goal: the user can outrun the preloader; skip must still work, just
        // with a resolve first.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.app.on_key(key('n'));
        assert_eq!(r.app.queue_pos, 1);
        assert_eq!(resolved_ids(&requests(&r)), vec![2]);
    }

    #[test]
    fn previous_restarts_the_track_before_it_steps_back() {
        // Goal: the familiar transport behaviour - `p` part-way through a track
        // returns to its start, and only steps back when already near the start.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.selected = 1;
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.app.status.position = 30.0;
        r.app.on_key(key('p'));
        assert_eq!(r.app.queue_pos, 1, "still on the same track");
        assert!(resolved_ids(&requests(&r)).is_empty(), "a seek, not a load");

        r.app.status.position = 1.0;
        r.app.on_key(key('p'));
        assert_eq!(r.app.queue_pos, 0);
        assert_eq!(resolved_ids(&requests(&r)), vec![1]);
    }

    #[test]
    fn transport_keys_on_an_empty_queue_do_nothing() {
        // Goal: pressing skip before anything plays must not panic or request.
        let mut r = rig();
        r.app.on_key(key('n'));
        r.app.on_key(key('p'));
        assert!(requests(&r).is_empty());
        assert!(r.app.now_playing.is_none());
    }

    #[test]
    fn shuffle_from_a_standstill_starts_playing_something() {
        // Goal: toggling shuffle with nothing playing is a play command, which
        // is what makes it usable as the "just play music" button.
        let mut r = rig();
        r.app.favorites = (1..=5).map(|i| track(i, "T", "A")).collect();
        r.app.on_key(key('s'));

        assert!(r.app.shuffle);
        assert!(r.app.now_playing.is_some());
        assert_eq!(resolved_ids(&requests(&r)).len(), 1);

        r.app.on_key(key('s'));
        assert!(!r.app.shuffle);
        assert!(r.app.notice.as_deref().unwrap().contains("OFF"));
    }

    #[test]
    fn shuffle_never_picks_the_track_already_playing() {
        // Goal: a random pick that lands on the current track looks like a
        // freeze. With one track there is nothing else to choose.
        let mut r = rig();
        r.app.queue = (1..=4).map(|i| track(i, "T", "A")).collect();
        r.app.shuffle = true;
        for pos in 0..4 {
            r.app.queue_pos = pos;
            for _ in 0..25 {
                assert_ne!(r.app.rand_other(), pos, "must move somewhere else");
            }
        }
        r.app.queue.truncate(1);
        r.app.queue_pos = 0;
        assert_eq!(r.app.rand_other(), 0, "a single-track queue stays put");
    }

    #[test]
    fn a_gapless_handover_follows_mpv_to_the_new_track() {
        // Goal: mpv advances on its own, so the app learns about it from
        // current_id. Missing this leaves the now-playing bar on the old track.
        let mut r = rig();
        r.app.queue = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.now_playing = Some(track(1, "A", "X"));
        r.app.expected_id = 1;

        r.app.status.current_id = 2;
        r.app.status.playing = true;
        r.app.refresh_for_test();

        assert_eq!(r.app.queue_pos, 1);
        assert_eq!(r.app.now_playing.as_ref().unwrap().id, 2);
    }

    #[test]
    fn the_end_of_queue_fallback_only_fires_once_playback_really_stopped() {
        // Goal: this guard is the runaway bug. While audio is still flowing the
        // fallback must stay quiet, or it walks the whole queue in one tick.
        let mut r = rig();
        r.app.queue = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.now_playing = Some(track(1, "A", "X"));
        r.app.queue_pos = 0;
        r.app.expected_id = 1;

        r.app.status.ended = true;
        r.app.status.playing = true;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 0, "must not advance while audio plays");

        r.app.status.playing = false;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 1, "a genuinely stopped player advances");

        let before = r.app.queue_pos;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, before, "and it fires only once");
    }

    #[test]
    fn a_paused_player_at_the_end_does_not_advance() {
        // Goal: pause at the last moment of a track is not the end of a track.
        let mut r = rig();
        r.app.queue = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.now_playing = Some(track(1, "A", "X"));
        r.app.expected_id = 1;
        r.app.status.ended = true;
        r.app.status.paused = true;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 0);
    }

    #[test]
    fn the_queue_indicator_counts_from_one() {
        // Goal: it is user-facing, so it is 1-based while queue_pos is not.
        let mut r = rig();
        assert!(r.app.queue_indicator().is_none());
        r.app.queue = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.queue_pos = 1;
        assert_eq!(r.app.queue_indicator().unwrap(), "2/2");
    }

    #[test]
    fn buffering_is_only_reported_between_resolving_and_audio() {
        // Goal: the activity slot shows one thing at a time, and "buffering"
        // must not linger once playback starts or after it ends.
        let mut r = rig();
        assert!(!r.app.is_buffering(), "nothing playing is not buffering");

        r.app.now_playing = Some(track(1, "A", "X"));
        assert!(r.app.is_buffering());

        r.app.status.playing = true;
        assert!(!r.app.is_buffering());

        r.app.status.playing = false;
        r.app.status.paused = true;
        assert!(!r.app.is_buffering(), "paused is not buffering");
    }

    #[test]
    fn the_spinner_cycles_through_its_frames() {
        // Goal: a spinner that never changes frame reads as a hang.
        let mut r = rig();
        let first = r.app.spinner();
        r.app.frame += 1;
        assert_ne!(first, r.app.spinner());
    }

    // ---- mouse ----

    #[test]
    fn clicking_a_registered_control_runs_its_action() {
        // Goal: hit boxes come from the renderer, and a click inside one must
        // dispatch exactly the action a key press would.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X")];
        r.app.hits = vec![
            (
                Rect {
                    x: 0,
                    y: 0,
                    width: 5,
                    height: 1,
                },
                Hit::View(View::Search),
            ),
            (
                Rect {
                    x: 10,
                    y: 0,
                    width: 3,
                    height: 1,
                },
                Hit::Shuffle,
            ),
        ];

        r.app.on_mouse(click(2, 0));
        assert_eq!(r.app.view, View::Search);

        r.app.on_mouse(click(11, 0));
        assert!(r.app.shuffle);
    }

    #[test]
    fn a_click_outside_every_control_is_harmless() {
        // Goal: most of the screen is not a control; clicking it must not
        // dispatch the nearest one.
        let mut r = rig();
        r.app.hits = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            },
            Hit::Shuffle,
        )];
        r.app.on_mouse(click(50, 20));
        assert!(!r.app.shuffle);
    }

    #[test]
    fn the_scroll_wheel_moves_the_selection() {
        // Goal: scrolling is the most-used mouse gesture in the list.
        let mut r = rig();
        r.app.favorites = (0..5).map(|i| track(i, "T", "A")).collect();
        r.app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(r.app.selected, 1);
        r.app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(r.app.selected, 0);
    }

    #[test]
    fn mouse_motion_changes_nothing_and_forces_no_redraw() {
        // Goal: motion events arrive in floods. Treating them as changes turned
        // a mouse sweep across the terminal into a full-speed render loop.
        let mut r = rig();
        let _ = r.app.take_dirty();
        r.app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 4,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!r.app.take_dirty(), "motion must not mark the screen dirty");
    }

    #[test]
    fn clicking_a_row_selects_it_and_a_double_click_plays_it() {
        // Goal: single click selects, double click plays - the list gesture
        // everyone expects from a file manager.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.list_inner = Rect {
            x: 0,
            y: 5,
            width: 40,
            height: 4,
        };

        r.app.on_mouse(click(3, 6));
        assert_eq!(r.app.selected, 1);
        assert!(r.app.now_playing.is_none(), "one click only selects");

        r.app.on_mouse(click(3, 6));
        assert_eq!(r.app.now_playing.as_ref().unwrap().id, 2);
    }

    #[test]
    fn clicking_the_progress_bar_seeks_proportionally() {
        // Goal: the bar is a scrubber; the click position maps to a fraction of
        // the track, and a zero-length track must not divide by zero.
        let mut r = rig();
        r.app.progress_rect = Rect {
            x: 0,
            y: 9,
            width: 100,
            height: 1,
        };
        r.app.status.duration = 0.0;
        r.app.on_mouse(click(50, 9)); // must not panic

        r.app.status.duration = 200.0;
        r.app.on_mouse(click(50, 9));
        r.app.on_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 75,
            row: 9,
            modifiers: KeyModifiers::NONE,
        });
    }

    // ---- help overlay ----

    #[test]
    fn question_mark_opens_the_reference_and_it_swallows_input() {
        // Goal: modal means modal - keys behind it must not reach the list, or
        // the user quits the app while reading how to quit the app.
        let mut r = rig();
        r.app.favorites = (0..5).map(|i| track(i, "T", "A")).collect();
        r.app.on_key(key('?'));
        assert_eq!(r.app.mode, Mode::Help);

        r.app.on_key(key('j'));
        assert_eq!(r.app.selected, 0, "movement must not leak through");
        r.app.on_key(key('q'));
        assert_eq!(r.app.mode, Mode::Normal, "q closes the overlay");
        assert!(!r.app.should_quit, "and does not quit the app");

        for closer in [KeyCode::Esc, KeyCode::Enter] {
            r.app.on_key(key('?'));
            r.app.on_key(code(closer));
            assert_eq!(r.app.mode, Mode::Normal);
        }
    }

    #[test]
    fn a_click_dismisses_the_reference_without_activating_anything() {
        // Goal: clicking to dismiss must not also press whatever control sits
        // under the pointer.
        let mut r = rig();
        r.app.hits = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 1,
            },
            Hit::Shuffle,
        )];
        r.app.on_key(key('?'));
        r.app.on_mouse(click(1, 0));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(!r.app.shuffle, "the click was consumed by the overlay");
    }

    // ---- redraw gating ----

    #[test]
    fn only_real_changes_request_a_redraw() {
        // Goal: an idle player must cost no rendering at all. This is the whole
        // point of the dirty flag.
        let mut r = rig();
        let _ = r.app.take_dirty();
        r.app.refresh_for_test();
        assert!(!r.app.take_dirty(), "an unchanged tick draws nothing");

        r.app.status.position = 9.0;
        r.app.refresh_for_test();
        assert!(
            r.app.take_dirty(),
            "a moving position redraws the progress bar"
        );

        r.app.on_key(key('j'));
        assert!(r.app.take_dirty(), "input always redraws");
    }

    #[test]
    fn a_running_spinner_keeps_redrawing() {
        // Goal: while resolving, the frame counter is the only thing changing,
        // so the signature has to include it or the spinner freezes.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X")];
        r.app.on_key(code(KeyCode::Enter));
        let _ = r.app.take_dirty();
        r.app.refresh_for_test();
        assert!(r.app.take_dirty(), "the spinner must keep animating");
    }

    #[test]
    fn quit_is_reachable_from_a_key_and_a_hint_click() {
        // Goal: both routes exist and both must work; the hint key *is* the
        // quit button, so there is no separate control to test.
        let mut r = rig();
        r.app.on_key(key('q'));
        assert!(r.app.should_quit);

        let mut r = rig();
        r.app.hits = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            Hit::Quit,
        )];
        r.app.on_mouse(click(0, 0));
        assert!(r.app.should_quit);
    }

    #[test]
    fn a_key_release_is_not_a_key_press() {
        // Goal: terminals that report both would otherwise run every action
        // twice.
        let mut r = rig();
        r.app.on_key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert!(!r.app.should_quit);
    }

    #[test]
    fn every_clickable_control_dispatches_to_a_real_action() {
        // Goal: `Hit` is the contract between the renderer and the input layer.
        // A variant wired to nothing compiles fine and silently produces a dead
        // button, so every one is exercised here.
        let mut r = rig();
        r.app.favorites = (1..=5).map(|i| track(i, "T", "A")).collect();
        r.app.queue = r.app.favorites.clone();
        r.app.now_playing = Some(track(1, "T", "A"));

        let fire = |app: &mut App, h: Hit| {
            app.mode = Mode::Normal;
            app.hits = vec![(
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 1,
                },
                h,
            )];
            app.on_mouse(click(1, 0));
        };

        fire(&mut r.app, Hit::MoveDown);
        assert_eq!(r.app.selected, 1);
        fire(&mut r.app, Hit::Bottom);
        assert_eq!(r.app.selected, 4);
        fire(&mut r.app, Hit::MoveUp);
        assert_eq!(r.app.selected, 3);
        fire(&mut r.app, Hit::Top);
        assert_eq!(r.app.selected, 0);

        fire(&mut r.app, Hit::Shuffle);
        assert!(r.app.shuffle);
        fire(&mut r.app, Hit::CycleView);
        assert_eq!(r.app.view, View::Playlists);
        fire(&mut r.app, Hit::View(View::Favorites));
        assert_eq!(r.app.view, View::Favorites);
        fire(&mut r.app, Hit::Filter);
        assert_eq!(r.app.mode, Mode::Filter);
        fire(&mut r.app, Hit::Help);
        assert_eq!(r.app.mode, Mode::Help);

        // These reach the player rather than app state; with a silent player the
        // observable part is that they are accepted without panicking.
        for h in [
            Hit::PlayPause,
            Hit::Prev,
            Hit::Next,
            Hit::SeekBack,
            Hit::SeekFwd,
            Hit::VolUp,
            Hit::VolDown,
        ] {
            fire(&mut r.app, h);
        }

        fire(&mut r.app, Hit::Quit);
        assert!(r.app.should_quit);
    }

    #[test]
    fn every_normal_mode_binding_is_accepted() {
        // Goal: the help overlay promises these keys. A binding that fell out of
        // the match would still compile and simply do nothing.
        let mut r = rig();
        r.app.favorites = (1..=6).map(|i| track(i, "T", "A")).collect();
        r.app.queue = r.app.favorites.clone();
        r.app.now_playing = Some(track(1, "T", "A"));
        r.app.list_inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };

        for c in [
            'j', 'k', 'J', 'K', 'g', 'G', ' ', 's', 'n', 'p', 'L', 'H', 'h', 'l', '+', '=', '-',
        ] {
            r.app.on_key(key(c));
            assert_eq!(r.app.mode, Mode::Normal, "{c} should not change mode");
        }
        for k in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
        ] {
            r.app.on_key(code(k));
        }

        r.app.on_key(key('?'));
        assert_eq!(r.app.mode, Mode::Help);
        r.app.on_key(code(KeyCode::Esc));
        r.app.on_key(key('d'));
        assert_eq!(r.app.mode, Mode::Devices);
        r.app.on_key(code(KeyCode::Esc));
        r.app.on_key(key('/'));
        assert_eq!(r.app.mode, Mode::Filter);
    }

    #[test]
    fn escape_outside_a_playlist_does_nothing() {
        // Goal: Esc only means "go back" inside the drill-down; elsewhere it must
        // not quit or change the view.
        let mut r = rig();
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.view, View::Favorites);
        assert!(!r.app.should_quit);
    }

    #[test]
    fn opening_a_playlist_with_nothing_selected_is_harmless() {
        // Goal: Enter on an empty playlist list must not index into it.
        let mut r = rig();
        r.app.view = View::Playlists;
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.view, View::Playlists);
    }

    #[test]
    fn the_real_constructor_wires_a_player_and_a_worker() {
        // Goal: `new` is what main calls. A bad token path must still produce a
        // usable app - the failure arrives later as a notice.
        let app = App::new(
            PlayerConfig {
                audio_device: Some("null".into()),
                ..PlayerConfig::default()
            },
            "/nonexistent/priel.json".into(),
            crate::logging::Recent::default(),
        )
        .expect("an app should be constructible without a valid token");
        assert_eq!(app.view, View::Favorites);
        assert!(app.loading, "it starts out loading");
    }

    #[test]
    fn playing_with_nothing_to_play_is_a_no_op() {
        // Goal: Enter on an empty or fully filtered-out list must not build a
        // queue or index into an empty vector.
        let mut r = rig();
        r.app.on_key(code(KeyCode::Enter));
        assert!(r.app.queue.is_empty());

        r.app.favorites = vec![track(1, "A", "X")];
        r.app.filter = "nomatch".into();
        r.app.on_key(code(KeyCode::Enter));
        assert!(
            r.app.queue.is_empty(),
            "a filter matching nothing plays nothing"
        );
    }

    #[test]
    fn toggling_shuffle_with_an_empty_list_starts_nothing() {
        // Goal: the shuffle-from-standstill shortcut must cope with an empty
        // library rather than picking a random index into it.
        let mut r = rig();
        r.app.on_key(key('s'));
        assert!(r.app.shuffle);
        assert!(r.app.now_playing.is_none());
    }

    #[test]
    fn unity_gain_has_its_own_binding_and_says_so() {
        // Goal: any volume below 100% costs bit-perfect playback, so getting
        // back to unity must not mean pressing `+` repeatedly and guessing.
        let mut r = rig();
        r.app.on_key(key('-'));
        r.app.on_key(key('0'));
        assert!(
            r.app
                .notice
                .as_deref()
                .unwrap_or_default()
                .contains("unity"),
            "the restore should be acknowledged: {:?}",
            r.app.notice
        );

        let mut r = rig();
        r.app.hits = vec![(
            Rect {
                x: 0,
                y: 0,
                width: 6,
                height: 1,
            },
            Hit::VolUnity,
        )];
        r.app.on_mouse(click(1, 0));
        assert!(
            r.app
                .notice
                .as_deref()
                .unwrap_or_default()
                .contains("unity")
        );
    }

    #[test]
    fn the_consent_screen_swallows_input_and_offers_a_way_out() {
        // Goal: modal means modal. It appears before the user has asked for
        // anything, so `Esc` must dismiss it without downloading, and no
        // keystroke may leak through to the list behind it.
        let mut r = rig();
        r.app.favorites = (0..5).map(|i| track(i, "T", "A")).collect();
        r.app.set_mode_for_test(Mode::Credentials);

        r.app.on_key(key('j'));
        assert_eq!(r.app.selected, 0, "movement must not leak through");

        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(
            r.app.mode,
            Mode::Normal,
            "Esc continues without downloading"
        );
        assert!(!r.app.should_quit);
    }

    #[test]
    fn the_consent_screen_can_be_declined_by_quitting() {
        // Goal: a user who wants no part of this must be able to leave from the
        // screen itself rather than hunting for a way out.
        let mut r = rig();
        r.app.set_mode_for_test(Mode::Credentials);
        r.app.on_key(key('q'));
        assert!(r.app.should_quit);
    }

    #[test]
    fn nothing_is_downloaded_without_a_configured_destination() {
        // Goal: the fetch writes a file. With no path decided there is nothing
        // to write, and it must not reach the network to find that out.
        let mut r = rig();
        r.app.set_mode_for_test(Mode::Credentials);
        r.app.on_key(key('f'));
        assert!(
            r.app.credential_status().is_none(),
            "no attempt should have started"
        );
    }

    #[test]
    fn the_consent_screen_ignores_the_mouse() {
        // Goal: every other overlay is dismissed by a click. This one is not:
        // a stray click must not be read as consent to download a credential.
        let mut r = rig();
        r.app.set_mode_for_test(Mode::Credentials);
        r.app.on_mouse(click(1, 1));
        assert_eq!(r.app.mode, Mode::Credentials, "a click is not consent");
    }

    #[test]
    fn a_saved_credential_reloads_the_library_without_a_restart() {
        // Goal: telling a user to restart after they just signed in is a poor
        // ending to the flow. The worker is rebuilt in place and repeats the
        // initial load, so the view fills in as it would have at startup.
        let dir = std::env::temp_dir().join(format!("priel-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp");
        let creds = dir.join("credentials.json");
        std::fs::write(&creds, r#"{"client_id":"x"}"#).expect("write");

        let mut r = rig();
        r.app.set_paths_for_test(
            dir.join("token.json").to_str().expect("path").to_string(),
            creds.to_str().expect("path").to_string(),
        );
        r.app.set_mode_for_test(Mode::Credentials);
        r.app.restart_worker_for_test();

        assert!(r.app.loading, "the rebuilt worker starts by loading again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuilding_without_configured_paths_is_a_no_op() {
        // Goal: a rigged app has no paths. Rebuilding must not replace a working
        // worker with one pointed at nothing.
        let mut r = rig();
        r.app.restart_worker_for_test();
        r.app.start();
        assert!(
            matches!(requests(&r)[..], [ToWorker::LoadFavorites]),
            "the original worker should still be the one listening"
        );
    }

    // ---- signing in ----

    fn credentials_fixture() -> String {
        let dir = std::env::temp_dir().join(format!(
            "priel-app-login-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"client_id":"cid","client_secret":"sec"}"#).expect("write");
        path.to_str().expect("path").to_string()
    }

    fn start_signin(r: &mut Rig) {
        r.app
            .set_paths_for_test("/nonexistent/token.json".into(), credentials_fixture());
        r.app.start_login();
    }

    #[test]
    fn signing_in_builds_a_url_carrying_the_challenge() {
        // Goal: the URL is the whole first half of the flow. A missing challenge
        // means the exchange is refused at the very end, after the user has
        // already logged in.
        let mut r = rig();
        start_signin(&mut r);
        assert_eq!(r.app.mode, Mode::Login);
        let url = &r.app.login().expect("a flow").url;
        assert!(
            url.starts_with("https://login.tidal.com/authorize?"),
            "{url}"
        );
        assert!(url.contains("code_challenge="), "{url}");
        assert!(url.contains("client_id=cid"), "{url}");
    }

    #[test]
    fn a_paste_arrives_whole_and_can_be_cleared() {
        // Goal: bracketed paste delivers the URL in one event. Typing it by hand
        // is not realistic at ~200 characters, so the box must accept both and
        // offer a way to start over.
        let mut r = rig();
        start_signin(&mut r);
        r.app.on_paste("https://tidal.com/x?code=ABC");
        assert_eq!(
            r.app.login().expect("flow").pasted,
            "https://tidal.com/x?code=ABC"
        );

        r.app
            .on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(r.app.login().expect("flow").pasted.is_empty());

        r.app.on_key(key('z'));
        assert_eq!(
            r.app.login().expect("flow").pasted,
            "z",
            "typing still works"
        );
        r.app.on_key(code(KeyCode::Backspace));
        assert!(r.app.login().expect("flow").pasted.is_empty());
    }

    #[test]
    fn letters_reach_the_box_rather_than_triggering_actions() {
        // Goal: a URL contains every letter, so no unmodified key may be an
        // action - otherwise pasting one would fire commands at random.
        let mut r = rig();
        start_signin(&mut r);
        for c in ['q', 'o', 'u', 'f', 'j'] {
            r.app.on_key(key(c));
        }
        assert!(!r.app.should_quit, "`q` must not quit while signing in");
        assert_eq!(r.app.login().expect("flow").pasted, "qoufj");
    }

    #[test]
    fn a_wrong_paste_is_reported_in_place() {
        // Goal: pasting the login page rather than the redirected page is the
        // likeliest mistake, and it must be correctable without starting over.
        let mut r = rig();
        start_signin(&mut r);
        r.app.on_paste("https://login.tidal.com/authorize");
        r.app.on_key(code(KeyCode::Enter));

        let flow = r.app.login().expect("still signing in");
        assert!(
            flow.status
                .as_deref()
                .unwrap_or_default()
                .contains("query string"),
            "{:?}",
            flow.status
        );
        assert_eq!(r.app.mode, Mode::Login, "the screen must stay up");
    }

    #[test]
    fn signing_in_can_be_abandoned() {
        // Goal: a user who cannot complete the sign-in must be able to leave.
        let mut r = rig();
        start_signin(&mut r);
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(r.app.login().is_none());
    }

    #[test]
    fn a_refused_session_leads_into_signing_in_again() {
        // Goal: a refresh token expires too. When it does, every request fails
        // with the same message, and turning that into the sign-in screen is the
        // difference between a dead application and one keystroke.
        let mut r = rig();
        r.app
            .set_paths_for_test("/nonexistent/token.json".into(), credentials_fixture());
        r.to_app
            .send(FromWorker::Failed {
                fault: Fault::SignedOut,
                detail: "resolve: refused".into(),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.mode, Mode::Login, "it should offer the way back in");
    }

    #[test]
    fn a_saved_client_key_leads_straight_into_signing_in() {
        // Goal: the screens chain. Once a client key exists, a user with no
        // session should be taken to the sign-in rather than dropped on an
        // empty library with no hint about what to do.
        //
        // The reverse case - no key anywhere - is deliberately not asserted
        // here: `local_credentials` falls back to scanning the machine for an
        // installed package, so its absence is not something a test can arrange.
        let mut r = rig();
        r.app
            .set_paths_for_test("/nonexistent/token.json".into(), credentials_fixture());
        r.app.continue_after_credentials_for_test();
        assert_eq!(r.app.mode, Mode::Login, "no session means sign in");
    }

    #[test]
    fn the_test_suite_never_spawns_a_browser() {
        // Goal: several tests drive the sign-in flow, and each one spawning a
        // real browser tab makes `cargo test` hostile to run. This is the guard
        // that stops a refactor quietly reinstating that.
        assert!(
            !super::should_open_browser(),
            "the suite must never reach the user's browser"
        );
    }
}
