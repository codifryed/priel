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
use priel_player::Alteration;
use priel_player::graph::{Attribution, AudioGraph, GraphError, GraphNode, NodeRole};
use priel_player::{AudioDevice, PlaybackStatus, Player, PlayerConfig};

#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::worker::{self, FromWorker, Task, ToWorker, Worker};

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
    Reload,
    CycleView,
    Help,
    Graph,
    Quit,
}

#[derive(PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Filter,      // local filter of the current list
    Search,      // editing the global TIDAL search query
    Help,        // the shortcut reference is up; it swallows input until dismissed
    Log,         // the recent diagnostics are up; modal in the same way
    Graph,       // the chain to the output device is up; modal in the same way
    Devices,     // the output picker is up; modal in the same way
    Credentials, // first run with no client identity; asking before fetching one
    Login,       // signing in: browser is open, waiting for the redirected URL
}

/// What one line of the audio-graph overlay says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphRowKind {
    /// A node on the path from priel to the device.
    Node,
    /// The node the chain says is altering the samples, or the sentence naming
    /// it. Drawn the colour the fidelity badge uses for the same finding, so
    /// the two read as one answer rather than two opinions.
    Culprit,
    /// The connector drawn between two nodes.
    Link,
    /// Prose: what is being waited for, or why there is nothing to show.
    Note,
}

/// One line of the audio-graph overlay, ready to draw.
///
/// Built in `App` rather than in the renderer so the overlay's scroll bounds and
/// what it paints count the same lines. A renderer that laid the rows out itself
/// would be the only thing that knew how many there were, and the scroll would
/// drift the moment either changed.
pub struct GraphRow {
    /// The left column: a node, or a whole sentence.
    pub label: String,
    /// The right column: what that node negotiated. Empty for prose.
    pub detail: String,
    pub kind: GraphRowKind,
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

/// How far through a paged listing one view has got.
///
/// A flat `Vec` per view has nowhere to record how much of the listing it is,
/// which is why a listing used to stop at whatever the first request returned.
/// Kept as a struct rather than three more fields on `App` so that giving the
/// other views the same treatment is one field each, not three.
#[derive(Default)]
pub struct Paging {
    /// Rows the listing has, as best as is known. Zero until something says.
    ///
    /// Usually the service's own count, arriving with the first page. The
    /// playlist-tracks view is the exception: a playlist row already carries its
    /// track count, so that view knows its length before it has asked for a
    /// single track.
    pub total: u32,
    /// The offset of the request in flight, if any.
    ///
    /// One at a time, and the identity a reply is matched against: a page for
    /// an offset nobody is waiting on belongs to a listing that has since been
    /// thrown away, and appending it would interleave two different lists.
    wanted: Option<u32>,
    /// A page request failed. Latched, because the trigger runs on every tick:
    /// without this a service that is refusing would be asked ten times a
    /// second for as long as priel stayed open. Cleared by a deliberate user
    /// action - reloading, or coming back to the view.
    stalled: bool,
}

impl Paging {
    /// Rows are missing, and nothing is stopping us asking for them.
    fn wants_more(&self, loaded: usize) -> bool {
        if self.wanted.is_some() || self.stalled {
            return false;
        }
        usize::try_from(self.total).is_ok_and(|total| loaded < total)
    }

    /// Start the listing again from the top, waiting on its first page.
    ///
    /// `known_total` is what the caller can already say the length is - a
    /// playlist's own track count - or zero when only the service can say.
    /// Restarting also clears the stall latch, so no failure can leave a view
    /// permanently unable to load.
    fn restart(&mut self, known_total: u32) {
        *self = Self {
            total: known_total,
            wanted: Some(0),
            stalled: false,
        };
    }

    /// Take in a page that answers `offset`, and record how much is left.
    ///
    /// The first page replaces and every later one appends. Appending is what
    /// keeps the user's row under their cursor: selection is an index into the
    /// filtered rows, and rows that only ever arrive after the ones already
    /// there cannot shift it.
    fn absorb<T>(&mut self, rows: &mut Vec<T>, offset: u32, page: priel_core::Page<T>) {
        debug_assert!(
            offset == 0 || usize::try_from(offset).is_ok_and(|o| o == rows.len()),
            "a page either restarts the list or continues where it ended"
        );
        self.wanted = None;
        let ran_out = page.items.is_empty() && offset > 0;
        if offset == 0 {
            *rows = page.items;
        } else {
            rows.extend(page.items);
        }
        let loaded = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        if ran_out {
            // The service has no more rows to give, whatever it counted. Its
            // count is sometimes the larger of the two, and believing it over
            // an empty answer asks for that same empty page on every tick.
            self.total = loaded;
            return;
        }
        // Never below what is already loaded: an answer carrying no count would
        // otherwise read as "0 available" beside rows that are plainly on
        // screen. Never below what was already known either, so a page with no
        // count of its own cannot unsay a playlist's own track count.
        self.total = page.total.max(loaded).max(self.total);
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
    /// The query the loaded results answer, which is not `search_query`: that
    /// one changes under the user's fingers while the box is open, and a reply
    /// has to be matched against what was actually asked for.
    search_asked: String,
    /// How much of each listing has been fetched. One per view, because the
    /// stall latch and the page in flight belong to a listing rather than to
    /// the app.
    pub favorites_paging: Paging,
    pub playlists_paging: Paging,
    pub playlist_tracks_paging: Paging,
    pub search_paging: Paging,

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
    /// The last chain the worker read, or the reason it could not. `None` while
    /// a read is in flight, which is what the overlay says it is doing.
    audio_graph: Option<Result<AudioGraph, GraphError>>,
    /// How far down the audio-graph overlay is scrolled, from the top.
    graph_scroll: usize,
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
    /// Where the picker painted its exclusivity toggle, so a click on it runs
    /// the same method the `x` key does. Rebuilt by the renderer, like every
    /// other hit box here.
    pub device_exclusive_rect: Rect,
    /// Exclusive use of the output device has been asked for.
    ///
    /// What the *listener* wants, which is not the same thing as what the
    /// player achieved - that is `status.access`, and the badge reports it. Kept
    /// here because it is the thing the picker toggles and priel itself never
    /// changes.
    exclusive: bool,
    /// When the list was last asked for. The picker says it is still looking
    /// until this is old enough to mean nothing is going to answer.
    devices_asked: Option<Instant>,
    /// The device failure already shown. Latched so it is reported once rather
    /// than on every tick for as long as the player carries it.
    reported_device_error: Option<String>,
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
        // Read before the config is handed over: the picker shows what was
        // asked for, and `--exclusive` is where a session starts from.
        let exclusive = player.exclusive;
        let player = Player::with_config(player)?;
        let creds_path = Credentials::default_path();
        let has_credentials = priel_core::auth::local_credentials(&creds_path).is_some();
        let worker = worker::spawn(token_path.clone(), creds_path.clone());
        let mut app = Self::with(player, worker);
        app.exclusive = exclusive;
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
            search_asked: String::new(),
            favorites_paging: Paging::default(),
            playlists_paging: Paging::default(),
            playlist_tracks_paging: Paging::default(),
            search_paging: Paging::default(),
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
            audio_graph: None,
            graph_scroll: 0,
            devices: Vec::new(),
            device_selected: 0,
            device_offset: 0,
            device_rows: Vec::new(),
            device_exclusive_rect: Rect::default(),
            exclusive: false,
            devices_asked: None,
            reported_device_error: None,
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
        self.load_favorites_from_the_top();
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
        self.load_favorites_from_the_top();
    }

    /// Ask for the first page of favorites, discarding whatever was loaded.
    ///
    /// Also the retry after a page failed: restarting clears the stall latch, so
    /// nothing can leave the view permanently unable to load.
    fn load_favorites_from_the_top(&mut self) {
        self.favorites_paging.restart(0);
        self.loading = true;
        self.ask(ToWorker::LoadFavorites {
            offset: 0,
            limit: worker::FAVORITES_PAGE,
        });
    }

    /// Ask for the first page of a playlist's tracks, discarding any loaded.
    ///
    /// `num_tracks` is the count the playlist row carried, which is what lets
    /// this view know its own length before its first page of tracks arrives.
    fn load_playlist_tracks_from_the_top(&mut self, uuid: String, num_tracks: u32) {
        self.playlist_tracks.clear();
        self.playlist_tracks_paging.restart(num_tracks);
        self.loading = true;
        self.ask(ToWorker::LoadPlaylistTracks {
            uuid,
            offset: 0,
            limit: worker::PLAYLIST_TRACKS_PAGE,
        });
    }

    /// Fetch the list on screen again, from its first page.
    ///
    /// The only way to retry a page that failed used to be to leave the view and
    /// come back, because that is what cleared the anti-spin latch - which is
    /// not something a user has any reason to think of. Restarting a listing
    /// clears the latch, so this is that retry made deliberate. It is also the
    /// only way to pick up rows added to a listing since it was loaded.
    ///
    /// A view with nothing behind it - no playlist open, no query run - has no
    /// first page to ask for, and asks for nothing.
    fn reload_view(&mut self) {
        match self.view {
            View::Favorites => self.load_favorites_from_the_top(),
            View::Playlists => self.load_playlists_from_the_top(),
            View::PlaylistTracks => {
                if let Some((uuid, _)) = self.open_playlist.clone() {
                    // The playlist's own track count has not changed with the
                    // rows, so the view keeps knowing how long it is.
                    let known = self.playlist_tracks_paging.total;
                    self.load_playlist_tracks_from_the_top(uuid, known);
                }
            }
            View::Search => {
                // The query the rows answer, not the text in the box: the box
                // may be halfway through an edit the user has not run yet.
                let query = self.search_asked.clone();
                if !query.is_empty() {
                    self.run_search(query);
                }
            }
        }
    }

    /// Ask for the first page of the user's playlists, discarding any loaded.
    fn load_playlists_from_the_top(&mut self) {
        self.playlists_paging.restart(0);
        self.loading = true;
        self.ask(ToWorker::LoadPlaylists {
            offset: 0,
            limit: worker::PLAYLISTS_PAGE,
        });
    }

    /// The paging state of the view on screen.
    ///
    /// One listing at a time is on screen, and only that listing's rows can be
    /// scrolled towards, so the trigger below needs no more than this.
    fn paging(&self) -> &Paging {
        match self.view {
            View::Favorites => &self.favorites_paging,
            View::Playlists => &self.playlists_paging,
            View::PlaylistTracks => &self.playlist_tracks_paging,
            View::Search => &self.search_paging,
        }
    }

    fn paging_mut(&mut self) -> &mut Paging {
        match self.view {
            View::Favorites => &mut self.favorites_paging,
            View::Playlists => &mut self.playlists_paging,
            View::PlaylistTracks => &mut self.playlist_tracks_paging,
            View::Search => &mut self.search_paging,
        }
    }

    /// The listing a failed request belongs to, and the page it was for.
    ///
    /// Matched on the task's whole identity rather than on the view on screen:
    /// a page can fail long after the user has moved on, and clearing another
    /// listing's slot would let a second copy of its page go out - or latch a
    /// listing that has nothing wrong with it. `None` when the failure belongs
    /// to no listing at all, or to one the user has already left.
    fn paging_for(&mut self, task: &Task) -> Option<(&mut Paging, u32)> {
        match task {
            Task::Favorites { offset } => Some((&mut self.favorites_paging, *offset)),
            Task::Playlists { offset } => Some((&mut self.playlists_paging, *offset)),
            Task::PlaylistTracks { uuid, offset } => {
                let open = self.open_playlist.as_ref().is_some_and(|(u, _)| u == uuid);
                open.then_some((&mut self.playlist_tracks_paging, *offset))
            }
            Task::Search { query, offset } => {
                (*query == self.search_asked).then_some((&mut self.search_paging, *offset))
            }
            Task::Startup | Task::Resolve => None,
        }
    }

    /// Rows of the current listing that have been fetched.
    ///
    /// Not `visible_len`: an offset counts rows the service has handed over, and
    /// the local filter hides rows without unfetching them. Asking for
    /// `visible_len` as the next offset would skip everything filtered out.
    fn loaded_rows(&self) -> usize {
        match self.view {
            View::Favorites => self.favorites.len(),
            View::Playlists => self.playlists.len(),
            View::PlaylistTracks => self.playlist_tracks.len(),
            View::Search => self.search_tracks.len(),
        }
    }

    /// What would fetch `offset` for the view on screen, if it has anything to
    /// ask for: a playlist view with no playlist open, and a search with no
    /// query behind it, have nothing to page.
    fn page_request(&self, offset: u32) -> Option<ToWorker> {
        Some(match self.view {
            View::Favorites => ToWorker::LoadFavorites {
                offset,
                limit: worker::FAVORITES_PAGE,
            },
            View::Playlists => ToWorker::LoadPlaylists {
                offset,
                limit: worker::PLAYLISTS_PAGE,
            },
            View::PlaylistTracks => ToWorker::LoadPlaylistTracks {
                uuid: self.open_playlist.as_ref()?.0.clone(),
                offset,
                limit: worker::PLAYLIST_TRACKS_PAGE,
            },
            View::Search if !self.search_asked.is_empty() => ToWorker::Search {
                query: self.search_asked.clone(),
                offset,
                limit: worker::SEARCH_PAGE,
            },
            View::Search => return None,
        })
    }

    /// Ask for the next page when the selection nears the end of what is loaded.
    ///
    /// Driven from the status tick rather than from a key, so scrolling by
    /// keyboard, by wheel and by click all reach it down one path. The guards
    /// are what keep that cheap: the ordinary cases - a list fully loaded, a
    /// page already in flight, a view that gave up - all answer before anything
    /// is walked.
    ///
    /// One trigger for all four views rather than four of them: everything that
    /// differs between the listings is in `paging`, `loaded_rows` and
    /// `page_request`, and the guards are the part that must not drift.
    fn page_in_more(&mut self) {
        // The renderer is what publishes the list geometry, so before the first
        // frame "near the bottom" is a question with no answer. Acting on a
        // zero-height list would page the whole library in before anything had
        // been drawn.
        if self.list_inner.height == 0 {
            return;
        }
        let loaded = self.loaded_rows();
        if !self.paging().wants_more(loaded) {
            return;
        }
        // A screenful of lookahead: the next page has to be there before the
        // user scrolls into the gap, and one screen is as far as a single
        // keystroke can take them.
        if self.selected + self.full_page() < self.visible_len() {
            return;
        }
        let Ok(offset) = u32::try_from(loaded) else {
            return; // more rows than an offset can name; there is nothing to ask
        };
        let Some(req) = self.page_request(offset) else {
            return;
        };
        self.paging_mut().wanted = Some(offset);
        self.ask(req);
    }

    /// A page of favorites arrived.
    ///
    /// The offset it was asked for is its identity, exactly as a resolve is
    /// matched by track id: a page for an offset the view is no longer waiting
    /// on belongs to a listing that has since been thrown away.
    fn on_favorites_page(&mut self, offset: u32, page: priel_core::Page<Track>) {
        if self.favorites_paging.wanted != Some(offset) {
            log::debug!("dropping a favorites page at offset {offset}: nothing is waiting for it");
            return;
        }
        let mut rows = std::mem::take(&mut self.favorites);
        self.favorites_paging.absorb(&mut rows, offset, page);
        self.favorites = rows;
        self.loading = false;
        let loaded = self.favorites.len();
        self.notice = Some(match rows_missing(loaded, self.favorites_paging.total) {
            Some(total) => format!("{loaded} of {total} favorites"),
            None => format!("{loaded} favorites"),
        });
        if self.view == View::Favorites {
            self.clamp_selection();
        }
    }

    /// A page of the user's playlists arrived, matched by the offset it answers.
    fn on_playlists_page(&mut self, offset: u32, page: priel_core::Page<Playlist>) {
        if self.playlists_paging.wanted != Some(offset) {
            log::debug!("dropping a playlists page at offset {offset}: nothing is waiting for it");
            return;
        }
        let mut rows = std::mem::take(&mut self.playlists);
        self.playlists_paging.absorb(&mut rows, offset, page);
        self.playlists = rows;
        self.loading = false;
        if self.view == View::Playlists {
            self.clamp_selection();
        }
    }

    /// A page of one playlist's tracks arrived.
    ///
    /// Two things make one of these stale and both have to be checked. A reply
    /// for a playlist the user has left is the one that was always guarded
    /// against; a page of the *open* playlist that was superseded before it
    /// arrived is just as wrong, and paging is what made it possible.
    fn on_playlist_tracks_page(&mut self, uuid: &str, offset: u32, page: priel_core::Page<Track>) {
        if self.open_playlist.as_ref().is_none_or(|(u, _)| u != uuid) {
            log::debug!("dropping tracks for {uuid}: that playlist is not open");
            return;
        }
        if self.playlist_tracks_paging.wanted != Some(offset) {
            log::debug!("dropping {uuid} tracks at offset {offset}: nothing is waiting for it");
            return;
        }
        let mut rows = std::mem::take(&mut self.playlist_tracks);
        self.playlist_tracks_paging.absorb(&mut rows, offset, page);
        self.playlist_tracks = rows;
        self.loading = false;
        self.clamp_selection();
    }

    /// A page of search results arrived.
    ///
    /// The query is half the identity: both a new search and a reload ask for
    /// offset zero, so the offset alone cannot tell a page of the results on
    /// screen from a page of the query the user has just replaced.
    fn on_search_page(&mut self, query: &str, offset: u32, page: priel_core::Page<Track>) {
        if query != self.search_asked {
            log::debug!("dropping results for {query:?}: the query has moved on");
            return;
        }
        if self.search_paging.wanted != Some(offset) {
            log::debug!("dropping results at offset {offset}: nothing is waiting for them");
            return;
        }
        // A fresh set of results starts at the top; rows added to the end of the
        // ones being read must leave the cursor exactly where it is. Sending it
        // to the top on every reply is what this replaces.
        if offset == 0 {
            self.selected = 0;
        }
        let mut rows = std::mem::take(&mut self.search_tracks);
        self.search_paging.absorb(&mut rows, offset, page);
        self.search_tracks = rows;
        self.loading = false;
        let loaded = self.search_tracks.len();
        self.notice = Some(match rows_missing(loaded, self.search_paging.total) {
            Some(total) => format!("{loaded} of {total} results"),
            None => format!("{loaded} results"),
        });
        if self.view == View::Search {
            self.clamp_selection();
        }
    }

    /// How long the current listing is, while some of it is still missing.
    ///
    /// `None` once everything is loaded, so the heading mentions a total only
    /// while it still tells the user something.
    #[must_use]
    pub fn rows_available(&self) -> Option<u32> {
        rows_missing(self.loaded_rows(), self.paging().total)
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

    /// Walk the rows of the current view that pass the local filter.
    ///
    /// The one place the filter is applied. `visible` and `visible_len` both go
    /// through here so they cannot come to disagree about which rows exist -
    /// one collects the indices, the other only counts them.
    fn each_visible(&self, mut row: impl FnMut(usize)) {
        let f = self.filter.to_lowercase();
        if self.view == View::Playlists {
            for (i, p) in self.playlists.iter().enumerate() {
                if row_matches(&p.title, "", &f) {
                    row(i);
                }
            }
        } else {
            for (i, t) in self.current_tracks().iter().enumerate() {
                if row_matches(&t.title, &t.artist, &f) {
                    row(i);
                }
            }
        }
    }

    /// Indices into the current view's items matching the local filter.
    ///
    /// Callers on the render path must call this **once per frame** and reuse
    /// the result: it allocates and walks the whole list, so calling it per row
    /// is quadratic.
    pub fn visible(&self) -> Vec<usize> {
        let mut indices = Vec::with_capacity(self.visible_len());
        self.each_visible(|i| indices.push(i));
        indices
    }

    /// How many rows the current view shows, without building the index list.
    ///
    /// `visible()` allocates, and the paging trigger reads this on every tick.
    /// With no filter - which is nearly always - every row is visible, and the
    /// answer needs no walk at all.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        if self.filter.is_empty() {
            return self.loaded_rows();
        }
        let mut count = 0;
        self.each_visible(|_| count += 1);
        count
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

    /// A request came back as a failure.
    ///
    /// `task` says which one, so the view that was waiting stops waiting. The
    /// rows already loaded are left exactly as they are: a page that did not
    /// arrive is a page missing from the end, not a reason to empty the list.
    fn on_failed(&mut self, task: &Task, fault: Fault, detail: &str) {
        self.loading = false;
        if let Some((paging, offset)) = self.paging_for(task)
            && paging.wanted == Some(offset)
        {
            paging.wanted = None;
            // Latched rather than retried. The trigger runs on every tick, so a
            // service that is down would otherwise be asked ten times a second
            // until priel was closed.
            paging.stalled = true;
        }
        // Branching on the classification, never on the words. This was
        // `e.contains("log in again")`, which made the core's wording
        // load-bearing: rewording that sentence would have quietly stopped the
        // login screen from being offered.
        match fault {
            Fault::SignedOut => self.offer_relogin(detail),
            Fault::Unreachable => {
                self.notice = Some(format!("⚠ could not reach the service: {detail}"));
            }
            Fault::Refused => self.notice = Some(format!("⚠ {detail}")),
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
                FromWorker::Favorites { offset, page } => self.on_favorites_page(offset, page),
                FromWorker::Playlists { offset, page } => self.on_playlists_page(offset, page),
                FromWorker::PlaylistTracks { uuid, offset, page } => {
                    self.on_playlist_tracks_page(&uuid, offset, page);
                }
                FromWorker::SearchResults {
                    query,
                    offset,
                    page,
                } => self.on_search_page(&query, offset, page),
                FromWorker::Resolved(id, r) => self.on_resolved(id, &r),
                FromWorker::AudioGraph(read) => {
                    self.audio_graph = Some(read);
                    // The reply can be longer than the request that opened the
                    // overlay left room for, so the scroll starts again rather
                    // than pointing past the end of the new reading.
                    self.graph_scroll = 0;
                }
                FromWorker::Failed {
                    task,
                    fault,
                    detail,
                } => self.on_failed(&task, fault, &detail),
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
        self.page_in_more();
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
        self.report_device_error();

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
        // Coming back to a list is the retry. A page that failed left that view
        // unwilling to ask again; arriving here is the deliberate user action
        // that clears it, and it is worth at most one more request.
        self.paging_mut().stalled = false;
        match v {
            View::Playlists if self.playlists.is_empty() => self.load_playlists_from_the_top(),
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
            self.selected = 0;
            self.list_offset = 0;
            self.filter.clear();
            // The playlist row already says how many tracks there are, so this
            // view knows where its end is before a single track has arrived -
            // and does not depend on a total the tracks response may not carry.
            self.load_playlist_tracks_from_the_top(p.uuid, p.num_tracks);
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

    /// Build the play queue from the rows on screen and start at one of them.
    ///
    /// The queue is a snapshot, and a page of favorites that lands later does
    /// **not** join it. The listener chose a set of tracks; extending it behind
    /// their back would change what plays next without being asked, and the
    /// track that follows the last one they saw would no longer be the one they
    /// picked. Pressing Enter again rebuilds the queue from the larger list,
    /// which is the deliberate way to take the new rows.
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
            Mode::Graph => self.on_key_graph(key),
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

    /// Open the audio-graph overlay and ask the worker to read the chain.
    ///
    /// Shared by the key press and the click so the two paths cannot drift.
    /// Reading it means running `pw-dump` and waiting for it, so the request
    /// goes to the worker and the overlay opens straight away with nothing in
    /// it - the render loop waits for nothing.
    fn open_graph(&mut self) {
        self.mode = Mode::Graph;
        self.graph_scroll = 0;
        // Cleared rather than kept: the last reading shown as if it were
        // current is exactly the lie this overlay exists to stop.
        self.audio_graph = None;
        // A direct device puts priel outside the sound server entirely, so
        // there is no graph to read rather than a graph priel is missing from.
        // Asking anyway would answer "priel has no stream in the graph", which
        // reads as "nothing is playing yet" - the opposite of what is true.
        if self.status.bypasses_sound_server() {
            self.audio_graph = Some(Err(GraphError::Bypassed));
            return;
        }
        let _ = self.worker.tx.send(ToWorker::ReadAudioGraph);
    }

    /// The audio-graph overlay: modal like the log one, scrolled like every list.
    fn on_key_graph(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('D' | 'q' | ' ') => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.graph_scroll = (self.graph_scroll + 1).min(self.graph_scroll_max());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.graph_scroll = self.graph_scroll.saturating_sub(1);
            }
            KeyCode::Char('g') => self.graph_scroll = 0,
            KeyCode::Char('G') => self.graph_scroll = self.graph_scroll_max(),
            _ => {}
        }
        self.dirty = true;
    }

    fn graph_scroll_max(&self) -> usize {
        self.graph_rows().len().saturating_sub(1)
    }

    /// How far down the audio-graph overlay is scrolled.
    #[must_use]
    pub fn graph_offset(&self) -> usize {
        self.graph_scroll
    }

    /// The lines of the audio-graph overlay, top to bottom.
    #[must_use]
    pub fn graph_rows(&self) -> Vec<GraphRow> {
        match &self.audio_graph {
            None => vec![note("Reading the graph…")],
            Some(Err(e)) => {
                let mut rows = vec![note(&e.to_string())];
                rows.extend(e.hint().map(note));
                rows
            }
            Some(Ok(g)) => {
                // The same two readings the badge is built from, so the row and
                // the overlay cannot disagree about one track.
                let source = self.status.decoded_format(self.now_meta.bit_depth);
                let observed = self.status.fidelity(self.now_meta.bit_depth).alteration();
                let blame = g.attribute(source, observed);
                let mut rows = path_rows(g, blame);
                rows.extend(blame_row(g, blame, observed));
                rows
            }
        }
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
            KeyCode::Enter => self.choose_device(self.device_selected),
            KeyCode::Char('x') => self.toggle_exclusive(),
            _ => {}
        }
        self.dirty = true;
    }

    /// Ask for the output device exclusively, or give it back.
    ///
    /// The one place that changes it: the `x` key and a click on the toggle
    /// both arrive here, so the two cannot drift apart. Deliberately does not
    /// close the picker - this is a setting whose effect is worth watching on
    /// the badge, not a choice to make and walk away from. And deliberately
    /// separate from choosing a device: taking a device is not implied by
    /// selecting one.
    fn toggle_exclusive(&mut self) {
        self.exclusive = !self.exclusive;
        self.player.set_exclusive(self.exclusive);
        self.notice = Some(if self.exclusive {
            "Asking for the device exclusively — this session only, --exclusive makes it permanent"
                .to_string()
        } else {
            "Sharing the output device again — this session only, --exclusive makes it permanent"
                .to_string()
        });
        self.dirty = true;
    }

    /// Has exclusive use of the device been asked for?
    ///
    /// What was *asked*, not what was got: the player answers the second
    /// through `status.access`, and only that may reach the badge.
    #[must_use]
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    /// Move the output to the device on this row, for this session.
    ///
    /// The one place that changes the output: the Enter key and a click on a
    /// row both arrive here, so the two cannot drift apart. Nothing is written
    /// anywhere - priel reads no configuration file, and `--device` is what
    /// makes a choice outlive the session, which is what the overlay says.
    fn choose_device(&mut self, index: usize) {
        let Some(device) = self.devices.get(index) else {
            return;
        };
        let name = device.name.clone();
        let label = if device.description.is_empty() {
            name.clone()
        } else {
            device.description.clone()
        };
        self.player.set_device(&name);
        self.notice = Some(format!("Output: {label} — this session only"));
        self.mode = Mode::Normal;
        self.dirty = true;
    }

    /// Report a device change that did not take, once.
    ///
    /// The player carries the reason in its status until the next change is
    /// accepted, so without the latch this would replace every other notice on
    /// screen ten times a second.
    fn report_device_error(&mut self) {
        if self.status.device_error == self.reported_device_error {
            return;
        }
        self.reported_device_error = self.status.device_error.clone();
        if let Some(detail) = self.reported_device_error.clone() {
            self.notice = Some(detail);
            self.dirty = true;
        }
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

    /// A click inside the picker. On the toggle it flips exclusivity, on a row
    /// it takes that row, and anywhere else it dismisses, as a click on the log
    /// overlay does.
    fn click_device(&mut self, col: u16, row: u16) {
        if hit(self.device_exclusive_rect, col, row) {
            self.toggle_exclusive();
            return;
        }
        match self
            .device_rows
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, i)| *i)
        {
            Some(i) => {
                self.device_selected = i;
                self.choose_device(i);
            }
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
                self.submit_search();
            }
            KeyCode::Backspace => {
                self.search_query.pop();
            }
            KeyCode::Char(c) => self.search_query.push(c),
            _ => {}
        }
    }

    /// Run the query in the box, from its first page.
    ///
    /// The query is remembered as `search_asked` because the box keeps taking
    /// keys after the request has gone out: a reply matched against
    /// `search_query` would be dropped the moment the user typed another letter.
    fn submit_search(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            return; // an accidental Enter on an empty box is not a request
        }
        self.run_search(query);
    }

    /// Run `query` from its first page, discarding whatever was on screen.
    fn run_search(&mut self, query: String) {
        self.search_asked.clone_from(&query);
        self.search_tracks.clear();
        self.search_paging.restart(0);
        self.selected = 0;
        self.loading = true;
        self.ask(ToWorker::Search {
            query,
            offset: 0,
            limit: worker::SEARCH_PAGE,
        });
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
            KeyCode::Char('D') => self.open_graph(),
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
            KeyCode::Char('r') => self.reload_view(),
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
        if self.mode == Mode::Graph {
            match m.kind {
                MouseEventKind::ScrollDown => {
                    self.graph_scroll = (self.graph_scroll + 1).min(self.graph_scroll_max());
                    self.dirty = true;
                }
                MouseEventKind::ScrollUp => {
                    self.graph_scroll = self.graph_scroll.saturating_sub(1);
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
            Hit::Reload => self.reload_view(),
            Hit::CycleView => self.cycle_view(),
            Hit::Help => self.mode = Mode::Help,
            Hit::Graph => self.open_graph(),
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

/// A line of prose in the audio-graph overlay.
fn note(text: &str) -> GraphRow {
    GraphRow {
        label: text.to_string(),
        detail: String::new(),
        kind: GraphRowKind::Note,
    }
}

/// The chain, one row per node with a connector between them.
///
/// Pure so the overlay's content is a table of tests rather than something only
/// a rendered frame can show. The accusation arrives already decided, from
/// `AudioGraph::attribute` in the player crate: the grading is the player's and
/// this only draws it.
fn path_rows(g: &AudioGraph, blame: Attribution) -> Vec<GraphRow> {
    let accused = match blame {
        Attribution::Node { index, .. } => Some(index),
        _ => None,
    };
    let mut rows = Vec::with_capacity(g.path.len() * 2);
    for (hop, node) in g.path.iter().enumerate() {
        if hop > 0 {
            rows.push(GraphRow {
                label: "  │".into(),
                detail: String::new(),
                kind: GraphRowKind::Link,
            });
        }
        let marked = accused == Some(hop);
        rows.push(GraphRow {
            label: node_label(node, marked),
            detail: negotiated(node),
            kind: if marked {
                GraphRowKind::Culprit
            } else {
                GraphRowKind::Node
            },
        });
    }
    rows
}

/// What the chain has to say about the samples being altered, in a sentence.
///
/// Empty for a chain that alters nothing, and empty for an idle player: an
/// accusation on a clean chain teaches the reader to ignore the marker, and a
/// disclaimer on every idle reading is noise over rows that already say they
/// have settled on nothing.
///
/// The two admissions are deliberately different sentences. "Nothing on this
/// path did it" is a finding - every node was compared and none of them
/// diverged. "Not enough was negotiated" is the absence of one, and reporting
/// it as the first would claim a comparison that never happened.
fn blame_row(g: &AudioGraph, blame: Attribution, observed: Option<Alteration>) -> Option<GraphRow> {
    match blame {
        Attribution::Node { index, alteration } => {
            let node = g.path.get(index)?;
            let verb = match alteration {
                Alteration::Truncated => "truncating the samples",
                _ => "resampling",
            };
            Some(GraphRow {
                label: format!("  {} is {verb}.", display_name(node)),
                detail: String::new(),
                kind: GraphRowKind::Culprit,
            })
        }
        Attribution::Unexplained(Alteration::Truncated) => Some(note(
            "  The width narrowed, and nothing on this path did it.",
        )),
        Attribution::Unexplained(_) => {
            Some(note("  The rate changed, and nothing on this path did it."))
        }
        // Something was measured and there was nothing here to check it
        // against, which is not the same admission as the one above.
        Attribution::NothingToCompare if observed.is_some() => {
            Some(note("  Not enough was negotiated here to say what did it."))
        }
        // A clean chain accuses nobody, and an idle one is not asked to.
        Attribution::Clean | Attribution::NothingToCompare => None,
    }
}

/// How a node introduces itself.
///
/// The stream is libmpv's and `PipeWire` calls it `mpv`, which is a puzzle on
/// screen until it is labelled - so it is labelled.
///
/// The marker replaces the indent rather than sitting inside it, so the names
/// still line up under one another and the accused row is the only one that
/// breaks the left edge.
fn node_label(node: &GraphNode, marked: bool) -> String {
    let name = display_name(node);
    let indent = if marked { "⚠ " } else { "  " };
    match node.role {
        NodeRole::Stream => format!("{indent}{name}  (priel)"),
        NodeRole::Intermediate => format!("{indent}{name}"),
        NodeRole::Device => format!("{indent}{name}  (device)"),
    }
}

/// The name a reader would recognise the node by.
fn display_name(node: &GraphNode) -> &str {
    if node.description.is_empty() {
        &node.name
    } else {
        &node.description
    }
}

/// What a node settled on, or a plain statement that it has not settled yet.
///
/// Rates are written the way the badge writes them - `crate::ui::fmt_khz` is
/// shared rather than reimplemented, so 44.1 kHz cannot appear as "44 kHz" in
/// one place and "44.1 kHz" in the other.
fn negotiated(node: &GraphNode) -> String {
    let Some(rate) = node.rate_hz else {
        return "no format yet".into();
    };
    let mut out = crate::ui::fmt_khz(rate);
    if let Some(format) = &node.format {
        out.push_str("  ");
        out.push_str(format);
    }
    if let Some(channels) = node.channels {
        out.push_str("  ");
        out.push_str(&channels.to_string());
        out.push_str(" ch");
    }
    out
}

/// Does a row match the local filter? `filter_lower` must already be lowercased
/// by the caller, which is what keeps this off the per-row allocation path.
/// An empty filter matches everything; `secondary` may be empty for item kinds
/// that only have one searchable field.
/// The length of a listing, while some of it is still missing.
///
/// `None` once `loaded` has caught up, so nothing on screen offers a total that
/// only repeats the row count beside it.
fn rows_missing(loaded: usize, total: u32) -> Option<u32> {
    let known = usize::try_from(total).ok()?;
    (loaded < known).then_some(total)
}

fn row_matches(primary: &str, secondary: &str, filter_lower: &str) -> bool {
    filter_lower.is_empty()
        || primary.to_lowercase().contains(filter_lower)
        || (!secondary.is_empty() && secondary.to_lowercase().contains(filter_lower))
}

#[cfg(test)]
mod tests {
    use super::*;
    use priel_core::{PlayableSource, ResolvedStream};
    use priel_player::hw::HwParams;

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

    // ---- paging the favorites list ----

    /// A page of favorites as the worker would deliver it: rows `ids`, starting
    /// at `offset`, out of a listing `total` long.
    fn track_page(ids: std::ops::Range<u64>, total: u32) -> priel_core::Page<Track> {
        priel_core::Page {
            items: ids.map(|i| track(i, "T", "A")).collect(),
            total,
        }
    }

    fn favorites_page(offset: u32, ids: std::ops::Range<u64>, total: u32) -> FromWorker {
        FromWorker::Favorites {
            offset,
            page: track_page(ids, total),
        }
    }

    fn playlists_page(offset: u32, uuids: &[&str], total: u32) -> FromWorker {
        FromWorker::Playlists {
            offset,
            page: priel_core::Page {
                items: uuids.iter().map(|u| playlist(u, u)).collect(),
                total,
            },
        }
    }

    fn playlist_tracks_page(
        uuid: &str,
        offset: u32,
        ids: std::ops::Range<u64>,
        total: u32,
    ) -> FromWorker {
        FromWorker::PlaylistTracks {
            uuid: uuid.into(),
            offset,
            page: track_page(ids, total),
        }
    }

    fn search_page(query: &str, offset: u32, ids: std::ops::Range<u64>, total: u32) -> FromWorker {
        FromWorker::SearchResults {
            query: query.into(),
            offset,
            page: track_page(ids, total),
        }
    }

    fn ids(tracks: &[Track]) -> Vec<u64> {
        tracks.iter().map(|t| t.id).collect()
    }

    /// Put the list in the state the paging trigger looks for: drawn, with the
    /// selection on the last loaded row. Existing tests set `list_inner` by hand
    /// for the same reason - the renderer is what normally writes it.
    fn scrolled_to_the_end(app: &mut App) {
        app.list_inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        app.selected = app.visible_len().saturating_sub(1);
    }

    #[test]
    fn startup_asks_for_the_first_page_and_says_it_is_loading() {
        // Goal: the first frame must not look like an empty library, and the
        // request has to name the page it wants or nothing can be paged.
        let mut r = rig();
        r.app.start();
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadFavorites { offset: 0, limit }] if limit > 0
        ));
        assert!(r.app.loading);
    }

    #[test]
    fn arriving_favorites_replace_the_list_and_clear_loading() {
        // Goal: the worker reply is the only thing that ends the loading state.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 1..2, 1)).unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.favorites.len(), 1);
        assert!(!r.app.loading);
        assert!(r.app.notice.as_deref().unwrap_or_default().contains('1'));
    }

    #[test]
    fn scrolling_near_the_end_asks_for_the_next_page_and_appends_it() {
        // Goal: the whole point. The list has to grow past its first page as the
        // user reaches the bottom of it, and grow at the end - the only place
        // that leaves the rows above where they were.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..3, 6)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.favorites), vec![0, 1, 2]);
        let _ = requests(&r);

        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        assert!(
            matches!(
                requests(&r)[..],
                [ToWorker::LoadFavorites { offset: 3, .. }]
            ),
            "the next page starts where the loaded rows end"
        );

        r.to_app.send(favorites_page(3, 3..6, 6)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.favorites), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_page_arriving_leaves_the_selected_row_alone() {
        // Goal: selection is an index into the filtered rows. Anything that
        // reorders or replaces moves the row out from under the cursor, which is
        // the bug this design exists to avoid.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..10, 20)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        let (row, under_cursor) = (r.app.selected, r.app.favorites[r.app.selected].id);
        r.app.refresh();

        r.to_app.send(favorites_page(10, 10..20, 20)).unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.selected, row, "the cursor must not jump");
        assert_eq!(
            r.app.favorites[r.app.selected].id, under_cursor,
            "and the same track must still be under it"
        );
    }

    #[test]
    fn only_one_page_request_is_in_flight_at_a_time() {
        // Goal: the trigger runs on every tick, and the tick runs ten times a
        // second. Without the in-flight marker one scroll to the bottom would
        // ask for the same page over and over.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        scrolled_to_the_end(&mut r.app);
        for _ in 0..5 {
            r.app.refresh();
        }
        assert_eq!(
            requests(&r).len(),
            1,
            "the tick repeats; the request must not"
        );
    }

    #[test]
    fn a_page_nobody_is_waiting_for_is_discarded() {
        // Goal: a page that arrives after the list moved on belongs to a listing
        // that no longer exists. Appending it would interleave two lists, which
        // is exactly what matching on arrival order gets you.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();

        // Nothing asked for offset 5, so this page is nobody's.
        r.to_app.send(favorites_page(5, 5..10, 500)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.favorites), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_page_from_before_a_reload_is_discarded() {
        // Goal: the same rule under the case that actually produces a stale
        // reply - a second page still in flight when the list restarts.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        r.app.refresh(); // asks for offset 5
        let _ = requests(&r);

        r.app.start(); // the list restarts; offset 5 is no longer wanted
        r.to_app.send(favorites_page(5, 5..10, 500)).unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.favorites),
            vec![0, 1, 2, 3, 4],
            "the page for the abandoned listing must not be appended"
        );
    }

    #[test]
    fn reaching_the_total_the_service_reports_stops_the_requests() {
        // Goal: the end of the list comes from the service's own count. A page
        // shorter than the limit is not the end, and a full one is not proof
        // that there is more.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..4, 4)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        scrolled_to_the_end(&mut r.app);
        for _ in 0..3 {
            r.app.refresh();
        }
        assert!(requests(&r).is_empty(), "everything is already loaded");
        assert_eq!(
            r.app.rows_available(),
            None,
            "and the heading should stop mentioning a total"
        );
    }

    #[test]
    fn no_page_is_fetched_before_the_first_frame_is_drawn() {
        // Goal: the list geometry is written by the renderer, so before a draw
        // it is a zero-height rect that the selection is trivially at the end
        // of. Acting on that would page the whole library in at startup. One
        // row out of many, so only the geometry guard can hold the request
        // back - the distance to the bottom is zero.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..1, 500)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        assert_eq!(r.app.list_inner.height, 0, "nothing has been rendered yet");
        r.app.refresh();
        assert!(requests(&r).is_empty());
    }

    #[test]
    fn a_failed_page_keeps_the_rows_reports_itself_and_does_not_spin() {
        // Goal: a page that never arrived is a page missing from the end, not a
        // reason to empty the list - and the trigger runs every tick, so a dead
        // link must not turn into a request storm.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        assert_eq!(requests(&r).len(), 1);

        r.to_app
            .send(FromWorker::Failed {
                task: Task::Favorites { offset: 5 },
                fault: Fault::Unreachable,
                detail: "favorites: no route to host".into(),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.favorites),
            vec![0, 1, 2, 3, 4],
            "rows stay usable"
        );
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("no route to host"), "{notice}");

        for _ in 0..5 {
            r.app.refresh();
        }
        assert!(
            requests(&r).is_empty(),
            "a failed page must not be retried on a timer"
        );
    }

    #[test]
    fn coming_back_to_the_list_retries_a_page_that_failed() {
        // Goal: the stall latch stops a spin, not the user. Returning to the
        // view is a deliberate action and is worth one more attempt.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        r.to_app
            .send(FromWorker::Failed {
                task: Task::Favorites { offset: 5 },
                fault: Fault::Unreachable,
                detail: "favorites: no route to host".into(),
            })
            .unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        r.app.on_key(key('2')); // away to the playlists
        r.app.on_key(key('1')); // and back
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        assert!(
            requests(&r)
                .iter()
                .any(|c| matches!(c, ToWorker::LoadFavorites { offset: 5, .. })),
            "the view should be willing to ask again"
        );
    }

    #[test]
    fn a_failure_elsewhere_does_not_free_the_favorites_slot() {
        // Goal: `Failed` says which request died precisely so one view's failure
        // cannot be mistaken for another's. A resolve that failed must not let a
        // second copy of the same page go out.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::Failed {
                task: Task::Resolve,
                fault: Fault::Refused,
                detail: "resolve: no".into(),
            })
            .unwrap();
        r.app.drain_worker();
        r.app.refresh();
        assert!(
            requests(&r).is_empty(),
            "the favorites page is still in flight"
        );
    }

    // ---- paging: the other three views ----

    /// Put the playlists view on screen with one page of `uuids` loaded and the
    /// selection on the last of them.
    fn playlists_loaded(r: &mut Rig, uuids: &[&str], total: u32) {
        r.app.on_key(key('2'));
        r.to_app.send(playlists_page(0, uuids, total)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        let _ = requests(r);
    }

    /// Open a playlist of `num_tracks` tracks with one page of them loaded, and
    /// the selection on the last loaded row.
    fn playlist_open(r: &mut Rig, uuid: &str, num_tracks: u32, loaded: std::ops::Range<u64>) {
        r.app.playlists = vec![Playlist {
            uuid: uuid.into(),
            title: uuid.into(),
            num_tracks,
            duration_secs: 0,
        }];
        r.app.view = View::Playlists;
        r.app.selected = 0;
        r.app.on_key(code(KeyCode::Enter));
        let end = u32::try_from(loaded.end).unwrap_or(0);
        r.to_app
            .send(playlist_tracks_page(uuid, 0, loaded, end))
            .unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        let _ = requests(r);
    }

    /// Run a search and load its first page, with the selection at the end.
    fn searched(r: &mut Rig, query: &str, ids: std::ops::Range<u64>, total: u32) {
        r.app.on_key(key('3'));
        for c in query.chars() {
            r.app.on_key(key(c));
        }
        r.app.on_key(code(KeyCode::Enter));
        r.to_app.send(search_page(query, 0, ids, total)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        let _ = requests(r);
    }

    fn uuids(lists: &[Playlist]) -> Vec<String> {
        lists.iter().map(|p| p.uuid.clone()).collect()
    }

    #[test]
    fn scrolling_the_playlists_to_the_end_asks_for_the_next_page_and_appends_it() {
        // Goal: the playlists list stopped at whatever the first request
        // returned. It has to grow at its end, the only place that leaves the
        // rows above where the user left them.
        let mut r = rig();
        playlists_loaded(&mut r, &["a", "b"], 4);

        r.app.refresh();
        assert!(
            matches!(
                requests(&r)[..],
                [ToWorker::LoadPlaylists { offset: 2, limit }] if limit > 0
            ),
            "the next page starts where the loaded rows end"
        );

        r.to_app.send(playlists_page(2, &["c", "d"], 4)).unwrap();
        r.app.drain_worker();
        assert_eq!(uuids(&r.app.playlists), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn the_playlists_stop_at_the_count_the_service_reports() {
        // Goal: the end of the listing comes from the service's own count, and
        // once it is reached nothing more may be asked for on any tick.
        let mut r = rig();
        playlists_loaded(&mut r, &["a", "b"], 2);
        for _ in 0..3 {
            r.app.refresh();
        }
        assert!(requests(&r).is_empty(), "everything is already loaded");
    }

    #[test]
    fn a_playlists_page_nobody_is_waiting_for_is_discarded() {
        // Goal: a page that arrives after the listing moved on belongs to a
        // listing that no longer exists; appending it would interleave two.
        let mut r = rig();
        playlists_loaded(&mut r, &["a", "b"], 9);

        r.to_app.send(playlists_page(5, &["z"], 9)).unwrap();
        r.app.drain_worker();
        assert_eq!(uuids(&r.app.playlists), vec!["a", "b"]);
    }

    #[test]
    fn a_playlist_knows_its_length_before_its_first_page_of_tracks_arrives() {
        // Goal: the playlist listing already says how many tracks a playlist
        // holds, so this view does not have to wait for - or trust - a total in
        // the tracks response to know it has more to fetch.
        let mut r = rig();
        playlist_open(&mut r, "mix", 5, 0..3);
        assert_eq!(
            r.app.rows_available(),
            Some(5),
            "the count came from the playlist row, not from the page"
        );

        r.app.refresh();
        assert!(
            matches!(
                requests(&r)[..],
                [ToWorker::LoadPlaylistTracks { uuid: ref u, offset: 3, .. }] if u == "mix"
            ),
            "the next page starts where the loaded rows end"
        );

        // The page that answers carries no count of its own, which must not
        // undo what the playlist row already said.
        r.to_app
            .send(playlist_tracks_page("mix", 3, 3..5, 0))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.playlist_tracks), vec![0, 1, 2, 3, 4]);

        for _ in 0..3 {
            r.app.refresh();
        }
        assert!(requests(&r).is_empty(), "the playlist is complete");
    }

    #[test]
    fn a_superseded_playlist_tracks_page_is_dropped_whichever_way_it_is_stale() {
        // Goal: two things make one of these replies wrong, and both have to be
        // checked. A page for a playlist the user has left, and a page of the
        // open playlist that was superseded before it arrived, are equally not
        // the rows on screen.
        let mut r = rig();
        playlist_open(&mut r, "mix", 9, 0..3);
        r.app.refresh(); // asks "mix" for offset 3

        r.to_app
            .send(playlist_tracks_page("other", 3, 90..93, 9))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.playlist_tracks),
            vec![0, 1, 2],
            "a page for a playlist that is not open must be dropped"
        );

        r.to_app
            .send(playlist_tracks_page("mix", 6, 60..63, 9))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.playlist_tracks),
            vec![0, 1, 2],
            "and so must a page of the open playlist nobody asked for"
        );
    }

    #[test]
    fn a_search_appends_a_page_but_replaces_a_new_query() {
        // Goal: the reply used to send the cursor to the top every time, which
        // is right for a fresh set of results and wrong for rows added to the
        // end of the ones being read.
        let mut r = rig();
        searched(&mut r, "blue", 0..3, 6);
        let row = r.app.selected;
        assert!(
            row > 0,
            "the test needs a cursor that is not already at the top"
        );

        r.app.refresh();
        assert!(
            matches!(
                requests(&r)[..],
                [ToWorker::Search { query: ref q, offset: 3, .. }] if q == "blue"
            ),
            "the next page starts where the loaded rows end"
        );

        r.to_app.send(search_page("blue", 3, 3..6, 6)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.search_tracks), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(
            r.app.selected, row,
            "an appended page must not move the cursor"
        );

        // A fresh query is the other case: it replaces, and starts at the top.
        r.app.on_key(key('i')); // re-edit, so the box still holds "blue"
        for c in "s".chars() {
            r.app.on_key(key(c));
        }
        r.app.on_key(code(KeyCode::Enter));
        r.to_app.send(search_page("blues", 0, 9..12, 3)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.search_tracks), vec![9, 10, 11]);
        assert_eq!(r.app.selected, 0, "a new set of results starts at the top");
    }

    #[test]
    fn a_page_for_a_query_that_has_been_replaced_is_dropped() {
        // Goal: the offset alone cannot tell these apart - both are page zero -
        // so the query is part of the reply's identity. Without it the results
        // for an abandoned query overwrite the ones the user is waiting for.
        let mut r = rig();
        searched(&mut r, "blue", 0..3, 6);

        r.app.on_key(key('i'));
        for c in "s".chars() {
            r.app.on_key(key(c));
        }
        r.app.on_key(code(KeyCode::Enter)); // now waiting on "blues" page zero
        let _ = requests(&r);

        r.to_app.send(search_page("blue", 0, 70..73, 6)).unwrap();
        r.app.drain_worker();
        assert!(
            r.app.search_tracks.is_empty(),
            "the abandoned query's page must not land in the new results"
        );

        r.to_app.send(search_page("blues", 0, 40..42, 2)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.search_tracks), vec![40, 41]);
    }

    fn unreachable(task: Task) -> FromWorker {
        FromWorker::Failed {
            task,
            fault: Fault::Unreachable,
            detail: "no route to host".into(),
        }
    }

    #[test]
    fn a_failure_for_a_listing_the_user_has_left_touches_nothing() {
        // Goal: a failure carries the listing as well as the page precisely so
        // it cannot be mistaken for another listing's. Taken for the open
        // playlist's, it would free a slot that is still in flight - and the
        // page that then arrives would be dropped as unwanted, leaving the view
        // short of rows with nothing left to ask.
        let mut r = rig();
        playlist_open(&mut r, "mix", 9, 0..3);
        r.app.refresh(); // asks "mix" for offset 3
        let _ = requests(&r);

        r.to_app
            .send(unreachable(Task::PlaylistTracks {
                uuid: "other".into(),
                offset: 3,
            }))
            .unwrap();
        r.app.drain_worker();

        r.to_app
            .send(playlist_tracks_page("mix", 3, 3..6, 9))
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.playlist_tracks),
            vec![0, 1, 2, 3, 4, 5],
            "the page the view was waiting for must still be accepted"
        );
    }

    #[test]
    fn a_failure_for_a_query_that_has_been_replaced_touches_nothing() {
        // Goal: the same rule on the search view, where the listing is the
        // query. Both requests are for page zero, so only the query tells them
        // apart.
        let mut r = rig();
        searched(&mut r, "blue", 0..3, 6);
        r.app.on_key(key('i'));
        r.app.on_key(key('s'));
        r.app.on_key(code(KeyCode::Enter)); // now waiting on "blues" page zero
        let _ = requests(&r);

        r.to_app
            .send(unreachable(Task::Search {
                query: "blue".into(),
                offset: 0,
            }))
            .unwrap();
        r.app.drain_worker();

        r.to_app.send(search_page("blues", 0, 40..42, 2)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.search_tracks), vec![40, 41]);
    }

    #[test]
    fn a_failed_page_keeps_the_rows_and_does_not_spin_on_any_view() {
        // Goal: the anti-spin latch is per listing. A page that never arrived is
        // a page missing from the end, not a reason to empty the list, and the
        // trigger runs on every tick - so a dead link must not turn into a
        // request storm on whichever view the user is looking at.
        let mut r = rig();
        playlist_open(&mut r, "mix", 9, 0..3);
        r.app.refresh(); // asks "mix" for offset 3
        assert_eq!(requests(&r).len(), 1);

        r.to_app
            .send(unreachable(Task::PlaylistTracks {
                uuid: "mix".into(),
                offset: 3,
            }))
            .unwrap();
        r.app.drain_worker();
        for _ in 0..5 {
            r.app.refresh();
        }
        assert!(
            requests(&r).is_empty(),
            "a failed page must not be retried on a timer"
        );
        assert_eq!(
            ids(&r.app.playlist_tracks),
            vec![0, 1, 2],
            "rows stay usable"
        );

        // And the latch belongs to that listing alone: another view is free.
        let mut r = rig();
        playlists_loaded(&mut r, &["a", "b"], 9);
        r.app.refresh();
        let _ = requests(&r);
        r.to_app
            .send(unreachable(Task::Favorites { offset: 0 }))
            .unwrap();
        r.app.drain_worker();
        r.to_app.send(playlists_page(2, &["c"], 9)).unwrap();
        r.app.drain_worker();
        assert_eq!(
            uuids(&r.app.playlists),
            vec!["a", "b", "c"],
            "a favorites failure must not stall the playlists"
        );
    }

    #[test]
    fn a_listing_that_runs_out_early_stops_asking() {
        // Goal: the count is the service's, and it is sometimes larger than the
        // rows it will actually hand over. An empty page is the end of the
        // listing whatever the count says - without that the same empty page is
        // requested on every tick, forever.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..3, 500)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        let _ = requests(&r);

        r.to_app.send(favorites_page(3, 3..3, 500)).unwrap();
        r.app.drain_worker();
        for _ in 0..5 {
            r.app.refresh();
        }
        assert!(requests(&r).is_empty(), "the listing has run out");
    }

    #[test]
    fn reload_asks_whichever_list_is_on_screen_for_its_first_page_again() {
        // Goal: one binding, four listings. Reload has to act on the list the
        // user is looking at, and start it from the top - a listing reloaded
        // from halfway is a listing with a hole in it.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..3, 9)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);
        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadFavorites { offset: 0, .. }]
        ));

        let mut r = rig();
        playlists_loaded(&mut r, &["a", "b"], 9);
        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadPlaylists { offset: 0, .. }]
        ));

        let mut r = rig();
        playlist_open(&mut r, "mix", 9, 0..3);
        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadPlaylistTracks { uuid: ref u, offset: 0, .. }] if u == "mix"
        ));
        assert_eq!(
            r.app.rows_available(),
            Some(9),
            "and the playlist's own track count survives the reload"
        );

        let mut r = rig();
        searched(&mut r, "blue", 0..3, 9);
        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::Search { query: ref q, offset: 0, .. }] if q == "blue"
        ));
    }

    #[test]
    fn reload_is_how_a_page_that_failed_is_retried_without_leaving_the_view() {
        // Goal: the gap this closes. The anti-spin latch used to be clearable
        // only by switching away and back, which is not something a user has any
        // reason to think of.
        let mut r = rig();
        playlist_open(&mut r, "mix", 9, 0..3);
        r.app.refresh();
        let _ = requests(&r);
        r.to_app
            .send(unreachable(Task::PlaylistTracks {
                uuid: "mix".into(),
                offset: 3,
            }))
            .unwrap();
        r.app.drain_worker();
        r.app.refresh();
        assert!(requests(&r).is_empty(), "latched, as it should be");

        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadPlaylistTracks { offset: 0, .. }]
        ));
        r.to_app
            .send(playlist_tracks_page("mix", 0, 0..3, 9))
            .unwrap();
        r.app.drain_worker();
        r.app.refresh();
        assert!(
            !requests(&r).is_empty(),
            "and the view is willing to page again"
        );
    }

    #[test]
    fn reload_answers_to_a_click_and_to_its_key_alike() {
        // Goal: parity runs both ways - every action reachable by mouse and by
        // keyboard - and the only way to keep the two from drifting is for both
        // to run the same method.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..3, 9)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        r.app.dispatch(Hit::Reload);
        let by_click = requests(&r);
        r.app.on_key(key('r'));
        let by_key = requests(&r);
        assert!(matches!(
            by_click[..],
            [ToWorker::LoadFavorites { offset: 0, .. }]
        ));
        assert_eq!(by_click.len(), by_key.len());
    }

    #[test]
    fn reload_with_nothing_to_reload_costs_nothing() {
        // Goal: the negative space. A search view that has never run a query,
        // and a playlist view with no playlist open, have no first page to ask
        // for again.
        let mut r = rig();
        r.app.on_key(key('3'));
        r.app.set_mode_for_test(Mode::Normal);
        let _ = requests(&r);
        r.app.on_key(key('r'));
        assert!(requests(&r).is_empty());

        r.app.view = View::PlaylistTracks;
        r.app.on_key(key('r'));
        assert!(requests(&r).is_empty());
    }

    #[test]
    fn the_queue_does_not_grow_when_a_page_lands() {
        // Goal: the documented decision in `start_queue_at`. The listener chose
        // a set of tracks; a page arriving must not silently change what plays
        // after the last one they saw.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 0..5, 500)).unwrap();
        r.app.drain_worker();
        r.app.selected = 0;
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.queue.len(), 5);

        scrolled_to_the_end(&mut r.app);
        r.app.refresh();
        r.to_app.send(favorites_page(5, 5..10, 500)).unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.queue.len(), 5, "the queue is a snapshot, not a view");
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
                task: Task::Favorites { offset: 0 },
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
                task: Task::Favorites { offset: 0 },
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
                task: Task::Startup,
                fault: Fault::Refused,
                detail: "token: expired".into(),
            })
            .unwrap();
        r.app.drain_worker();
        assert!(r.app.notice.as_deref().unwrap().contains("expired"));
        assert!(!r.app.loading);
    }

    /// A two-node chain: priel's stream straight into a DAC, no resample.
    fn chain() -> AudioGraph {
        AudioGraph {
            path: vec![
                GraphNode {
                    id: 58,
                    name: "mpv".into(),
                    description: "mpv".into(),
                    media_class: "Stream/Output/Audio".into(),
                    role: NodeRole::Stream,
                    rate_hz: Some(44_100),
                    format: Some("S16LE".into()),
                    channels: Some(2),
                },
                GraphNode {
                    id: 48,
                    name: "alsa_output.usb-DAC".into(),
                    description: "Studio DAC".into(),
                    media_class: "Audio/Sink".into(),
                    role: NodeRole::Device,
                    rate_hz: Some(44_100),
                    format: Some("S32LE".into()),
                    channels: Some(2),
                },
            ],
            ..AudioGraph::default()
        }
    }

    #[test]
    fn the_graph_key_opens_the_overlay_and_asks_the_worker_to_read_it() {
        // Goal: reading the graph runs a subprocess, so the request must leave
        // the UI thread. The overlay opens on the key press and says it is
        // reading, rather than the key press waiting for pw-dump.
        let mut r = rig();
        r.app.on_key(key('D'));
        assert_eq!(r.app.mode, Mode::Graph);
        assert!(
            matches!(r.from_app.try_recv(), Ok(ToWorker::ReadAudioGraph)),
            "the read has to go to the worker"
        );
        let rows = r.app.graph_rows();
        assert_eq!(rows.len(), 1, "nothing to show until the reply arrives");
        assert_eq!(rows[0].kind, GraphRowKind::Note);
    }

    #[test]
    fn the_graph_overlay_opens_on_its_key_and_closes_again() {
        // Goal: dismissed the way the help and log overlays are, so there is
        // one way out of an overlay rather than three.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.app.on_key(key('D'));
        assert_eq!(r.app.mode, Mode::Normal);
        r.app.on_key(key('D'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal, "Esc closes it too");
        r.app.on_key(key('D'));
        r.app.on_key(key('q'));
        assert_eq!(r.app.mode, Mode::Normal, "q closes it rather than quitting");
        assert!(!r.app.should_quit);
    }

    #[test]
    fn the_graph_overlay_swallows_the_keys_and_clicks_behind_it() {
        // Goal: modal like the other overlays. A view change or a track
        // starting underneath one is how a user ends up somewhere they did not
        // ask to be.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.on_key(key('D'));
        r.app.on_key(key('2'));
        assert_eq!(r.app.view, View::Favorites, "the view must not change");
        assert_eq!(r.app.mode, Mode::Graph, "and it is still open");
        r.app.on_mouse(click(2, 2));
        assert_eq!(r.app.mode, Mode::Normal, "a click dismisses it");
        assert_eq!(r.app.selected, 0, "and does not land on a row underneath");
    }

    #[test]
    fn a_direct_output_says_there_is_no_graph_rather_than_no_stream() {
        // Goal: on the direct path priel is not a client of the sound server at
        // all, so asking pw-dump gets "priel has no stream in the graph" - which
        // reads as "nothing is playing yet" and is the opposite of the truth.
        // The player knows which device it holds, so the overlay is told.
        let mut r = rig();
        r.app.status.audio_device = "alsa/hw:CARD=AUDIO,DEV=0".into();
        r.app.on_key(key('D'));

        let rows = r.app.graph_rows();
        let text = rows
            .iter()
            .map(|row| row.label.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("no graph"),
            "it has to say what is missing: {text}"
        );
        assert!(
            !text.contains("no stream"),
            "and not the sentence that means nothing is playing: {text}"
        );
        assert!(
            r.from_app.try_recv().is_err(),
            "and there is nothing to ask pw-dump, so it is not run"
        );
    }

    #[test]
    fn the_chain_the_worker_read_is_what_the_overlay_lists() {
        // Goal: the whole point - every node between priel and the device, in
        // order, with what each one negotiated.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain())))
            .expect("send");
        r.app.drain_worker();
        let rows = r.app.graph_rows();
        let nodes: Vec<&GraphRow> = rows
            .iter()
            .filter(|r| r.kind == GraphRowKind::Node)
            .collect();
        assert_eq!(nodes.len(), 2);
        assert!(nodes[0].label.contains("priel"), "{}", nodes[0].label);
        assert_eq!(nodes[0].detail, "44.1 kHz  S16LE  2 ch");
        assert!(nodes[1].label.contains("Studio DAC"), "{}", nodes[1].label);
        assert_eq!(nodes[1].detail, "44.1 kHz  S32LE  2 ch");
        assert!(
            rows.iter().any(|r| r.kind == GraphRowKind::Link),
            "the two nodes are drawn as a chain, not a list"
        );
    }

    #[test]
    fn a_graph_that_could_not_be_read_explains_itself_rather_than_showing_a_box() {
        // Goal: the machine with no PipeWire tools is the common case, and an
        // empty overlay there reads as a bug in priel.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Err(GraphError::NotInstalled)))
            .expect("send");
        r.app.drain_worker();
        let rows = r.app.graph_rows();
        assert!(rows.iter().all(|r| r.kind == GraphRowKind::Note));
        assert!(rows[0].label.contains("pw-dump"), "{}", rows[0].label);
        assert_eq!(rows.len(), 2, "the sentence and what to do about it");
    }

    #[test]
    fn a_node_with_no_negotiated_format_says_so_rather_than_showing_nothing() {
        // Goal: a suspended device has no format yet. A blank right-hand column
        // reads as "bit-perfect" to the eye scanning it.
        let mut r = rig();
        let mut g = chain();
        g.path[1].rate_hz = None;
        g.path[1].format = None;
        r.app.on_key(key('D'));
        r.to_app.send(FromWorker::AudioGraph(Ok(g))).expect("send");
        r.app.drain_worker();
        let rows = r.app.graph_rows();
        let last = rows.last().expect("a row");
        assert_eq!(last.detail, "no format yet");
    }

    /// A three-node chain with a loopback wedged in that moves the rate to
    /// 48 kHz and a device that narrows to 16 bits behind it.
    fn altered_chain() -> AudioGraph {
        let mut g = chain();
        g.path[0].format = Some("S32LE".into());
        g.path.insert(
            1,
            GraphNode {
                id: 71,
                name: "studio-loopback".into(),
                description: "Studio loopback".into(),
                media_class: "Audio/Sink".into(),
                role: NodeRole::Intermediate,
                rate_hz: Some(48_000),
                format: Some("F32LE".into()),
                channels: Some(2),
            },
        );
        g.path[2].rate_hz = Some(48_000);
        g
    }

    /// An app playing a 44.1 kHz 24-bit track, with the graph overlay open.
    fn playing_hires(graph: AudioGraph) -> Rig {
        let mut r = rig();
        r.app.status.loaded = true;
        r.app.status.playing = true;
        r.app.status.volume = 100.0;
        r.app.status.in_sample_rate = 44_100;
        r.app.status.in_format = "s32".into();
        r.app.status.sample_rate = 44_100;
        r.app.status.out_format = "s32".into();
        r.app.now_meta.bit_depth = 24;
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(graph)))
            .expect("send");
        r.app.drain_worker();
        r
    }

    fn overlay_text(app: &App) -> String {
        app.graph_rows()
            .iter()
            .map(|row| row.label.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_node_that_changed_the_rate_is_marked_and_named() {
        // Goal: the badge already says the samples were resampled. This is the
        // half it cannot answer - which of the nodes did it - and the answer is
        // useless unless the reader can see which row is accused, so the row is
        // marked and the sentence names it.
        let r = playing_hires(altered_chain());
        let rows = r.app.graph_rows();
        let marked: Vec<&GraphRow> = rows
            .iter()
            .filter(|row| row.kind == GraphRowKind::Culprit)
            .collect();
        assert_eq!(marked.len(), 2, "the row and the sentence about it");
        assert!(
            marked[0].label.contains("Studio loopback"),
            "the loopback moved the rate, not the DAC behind it: {}",
            marked[0].label
        );
        assert!(
            marked[1].label.contains("Studio loopback"),
            "{}",
            marked[1].label
        );
        assert!(
            marked[1].label.contains("resampling"),
            "{}",
            marked[1].label
        );
    }

    #[test]
    fn a_narrowing_node_is_named_the_same_way_a_resampling_one_is() {
        // Goal: a sink that takes 24-bit content as S16LE throws eight bits
        // away, which is the other half of the same question and gets the same
        // treatment rather than a second idiom.
        let mut g = chain();
        g.path[0].format = Some("S32LE".into());
        g.path[1].format = Some("S16LE".into());
        let r = playing_hires(g);
        let text = overlay_text(&r.app);
        assert!(text.contains("Studio DAC is truncating"), "{text}");
    }

    #[test]
    fn a_clean_chain_accuses_nobody() {
        // Goal: the overlay is read most often when nothing is wrong, and a
        // marker on a chain that is fine would teach the reader to ignore it.
        let mut g = chain();
        g.path[0].format = Some("S32LE".into());
        let r = playing_hires(g);
        assert!(
            r.app
                .graph_rows()
                .iter()
                .all(|row| row.kind != GraphRowKind::Culprit),
            "{}",
            overlay_text(&r.app)
        );
    }

    #[test]
    fn a_chain_that_explains_nothing_admits_it_rather_than_blaming_the_nearest_node() {
        // Goal: the device is clocked at 48 kHz and every node on the path says
        // 44.1 kHz - a resample the server did inside a node rather than
        // between two of them. The DAC is the nearest candidate and naming it
        // would send the reader to change a setting that was never wrong.
        let mut g = chain();
        g.path[0].format = Some("S32LE".into());
        let mut r = playing_hires(g);
        r.app.status.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        let text = overlay_text(&r.app);
        assert!(
            r.app
                .graph_rows()
                .iter()
                .all(|row| row.kind != GraphRowKind::Culprit),
            "{text}"
        );
        assert!(text.contains("nothing on this path"), "{text}");
        assert!(
            !text.contains("Studio DAC is"),
            "the nearest candidate must not be blamed: {text}"
        );
    }

    #[test]
    fn an_idle_overlay_says_nothing_about_a_track_there_is_none_of() {
        // Goal: with nothing playing there is no track format to compare
        // against, and a sentence about it on every idle reading would be noise
        // over a chain the reader can already see has settled on nothing.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain())))
            .expect("send");
        r.app.drain_worker();
        assert_eq!(
            r.app.graph_rows().len(),
            3,
            "two nodes and the connector, and nothing else: {}",
            overlay_text(&r.app)
        );
    }

    #[test]
    fn a_measurement_that_could_not_be_checked_is_admitted_too() {
        // Goal: the chain published no formats at all and the hardware says the
        // rate moved. "Nothing on this path did it" would claim a comparison
        // that never happened, so this case gets its own sentence.
        let mut g = chain();
        for node in &mut g.path {
            node.rate_hz = None;
            node.format = None;
        }
        let mut r = playing_hires(g);
        r.app.status.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        let text = overlay_text(&r.app);
        assert!(text.contains("Not enough"), "{text}");
        assert!(
            !text.contains("nothing on this path"),
            "nothing was compared, so nothing was ruled out: {text}"
        );
    }

    #[test]
    fn every_sentence_the_overlay_adds_fits_the_box_it_is_drawn_in() {
        // Goal: the overlay draws one row per line and does not rewrap, so a
        // sentence longer than the box loses its tail - and in an accusation
        // the tail is the verb.
        let mut r = playing_hires(altered_chain());
        for row in r.app.graph_rows() {
            assert!(
                row.label.chars().count() <= 60,
                "too long to draw: {}",
                row.label
            );
        }
        r.app.status.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 96_000,
            format: "S16_LE".into(),
            channels: 2,
        });
        for row in r.app.graph_rows() {
            assert!(
                row.label.chars().count() <= 60,
                "too long to draw: {}",
                row.label
            );
        }
    }

    #[test]
    fn the_graph_overlay_scrolls_with_the_same_keys_as_a_list() {
        // Goal: every other list in priel moves on j/k and g/G, so this one
        // must too - a second scrolling idiom would be its own bug.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain())))
            .expect("send");
        r.app.drain_worker();
        assert_eq!(r.app.graph_offset(), 0, "it opens at the top");
        r.app.on_key(key('j'));
        assert_eq!(r.app.graph_offset(), 1, "j goes down");
        r.app.on_key(key('k'));
        assert_eq!(r.app.graph_offset(), 0);
        r.app.on_key(key('k'));
        assert_eq!(r.app.graph_offset(), 0, "and stops at the top");
        r.app.on_key(key('G'));
        assert_eq!(r.app.graph_offset(), 2, "G reaches the last line");
        r.app.on_key(key('j'));
        assert_eq!(r.app.graph_offset(), 2, "and stops there");
        r.app.on_key(key('g'));
        assert_eq!(r.app.graph_offset(), 0, "g returns to the top");
    }

    #[test]
    fn reopening_the_overlay_reads_the_graph_again_rather_than_reusing_it() {
        // Goal: the chain changes when the device or the track rate does, and a
        // stale reading presented as current is the one thing this overlay must
        // never do.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain())))
            .expect("send");
        r.app.drain_worker();
        while r.from_app.try_recv().is_ok() {}
        r.app.on_key(key('D'));
        r.app.on_key(key('D'));
        assert!(
            matches!(r.from_app.try_recv(), Ok(ToWorker::ReadAudioGraph)),
            "opening it again asks again"
        );
        assert_eq!(
            r.app.graph_rows().len(),
            1,
            "and shows nothing until the new answer lands"
        );
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
        r.app.notice = None;
        r.app.on_key(key('j'));
        r.app.on_key(key('k'));
        r.app.on_key(key('G'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(
            r.app.notice.is_none(),
            "moving and cancelling chooses nothing: {:?}",
            r.app.notice
        );
    }

    #[test]
    fn the_picker_toggles_exclusive_access_and_leaves_the_device_alone() {
        // Goal: exclusivity is orthogonal to which device is chosen, so the
        // toggle must not move the output, and it must not close the picker
        // either - it is a setting to see the effect of, not a choice to make
        // and leave on.
        let mut r = with_picker("pipewire/dac");
        assert!(!r.app.exclusive(), "priel never asks for this on its own");

        r.app.on_key(key('x'));
        assert!(r.app.exclusive());
        assert_eq!(r.app.mode, Mode::Devices, "the picker stays up");
        assert_eq!(
            r.app.device_selected(),
            1,
            "and the chosen device is untouched"
        );

        r.app.on_key(key('x'));
        assert!(!r.app.exclusive(), "and it is a toggle, not a latch");
    }

    #[test]
    fn the_exclusive_toggle_answers_to_a_click_and_to_its_key_alike() {
        // Goal: parity runs both ways here - every action has a key binding and
        // every action is reachable with the mouse. Both go through the one
        // method, so the two paths cannot drift apart.
        let mut r = with_picker("pipewire/dac");
        let box_ = Rect {
            x: 4,
            y: 18,
            width: 20,
            height: 1,
        };
        r.app.device_exclusive_rect = box_;

        r.app.click_device(box_.x + 1, box_.y);
        assert!(r.app.exclusive(), "a click on the control asks for it");
        assert_eq!(r.app.mode, Mode::Devices, "and does not dismiss the picker");

        r.app.click_device(box_.x + 1, box_.y);
        assert!(!r.app.exclusive(), "clicking again gives it back");
    }

    #[test]
    fn asking_for_exclusive_access_says_it_lasts_for_this_session() {
        // Goal: priel reads no configuration file, so a toggle that said
        // nothing would look broken on the next start. --exclusive is what
        // makes it permanent, exactly as --device is for the device.
        let mut r = with_picker("pipewire/dac");
        r.app.notice = None;
        r.app.on_key(key('x'));
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("--exclusive"),
            "the notice says what makes it permanent: {notice}"
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
    fn choosing_a_device_closes_the_picker_and_says_it_is_for_this_session() {
        // Goal: the choice is not written anywhere - priel reads no
        // configuration file - so the one moment it can be said is now.
        let mut r = with_picker("auto");
        r.app.on_key(key('j'));
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.mode, Mode::Normal, "choosing closes the picker");
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("pipewire/dac description"),
            "the chosen device is named: {notice}"
        );
        assert!(
            notice.contains("this session"),
            "and the choice is temporary: {notice}"
        );
    }

    #[test]
    fn clicking_a_device_row_does_what_the_enter_key_does() {
        // Goal: the mouse is a first-class addition, never a second
        // implementation. Both paths run `choose_device` and nothing else.
        let mut r = with_picker("auto");
        let row = Rect {
            x: 2,
            y: 5,
            width: 60,
            height: 1,
        };
        r.app.device_rows = vec![(row, 2)];
        r.app.on_mouse(click(4, 5));
        assert_eq!(r.app.device_selected(), 2, "the clicked row is taken");
        assert_eq!(r.app.mode, Mode::Normal, "and choosing closes the picker");
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("alsa/hdmi description"), "{notice}");
    }

    #[test]
    fn a_click_outside_the_rows_dismisses_the_picker_without_choosing() {
        // Goal: the log overlay behaves this way and a second idiom would be
        // its own bug. Missing a row must not move the output.
        let mut r = with_picker("auto");
        r.app.notice = None;
        r.app.device_rows = vec![(
            Rect {
                x: 2,
                y: 5,
                width: 60,
                height: 1,
            },
            1,
        )];
        r.app.on_mouse(click(4, 9));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(r.app.notice.is_none(), "nothing was chosen");
    }

    #[test]
    fn a_device_that_will_not_open_is_reported_once_rather_than_every_tick() {
        // Goal: the player carries the reason until the next change is
        // accepted, so without the latch this would replace every other notice
        // on screen ten times a second - and a failure the user has to act on
        // has to survive long enough to be read.
        let mut r = rig();
        r.app.status.device_error = Some("alsa/hdmi would not open".into());
        r.app.refresh_for_test();
        assert_eq!(
            r.app.notice.as_deref(),
            Some("alsa/hdmi would not open"),
            "the failure reaches the user"
        );

        r.app.notice = Some("something else".into());
        r.app.refresh_for_test();
        assert_eq!(
            r.app.notice.as_deref(),
            Some("something else"),
            "and is not repeated over whatever came after it"
        );

        r.app.status.device_error = None;
        r.app.refresh_for_test();
        r.app.status.device_error = Some("alsa/hdmi would not open".into());
        r.app.refresh_for_test();
        assert_eq!(
            r.app.notice.as_deref(),
            Some("alsa/hdmi would not open"),
            "a second failure is a new report"
        );
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
            .send(playlist_tracks_page("stale", 0, 9..10, 1))
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
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadPlaylists { offset: 0, .. }]
        ));

        r.to_app.send(playlists_page(0, &["u"], 1)).unwrap();
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
            [ToWorker::LoadPlaylistTracks { uuid: ref u, offset: 0, .. }] if u == "uuid-1"
        ));

        r.to_app
            .send(playlist_tracks_page("uuid-1", 0, 4..5, 1))
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
            [ToWorker::Search { query: ref q, offset: 0, .. }] if q == "mil"
        ));

        r.to_app.send(search_page("mil", 0, 3..4, 1)).unwrap();
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
            matches!(requests(&r)[..], [ToWorker::LoadFavorites { .. }]),
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
                task: Task::Resolve,
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
