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
//! views (favorites / playlists / playlist tracks / mixes / mix tracks / search)
//! and the gapless play-queue orchestration.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use rand::seq::SliceRandom;
use ratatui::layout::Rect;

use priel_core::auth::{Credentials, Pkce};
use priel_core::{Fault, Mix, Playlist, Track};
use priel_player::Alteration;
use priel_player::graph::{
    Attribution, AudioGraph, ClockRates, DeviceHolder, GraphError, GraphNode, NodeRole, RateAdvice,
    SinkVolume, SourceFormat,
};
use priel_player::{AudioDevice, PlaybackStatus, Player, PlayerConfig, Verdict};

#[cfg(test)]
use std::sync::mpsc::Sender;
use std::sync::mpsc::{Receiver, TryRecvError};

use crate::bus::conn::Bus;
use crate::bus::mpris::{self, BusCommand, Entry, Now, Snapshot};
use crate::cli::ThemeName;
use crate::settings::Settings;
use crate::theme::{self, Theme};
use crate::worker::{self, FromWorker, QueueSource, Task, ToWorker, Worker};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum View {
    Favorites,
    Playlists,
    PlaylistTracks,
    /// The mixes the service builds for the listener.
    ///
    /// A view of its own rather than a section of the playlists, because the
    /// two behave the same way only at the moment of pressing play. A mix
    /// cannot be edited and is rebuilt without anyone asking, and putting the
    /// two in one list would offer the listener a single row type with two sets
    /// of rules.
    Mixes,
    MixTracks,
    Search,
}

/// Which of the two lists the keyboard is driving.
///
/// Two rather than a list of regions, and that is the whole reason the focus
/// key can be a single press: vim's `Ctrl-W` is a *prefix* because vim has any
/// number of windows to name, and priel has exactly two, so the prefix is the
/// move. A third region would have to grow a direction after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// The browse list: whatever [`View`] is showing.
    List,
    /// The play queue, in its column beside the list.
    Queue,
}

/// What the queue does when it reaches the end of a track, or the end of itself.
///
/// Three states rather than two flags, because they are exclusive: a queue
/// cannot both start again and play one track for ever, and two booleans would
/// have a fourth combination that means nothing. Spec 2.2's `LoopStatus` has the
/// same three, which is why one type serves the key, the control and the bus.
///
/// **Session state rather than a setting**, in the sense
/// `docs/adr/0004-a-settings-file-that-is-edited-never-rewritten.md` means it:
/// its membership test is "can a flag already set it", no flag can, and its
/// exclusion list names the shuffle flag - the toggle beside this one - as the
/// kind of thing that stays out. `settings.rs` has the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Repeat {
    /// The queue ends where it ends. The radio is then the only thing that can
    /// follow it, and only if it was asked for.
    #[default]
    Off,
    /// The queue starts again at its top.
    All,
    /// The track playing now plays again.
    One,
}

impl Repeat {
    /// The state after this one, which is what the key and the control both ask
    /// for. Off, then the whole queue, then one track: widest scope first, so
    /// the cycle reads as narrowing rather than as an arbitrary order.
    #[must_use]
    const fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    /// What a *deliberate* skip honours.
    ///
    /// Repeat-one reads as off here, and nothing else changes: the natural end
    /// of a track repeats it, but pressing next says otherwise. A repeat-one
    /// that could not be skipped out of would be a trap with no key to leave it.
    #[must_use]
    const fn skipped(self) -> Self {
        match self {
            Self::One => Self::Off,
            other => other,
        }
    }

    /// The MPRIS spelling. Spec 2.2 allows exactly these three words, and they
    /// map onto the three states one for one - which is what made publishing
    /// `LoopStatus` a translation rather than a design.
    #[must_use]
    pub(crate) const fn loop_status(self) -> &'static str {
        match self {
            Self::Off => "None",
            Self::All => "All",
            Self::One => "Track",
        }
    }

    /// And back, or `None` for a word the specification does not name - which
    /// the consumer is told about rather than being guessed at.
    #[must_use]
    pub(crate) fn from_loop_status(word: &str) -> Option<Self> {
        match word {
            "None" => Some(Self::Off),
            "All" => Some(Self::All),
            "Track" => Some(Self::One),
            _ => None,
        }
    }

    /// The glyph and mark a control paints, which says which of three it is in
    /// **without any colour at all** - the rule the verdict badges follow. The
    /// backing says on or off; only these say which kind of on.
    ///
    /// A gapped circle arrow rather than any of the repeat codepoints: those
    /// have emoji presentation, and an emoji font paints them two cells wide
    /// while unicode-width calls them one, moving every hit box after them off
    /// what was painted.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Off => " ⟳- ",
            Self::All => " ⟳A ",
            Self::One => " ⟳1 ",
        }
    }
}

/// What a left click at one cell means, read off the geometry the renderer
/// published for this frame.
///
/// A separate answer from acting on it, for the same reason [`App::decide`] is
/// separate from carrying a plan out: a player command is fire-and-forget, so a
/// test that clicks the progress bar has nothing to assert on afterwards. Here
/// the *intent* is a value, so a test can render a real frame, name a cell and
/// check the seconds a click there asks for - which is the one assertion that
/// catches `progress_rect` drifting away from the bar that was painted.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Click {
    /// A control registered a hit box over this cell.
    Control(Hit),
    /// The progress bar, at the position in seconds the cell stands for.
    Seek(f64),
    /// A loaded list row, by index into the visible list.
    Row(usize),
    /// A queue entry in the queue's own column, by row of the play order.
    ///
    /// A row rather than an index into a visible list: the queue has no filter
    /// of its own. It is a row of the *order* rather than of the queue, because
    /// that is what the panel draws, and [`App::queue_at`] is the one place
    /// that turns one into the other.
    QueueRow(usize),
    /// Bare surface, or a bar with nothing playing to seek within.
    Nothing,
}

/// A clickable region recorded by the renderer.
///
/// Parity runs both ways: every action has a VIM-style key binding *and* a hit
/// box, so this enum is the list of everything priel can be asked to do. The
/// renderer knows the geometry, and only the renderer should have to.
///
/// Three things deliberately have no variant here, and saying so is the point -
/// an admitted gap beats a silent one:
///
/// - **Typing.** The filter box, the search query and the pasted sign-in address
///   are text, and the keys that edit, accept or cancel them belong to the box
///   being typed in. A button standing for a keystroke aimed at a text field
///   would be a control that could not do what it named.
/// - **Scroll positions.** `g`, `G`, `J`, `K` and the half-page pair name
///   *distances*, not destinations the mouse cannot otherwise reach: the wheel
///   moves the same selection and a click lands on any row directly. They are
///   listed in the reference all the same, and clickable there, so the keyboard
///   idiom is never the only route.
/// - **`x` outside the output picker.** Exclusivity is a property of the picker
///   and the picker draws its own toggle. A control for it anywhere else would
///   do nothing where it was clicked.
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
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Enter,
    /// Leave a drill-down for the list it was opened from.
    ///
    /// One control rather than one per parent list: `Esc` has to land back
    /// where the listener came from, and a hit box naming a fixed destination
    /// would send whoever clicked it in the reference somewhere they had never
    /// been.
    Back,
    Shuffle,
    /// Cycle no repeat, repeat all and repeat one.
    ///
    /// One control for three states rather than three controls: they are
    /// exclusive, so a strip of them would offer two clicks that undo each
    /// other. Between [`Hit::Shuffle`] and [`Hit::Continue`] because the three
    /// are answers to "and then?" in the order they are consulted - the shuffle
    /// picks, the repeat decides whether there is an end, and the radio answers
    /// only where there is one.
    Repeat,
    /// Carry on with the service's radio when the queue runs out, or stop
    /// there. Beside [`Hit::Shuffle`] because both are answers to "and then?".
    Continue,
    VolUp,
    VolDown,
    VolUnity,
    Filter,
    EditSearch,
    Reload,
    /// Add the highlighted track to the favorites, or take it back off.
    FavoriteSelected,
    /// The same, for the track that is playing. Two controls rather than one
    /// because the row under the cursor and the track in the speakers are
    /// routinely different tracks, and the heart beside each one has to mean it.
    FavoriteNowPlaying,
    CycleView,
    /// Hand the keyboard to the other of the two lists.
    ///
    /// Only ever on screen where there are two: the queue's column arrives at
    /// 120 columns, and below that this control does not appear.
    CycleFocus,
    Help,
    Log,
    Graph,
    Devices,
    /// Open the colour theme picker.
    Themes,
    SignIn,
    /// Download a client identity, from the first-run consent screen.
    FetchCredentials,
    /// Carry on without one, from the same screen.
    DeclineCredentials,
    /// Hand the pasted redirect back for a session, from the sign-in screen.
    SubmitLogin,
    /// Open the authorization page again, from the same screen.
    ReopenBrowser,
    /// Empty the paste box, from the same screen.
    ClearPaste,
    /// Abandon the sign-in.
    CancelLogin,
    /// Make a new playlist. Opens the name prompt; it does not create anything
    /// on its own, because a playlist with no name is not worth making.
    NewPlaylist,
    /// Rename the highlighted playlist. Opens the same prompt, primed.
    RenamePlaylist,
    /// Take away whatever is highlighted: the playlist itself in the playlist
    /// list, the track in an open playlist.
    ///
    /// One control rather than two, for the reason [`Hit::Back`] is one: the
    /// destination depends on where the listener is, and a control naming a
    /// fixed one would delete something they were not looking at.
    RemoveSelected,
    /// Put the highlighted track into a playlist. Opens the target picker.
    AddToPlaylist,
    /// Accept what is typed in the name prompt.
    SubmitPrompt,
    /// Abandon the name prompt.
    CancelPrompt,
    /// A key, made clickable where it is printed.
    ///
    /// The overlays print their keys in a footer, and those footers were flat
    /// text while the bottom row's keys were buttons - so clicking `j k scroll`
    /// in the report closed it, which is the opposite of what it says. This is
    /// how they became live without giving each overlay its own vocabulary of
    /// hits: the click **is** the key press, fed to the same handler through the
    /// same door, so the two can never come to mean different things.
    Key(KeyCode),
    /// Go through with the change the confirmation is asking about.
    ///
    /// The only control in priel that destroys something, which is why it is a
    /// variant of its own rather than a reuse of [`Hit::Enter`]: nothing else
    /// may dispatch to it by accident, and it exists on screen only while a
    /// confirmation is up.
    ConfirmYes,
    /// Leave things as they are.
    ConfirmNo,
    /// Show or hide the queue's column beside the list.
    ///
    /// Registered only where there is a column to act on, which is the same
    /// rule `x` follows in the output picker: a control that would do nothing
    /// where it was clicked is not a control.
    QueueColumn,
    /// Hide the album cover, or ask for it back. Drawn only where the cover can
    /// appear - a tall enough terminal - for the reason [`Hit::QueueColumn`] is
    /// drawn only at [`crate::ui::WIDE_COLS`]: below that it would do nothing
    /// where it was clicked.
    CoverArt,
    Quit,
}

/// The change a modal is collecting an answer for.
///
/// One value rather than a flag per action. The screen that asks and the code
/// that acts read the same thing, so they cannot come to disagree about which
/// playlist is being renamed or which track is about to go - and a modal with
/// no pending change cannot act at all.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Pending {
    /// Make a playlist with the name being typed.
    NewPlaylist,
    /// Retitle a playlist. `was` is what to put back if the service says no.
    Rename { uuid: String, was: String },
    /// Delete a playlist. The title is here for the question and the notice:
    /// a confirmation that named a uuid would be a confirmation of nothing.
    DeletePlaylist { uuid: String, title: String },
    /// Take a track out of the playlist that is open.
    RemoveTrack {
        uuid: String,
        track_id: u64,
        title: String,
    },
}

#[derive(PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Filter,      // local filter of the current list
    Search,      // editing the global TIDAL search query
    Help,        // the shortcut reference is up; it swallows input until dismissed
    Log,         // the recent diagnostics are up; modal in the same way
    Graph,       // the chain to the output device is up; modal in the same way
    SetupAudio,  // confirming, then applying, the rates drop-in; modal in the same way
    Devices,     // the output picker is up; modal in the same way
    Themes,      // the colour theme picker is up; modal in the same way
    Credentials, // first run with no client identity; asking before fetching one
    Login,       // signing in: browser is open, waiting for the redirected URL
    Prompt,      // typing a playlist name; modal
    Confirm,     // asking before something that cannot be undone; modal
    AddTo,       // choosing which playlist a track goes into; modal
}

/// The "set up audio" flow, from the confirm to the outcome.
///
/// Held only while the overlay is up. The rates were settled the moment it
/// opened - from the graph that was already read - so nothing here reaches back
/// into the graph: what the preview shows and what the worker writes are the one
/// list, decided once. The step is a small state machine the overlay renders and
/// the keys advance, and the two `Writing`/`Restarting` steps are the ones with
/// a request in flight, where a keystroke does nothing but wait.
pub(crate) struct Setup {
    /// The rates being added - the device's, that the server was blocking. Kept
    /// apart from the whole list only to name them in the sentence; it is what
    /// the offer was about.
    pub(crate) adding_hz: Vec<u32>,
    /// The whole list the drop-in would permit: everything already permitted
    /// plus [`Self::adding_hz`]. This is what is written, because a later drop-in
    /// replaces the property rather than adding to it.
    pub(crate) allowed_hz: Vec<u32>,
    pub(crate) step: SetupStep,
}

/// Where the "set up audio" flow has got to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum SetupStep {
    /// The preview is up, waiting for the listener to say write it or not.
    Confirm,
    /// The write request is out; the overlay says so and waits for the reply.
    Writing,
    /// The file landed at `path`; offering the restart that makes it take effect.
    Restart { path: String },
    /// The restart request is out; the overlay says so and waits for the reply.
    Restarting,
    /// The flow is over, for good or ill; `message` is what it came to.
    Done { message: String },
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

/// How the active Bluetooth codec stands against what the device offers.
///
/// Derived at this layer, from the active codec (in the status) and the
/// available ones (in the last-read graph), rather than threaded through the
/// player: nothing on the player thread needs it, and it changes only when the
/// graph is re-read. `None` from [`App::bt_codec_standing`] means either not a
/// Bluetooth output, or the available codecs could not be read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum BtStanding {
    /// On the best codec the device offers: nothing more to do.
    Best,
    /// A higher-quality codec is available; `better` is its canonical id.
    Improvable { better: String },
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

/// How often the sink's own volume is re-read while something is playing.
///
/// Long, because reading it forks `pw-dump` and the tick that asks runs ten
/// times a second, and short enough that a volume changed in the system mixer
/// shows up in the verdict while the listener is still looking at it.
const SINK_VOLUME_INTERVAL: Duration = Duration::from_secs(5);

/// How long the picker says it is still looking before it says there is
/// nothing to look at.
///
/// The list is asked for as the overlay opens and answered a tick later, so
/// without this the first frame of every open would claim there are no devices.
const DEVICE_WAIT: Duration = Duration::from_secs(2);

/// The most the desktop may ask for in one tick.
///
/// A consumer holding down a media key may not hold the render thread, and what
/// is left waits a tenth of a second for the next tick.
const MAX_BUS_COMMANDS_PER_TICK: usize = 32;

/// Seconds as the microseconds every position in MPRIS is counted in.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a position in microseconds is nowhere near i64's range, and `as` saturates"
)]
fn micros(seconds: f64) -> i64 {
    (seconds.max(0.0) * 1_000_000.0) as i64
}

/// And back, which is the unit the player seeks in.
#[allow(
    clippy::cast_precision_loss,
    reason = "a position in microseconds is far inside the integers f64 counts exactly"
)]
fn seconds(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// The most tracks the **radio** may grow a queue to.
///
/// A queue built from a listing is bounded by the listing, but one the radio
/// keeps extending is bounded by nothing at all: it runs out, asks again, and
/// grows every time. This is the bound that makes *that* finite - about a day of
/// music, after which the radio adds no more and the queue ends, like any other.
/// It is not a limit on a queue you filled from a listing you already hold: see
/// [`QUEUE_MAX`].
const RADIO_MAX: usize = 500;

/// The absolute ceiling on a queue, however it was filled.
///
/// A queue is a handful of small strings per track, drawn a screenful at a time,
/// and mpv only ever holds the current track plus one preloaded next - so a long
/// queue costs almost nothing, and this is a sanity backstop rather than a real
/// limit. It is set far above any real library so that filling a queue from a
/// listing reads as "all of them"; the one operation that is linear in the queue
/// length, `playing_row`, runs once a frame and is trivial at this size. The
/// radio stops well below it, at [`RADIO_MAX`].
const QUEUE_MAX: usize = 10_000;

/// [`QUEUE_MAX`] as a `u32`, for comparing against a listing's row count.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a fixed 10_000 is far inside u32"
)]
const QUEUE_MAX_U32: u32 = QUEUE_MAX as u32;

/// How long a playlist name may be typed.
///
/// A bound because the box grows from keystrokes and nothing else would stop
/// it, and this one rather than a larger one because the listing shows a title
/// in a column: a name longer than this is a name nobody can read back.
const PLAYLIST_NAME_MAX: usize = 100;

/// The visual rows one queue entry occupies: a title and the artist beneath it.
///
/// **The one number the renderer and the click handler must share.** The panel
/// paints an entry as two rows (`queue_column`), so a screen row is not a queue
/// row any more: the offset a click is measured against, the window the cursor
/// scrolls, and how many entries a page turns are all this factor away from the
/// visual height. Keeping it here rather than as a `2` in each place is what
/// stops a click landing on the artist of the row above - the defect the pure
/// `click_at` seam exists to make untestable-by-accident.
pub(crate) const QUEUE_ROWS_PER_ENTRY: usize = 2;

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
    /// The true end of the listing has been reached, so [`Self::total`] is exact
    /// and no later page may raise it.
    ///
    /// The service's advertised count is sometimes an over-count - a favourites
    /// listing that claims 922 but delivers 921, an unavailable row counted but
    /// never sent. The only honest end signal is running out of rows, and two
    /// things see it: this view's own paging reaching an empty page, and the
    /// background queue fill paging the same listing to its end while the view
    /// stayed lazy (see [`App::confirm_listing_total`]). Once either does, the
    /// count is known, and [`Self::absorb`] stops folding in the service's
    /// figure so a later page cannot re-inflate a total that has been corrected.
    confirmed: bool,
}

/// The state of filling a queue up to the whole of its listing.
///
/// Held while a queue built from a listing still has rows of that listing left
/// to fetch. One page is in flight at a time, exactly as [`Paging`] pages a
/// view, and the reply is matched by `source` so a page for a queue that has
/// since been replaced is dropped rather than appended to the wrong one.
struct QueueFill {
    /// The listing being paged, and the identity the reply is matched against.
    source: QueueSource,
    /// The next offset to ask for: where the queue's rows end and the fill goes
    /// on. Advances by the size of each page that lands.
    next: u32,
    /// How many rows the listing has, so the fill knows when it is done. Never
    /// past [`QUEUE_MAX`], which is the ceiling whatever the listing's length.
    total: u32,
    /// Whether a page is in flight, so the once-a-frame driver asks for one at a
    /// time rather than a burst.
    inflight: bool,
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
            confirmed: false,
        };
    }

    /// Record the listing's true length, learnt from outside this view's own
    /// paging - the queue fill having paged the same listing to its end.
    ///
    /// Marks the count exact so [`Self::absorb`] will not let a later page fold
    /// the service's over-count back in. Never below what is already loaded,
    /// for the same reason `absorb` is not: a figure under the rows on screen
    /// would read as fewer than are plainly there.
    fn confirm(&mut self, count: u32, loaded: u32) {
        self.total = count.max(loaded);
        self.confirmed = true;
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
            // an empty answer asks for that same empty page on every tick. This
            // is the end, so the count is now exact.
            self.total = loaded;
            self.confirmed = true;
            return;
        }
        if self.confirmed {
            // The end has already been seen - here or by the queue fill - so the
            // total is exact. A later page may only carry the loaded count up to
            // it, never fold the service's over-count back in.
            self.total = self.total.max(loaded);
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
    pub mixes: Vec<Mix>,
    pub mix_tracks: Vec<Track>,
    pub open_mix: Option<(String, String)>, // (id, title)
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
    pub mixes_paging: Paging,
    pub mix_tracks_paging: Paging,
    pub search_paging: Paging,

    pub selected: usize,
    pub list_offset: usize,

    /// Which list the keyboard was last asked for, which is not the same thing
    /// as which list has it: see [`App::focus`].
    focus_wanted: Focus,
    /// The row of the play order under the queue's own cursor, which is a row
    /// of the panel and not an index into `queue`: see [`App::queue_at`].
    pub queue_selected: usize,
    /// The first row of the play order on screen, maintained by the renderer
    /// exactly as `list_offset` is.
    pub queue_offset: usize,

    pub now_playing: Option<Track>,
    pub now_meta: StreamMeta,
    pub status: PlaybackStatus,

    // Play queue + gapless pipeline state.
    pub queue: Vec<Track>,
    pub queue_pos: usize,
    /// The order the queue is played in: a permutation of indices into
    /// [`Self::queue`], and never a reordering of the queue itself.
    ///
    /// **Two structures on purpose.** The queue is what the listener chose, in
    /// the order they chose it; this says what follows what. Keeping them apart
    /// is what lets the shuffle be turned off with the track still playing -
    /// shuffling the queue in place would have thrown the listing order away,
    /// leaving nothing to go back to.
    ///
    /// Honoured only while it is as long as the queue, and the listing order is
    /// what a queue without one reads back as. See [`Self::queue_at`].
    order: Vec<usize>,
    expected_id: u64,
    current_target: Option<u64>,
    next_intended: Option<u64>,
    metas: HashMap<u64, StreamMeta>,
    advanced: bool,

    /// Whether the queue running out should be followed by the radio.
    ///
    /// **On unless it is turned off**: a player that stops dead at the end of a
    /// chosen queue is a surprising silence, so the service's own suggestions
    /// carry it on. The `c` key and the header control turn it off for the
    /// listener who wants the queue to end where it ends, and a repeat outranks
    /// it either way.
    ///
    /// Session state rather than a setting, in the sense
    /// `docs/adr/0004-a-settings-file-that-is-edited-never-rewritten.md` means
    /// it: the file holds what a flag can already set, and its own exclusion
    /// list names the shuffle flag - the toggle beside this one - as the kind of
    /// thing that stays out.
    pub continue_radio: bool,
    /// Whether the queue, or the track, starts again rather than ending.
    ///
    /// Independent of [`Self::continue_radio`] and never written by it: a
    /// repeating queue simply has no end for the radio to be asked at, so the
    /// two interact through the queue rather than through each other.
    pub repeat: Repeat,
    /// The track whose radio has already been asked about, whatever the answer.
    ///
    /// The preload decision fires on every tick for as long as nothing is queued
    /// behind the current track, which at the end of a queue is every tick of
    /// the last one. Without this, that is ten requests a second - and ten
    /// notices a second for a track that has no mix to ask for.
    radio_asked: Option<u64>,
    /// Where in the queue the radio took over, if it has.
    ///
    /// An index rather than a flag so a skip back into what the listener
    /// actually chose stops claiming to be a suggestion. Cleared whenever the
    /// queue is rebuilt, because that is a fresh choice.
    radio_from: Option<usize>,

    /// The listing a queue was built from, and how far it has been filled - or
    /// `None` when the queue holds the whole of its source already, or came from
    /// something that cannot be paged (a filtered listing).
    ///
    /// This is what makes "play my favorites" mean all of them: the queue starts
    /// as the rows that were loaded, and keeps growing in the background until it
    /// holds the listing. Correlated by [`QueueSource`], not by the view, so the
    /// fill goes on after the listener has navigated somewhere else.
    queue_fill: Option<QueueFill>,

    /// Tracks priel knows are in the user's favorites.
    ///
    /// **The service reports no favorite flag on a track.** A track carries its
    /// title, its artists and its quality tags and nothing that says whether the
    /// listener kept it, so the only thing that ever says so is membership of
    /// the favorites listing itself. That makes this set priel's own record, and
    /// it knows exactly as much as it has been told: the favorites pages that
    /// have been fetched, plus whatever has been changed here since.
    ///
    /// The consequence is worth naming rather than hiding. A favorited track met
    /// in the search results, on a page of the favorites that has not been
    /// scrolled to, reads as not favorited. Pressing the key on it adds it
    /// again, which the service accepts, and the heart then tells the truth.
    favorite_ids: HashSet<u64>,

    /// priel's place on the session bus, or `None` on a machine that has none.
    ///
    /// `None` is the ordinary answer on a media-server box, and it costs
    /// nothing at all: no thread, and no snapshot built on any tick.
    bus: Option<Bus>,
    /// What the desktop was last told. The next tick is announced by the
    /// difference between this and what is true then.
    published: Snapshot,
    /// How many plays priel has started.
    ///
    /// Mints the object path a queue entry is known by on the bus. It counts
    /// *plays* rather than tracks because a consumer resets its position when
    /// the id changes: the same track twice in a queue needs two ids, and one
    /// play of one entry must keep one throughout.
    plays: u64,
    /// The path the entry playing now is known by.
    track_path: String,

    pub mode: Mode,
    pub filter: String,
    pub shuffle: bool,

    /// Which palette is in force, and the colours it stands for. Held together
    /// so the picker can mark the row that is current without mapping a palette
    /// back to a name.
    theme_name: ThemeName,
    theme: Theme,
    /// Which row of the theme picker is highlighted.
    theme_selected: usize,
    /// Clickable theme rows and the palette each stands for, rebuilt by the
    /// renderer while the picker is up - exactly as `device_rows` is.
    pub theme_rows: Vec<(Rect, ThemeName)>,

    /// What the modal on screen is asking about, and what to do with the answer.
    ///
    /// `None` in every mode that is not [`Mode::Prompt`] or [`Mode::Confirm`],
    /// and the guard those modes act through: an answer with nothing pending
    /// does nothing at all, which is what stops a stale confirmation from
    /// deleting whatever happens to be selected now.
    pending: Option<Pending>,
    /// The name being typed in the prompt.
    pub prompt_text: String,
    /// The track the playlist picker is finding a home for. `None` unless
    /// [`Mode::AddTo`] is up.
    add_track: Option<u64>,
    /// Which row of the playlist picker is highlighted.
    add_selected: usize,
    /// The first picker row on screen, maintained by the renderer as
    /// `device_offset` is.
    pub add_offset: usize,
    /// Clickable picker rows and the index into `playlists` each stands for.
    pub add_rows: Vec<(Rect, usize)>,

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
    /// How far down the reference is scrolled, from the top. Clamped by the
    /// renderer, which is the only thing that knows how tall the box came out.
    pub help_scroll: usize,
    /// The last chain the worker read, or the reason it could not. `None` while
    /// a read is in flight, which is what the overlay says it is doing.
    audio_graph: Option<Result<AudioGraph, GraphError>>,
    /// The "set up audio" flow while its overlay is up, else `None`. See
    /// [`Setup`].
    setup: Option<Setup>,
    /// What the sink at the end of the chain is doing to the level.
    ///
    /// Kept apart from `audio_graph`, which the overlay clears every time it
    /// opens so that no stale chain is ever shown as current. This one outlives
    /// that: the bottom row is graded on it on every frame, and it is the last
    /// thing that was read rather than the last thing that was asked for.
    /// [`SinkVolume::Unread`] until something says otherwise, which is the
    /// modest answer rather than the flattering one.
    sink_volume: SinkVolume,
    /// When the sink's volume was last asked for, or `None` for never.
    ///
    /// Reading it runs `pw-dump`, and the tick that would ask runs ten times a
    /// second, so the request is spaced by [`SINK_VOLUME_INTERVAL`].
    sink_volume_asked: Option<Instant>,
    /// The track whose cover has already been asked for, so a poll running ten
    /// times a second sends one request per track rather than a stream of them.
    ///
    /// Not the same as holding the cover: a request may be in flight, or may
    /// have failed and returned nothing, and either way asking again every tick
    /// would be wrong. This is "asked", the cover map is "arrived", and a track
    /// with no cover id is never asked at all.
    cover_asked: Option<u64>,
    /// The tag of a newer release, once the forge has been asked and answered
    /// with one. `None` means "not checked, or already current" - the two are
    /// the same to the interface, which only ever has something to say when
    /// there is a newer version.
    update_available: Option<String>,
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
    /// Where the queue's entries were painted, or a zero rect where there is no
    /// column to paint them in. Written by the renderer every frame, and the
    /// one thing that says whether there is a second region at all.
    pub queue_inner: Rect,
    /// Whether the queue is given its column when the terminal is wide enough.
    ///
    /// An *intent*, not a geometry: the renderer still asks the width, and
    /// `queue_inner` is still the only answer to whether there is a region on
    /// screen. Kept for the session and never written to the settings file -
    /// the file holds what a flag can set, and this is a gesture rather than a
    /// setting.
    pub queue_shown: bool,
    /// Whether the album cover is wanted in the now-playing box.
    ///
    /// An *intent*, exactly like [`App::queue_shown`], and for the same reason:
    /// the renderer still asks whether the terminal has the rows to spare, so
    /// this can be true on a short terminal and draw nothing. A gesture rather
    /// than a setting, so it is kept for the session and never written to the
    /// settings file.
    pub cover_shown: bool,
    /// The cover for the track that is playing, once it has arrived.
    ///
    /// Decoded off the render thread and held as pixels, keyed by the track it
    /// belongs to: replies are correlated by id and never by request order, so a
    /// cover that arrives after the track changed is dropped rather than drawn
    /// over the one now playing.
    pub cover: Option<(u64, crate::art::Image)>,
    pub progress_rect: Rect,
    /// Clickable regions, rebuilt by the renderer every frame.
    pub hits: Vec<(Rect, Hit)>,
    /// What the last click meant, and when. The *meaning* rather than the row:
    /// the two lists sit side by side, so one screen row is a row in each, and
    /// a clock keyed on the row number read a click in each region as a double
    /// click on the second.
    last_click: Option<(Click, Instant)>,
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
    /// What a picker chose this session, and nothing else.
    ///
    /// `main` writes it out once the loop has ended. **`App` holds no path and
    /// never writes**: file I/O has no business on the render thread, and tests
    /// build an `App`, so anything that could reach the user's home directory
    /// from here would eventually reach a real one.
    chosen: Settings,
}

/// Snapshot of the render-relevant state that moves on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
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
    sample_rate: u32,
    spinner: usize,
}

impl App {
    /// `bus` is priel's place on the session bus, or `None` where there is
    /// none. It is opened by `main` rather than here for the same reason the
    /// migration is: it talks to the machine, and tests build an `App`.
    pub fn new(
        player: PlayerConfig,
        token_path: String,
        recent: crate::logging::Recent,
        theme: ThemeName,
        bus: Option<Bus>,
    ) -> anyhow::Result<Self> {
        // Read before the config is handed over: the picker shows what was
        // asked for, and `--exclusive` is where a session starts from.
        let exclusive = player.exclusive;
        let player = Player::with_config(player)?;
        let creds_path = Credentials::default_path();
        let has_credentials = priel_core::auth::local_credentials(&creds_path).is_some();
        let worker = worker::spawn(token_path.clone(), creds_path.clone());
        let mut app = Self::with(player, worker);
        app.bus = bus;
        app.set_theme(theme);
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
            mixes: Vec::new(),
            mix_tracks: Vec::new(),
            open_mix: None,
            search_tracks: Vec::new(),
            search_query: String::new(),
            search_asked: String::new(),
            favorites_paging: Paging::default(),
            playlists_paging: Paging::default(),
            playlist_tracks_paging: Paging::default(),
            mixes_paging: Paging::default(),
            mix_tracks_paging: Paging::default(),
            search_paging: Paging::default(),
            selected: 0,
            list_offset: 0,
            focus_wanted: Focus::List,
            queue_selected: 0,
            queue_offset: 0,
            now_playing: None,
            now_meta: StreamMeta::default(),
            status: PlaybackStatus::default(),
            queue: Vec::new(),
            queue_pos: 0,
            order: Vec::new(),
            expected_id: 0,
            current_target: None,
            next_intended: None,
            metas: HashMap::new(),
            advanced: false,
            continue_radio: true,
            queue_fill: None,
            repeat: Repeat::Off,
            radio_asked: None,
            radio_from: None,
            favorite_ids: HashSet::new(),
            bus: None,
            published: Snapshot::default(),
            plays: 0,
            track_path: String::new(),
            mode: Mode::Normal,
            filter: String::new(),
            shuffle: false,
            theme_name: ThemeName::default(),
            theme: Theme::default(),
            theme_selected: 0,
            theme_rows: Vec::new(),
            pending: None,
            prompt_text: String::new(),
            add_track: None,
            add_selected: 0,
            add_offset: 0,
            add_rows: Vec::new(),
            notice: Some("Loading favorites…".into()),
            loading: true,
            frame: 0,
            should_quit: false,
            list_inner: Rect::default(),
            queue_inner: Rect::default(),
            queue_shown: true,
            cover_shown: true,
            cover: None,
            progress_rect: Rect::default(),
            hits: Vec::new(),
            last_click: None,
            dirty: true,
            worker_lost: false,
            recent: crate::logging::Recent::default(),
            log_scroll: 0,
            help_scroll: 0,
            audio_graph: None,
            setup: None,
            sink_volume: SinkVolume::Unread,
            sink_volume_asked: None,
            cover_asked: None,
            update_available: None,
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
            chosen: Settings::default(),
        }
    }

    // ---- the colour theme ----

    /// The palette the renderer paints with.
    ///
    /// [`Theme`] is `Copy` and the whole of it is twenty colours, so the
    /// renderer takes a copy at the top of each function rather than holding a
    /// borrow of `App` it would then have to give back before recording a hit
    /// box.
    #[must_use]
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Paint with a different palette from here on.
    ///
    /// The one place that changes it, so `--theme`, the `t` key and a click on
    /// a row of the picker cannot drift apart.
    fn set_theme(&mut self, name: ThemeName) {
        self.theme_name = name;
        self.theme = Theme::of(name);
        self.dirty = true;
    }

    /// Open the theme picker, on the palette already in force.
    ///
    /// The one way in: `t` and the header's `◐` control both come through here.
    fn open_themes(&mut self) {
        self.mode = Mode::Themes;
        self.theme_selected = theme::OFFERED
            .iter()
            .position(|n| *n == self.theme_name)
            .unwrap_or(0);
    }

    /// The picker: modal like the output one, and scrolled with the same keys.
    fn on_key_themes(&mut self, key: KeyEvent) {
        let last = theme::OFFERED.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('t' | 'q') => self.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down => {
                self.theme_selected = (self.theme_selected + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.theme_selected = self.theme_selected.saturating_sub(1);
            }
            KeyCode::Char('g') => self.theme_selected = 0,
            KeyCode::Char('G') => self.theme_selected = last,
            KeyCode::Enter => self.choose_theme(self.theme_selected),
            _ => {}
        }
        self.dirty = true;
    }

    /// Repaint in the palette on this row, and remember it.
    ///
    /// The one place a choice made in the picker lands: the Enter key and a
    /// click on a row both arrive here. Recorded rather than written - `main`
    /// writes the settings file once, on the way out, because a `write(2)` on
    /// the render thread is exactly what the log has a writer thread to avoid.
    fn choose_theme(&mut self, index: usize) {
        let Some(name) = theme::OFFERED.get(index).copied() else {
            return;
        };
        self.set_theme(name);
        self.chosen.theme = Some(name);
        self.notice = Some(format!(
            "Theme: {} — kept for next time",
            theme::label(name)
        ));
        self.mode = Mode::Normal;
    }

    /// A click inside the picker. On a row it takes that palette, and anywhere
    /// else it dismisses, as a click on the output picker does.
    fn click_theme(&mut self, col: u16, row: u16) {
        match self
            .theme_rows
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, n)| *n)
        {
            Some(name) => {
                self.theme_selected = theme::OFFERED
                    .iter()
                    .position(|n| *n == name)
                    .unwrap_or(self.theme_selected);
                self.choose_theme(self.theme_selected);
            }
            None => self.mode = Mode::Normal,
        }
    }

    /// Which row the picker is on.
    #[must_use]
    pub fn theme_selected(&self) -> usize {
        self.theme_selected
    }

    /// Which palette is in force, so the picker can mark that row.
    #[must_use]
    pub fn theme_name(&self) -> ThemeName {
        self.theme_name
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
        // The other text box. A playlist name is as likely to be pasted as
        // typed, and a box that took keystrokes but not a paste would be the
        // one text field in priel that behaved differently. Bounded here as it
        // is on the keystroke path, and by the same count.
        if self.mode == Mode::Prompt {
            for c in text.trim().chars() {
                if self.prompt_text.chars().count() >= PLAYLIST_NAME_MAX {
                    break;
                }
                self.prompt_text.push(c);
            }
            self.dirty = true;
        }
    }

    /// Open the authorization page again.
    ///
    /// The one way in: `Ctrl-O` and the screen's own control both come through
    /// here, as with the three below. A browser that was closed, or never
    /// opened because there is no desktop session, is the whole reason this is
    /// an action rather than something that only happens once.
    fn reopen_browser(&self) {
        if let Some(flow) = &self.login {
            open_in_browser(&flow.url);
        }
    }

    /// Empty the paste box and forget what the last attempt said about it.
    fn clear_paste(&mut self) {
        if let Some(flow) = self.login.as_mut() {
            flow.pasted.clear();
            flow.status = None;
        }
    }

    /// Abandon the sign-in, dropping the flow with it.
    fn cancel_login(&mut self) {
        self.login = None;
        self.mode = Mode::Normal;
    }

    fn on_key_login(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Letters go into the pasted text, so the actions take a
                // modifier rather than stealing characters from a URL.
                KeyCode::Char('o') => self.reopen_browser(),
                KeyCode::Char('u') => self.clear_paste(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel_login(),
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

    /// Say where the radio took the queue over, for renderer tests.
    #[cfg(test)]
    pub fn set_radio_from_for_test(&mut self, from: Option<usize>) {
        self.radio_from = from;
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

    /// Carry on without downloading a client identity.
    ///
    /// The one way out that is not quitting: `Esc`, `Enter` and the screen's own
    /// "not now" control all come through here.
    fn decline_credentials(&mut self) {
        self.mode = Mode::Normal;
    }

    /// The consent screen is modal: nothing reaches the list behind it.
    fn on_key_credentials(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('f') => self.fetch_credentials(),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Enter => self.decline_credentials(),
            _ => {}
        }
    }

    pub fn start(&mut self) {
        self.load_favorites_from_the_top();
    }

    /// Ask the worker whether a newer release exists.
    ///
    /// Not folded into [`App::start`], which every test calls: the check is a
    /// network call `main` makes once, when it is wanted, and a test that built
    /// an `App` should not have an update request queued behind its back. Sent
    /// at most the once - `main` calls this a single time - so there is no guard
    /// against repeats to keep here.
    pub fn check_for_updates(&mut self) {
        self.ask(ToWorker::CheckUpdate);
    }

    /// A newer release turned up; say so on the notice line and remember it.
    ///
    /// A notice rather than anything louder: it is worth telling, not worth
    /// interrupting, and it names the one command that acts on it. The tag is
    /// kept as well so a later screen could show it, but the notice is the whole
    /// of the feature for now.
    fn on_update_available(&mut self, tag: String) {
        self.notice = Some(format!(
            "priel {tag} is available. Update with: priel --update"
        ));
        self.update_available = Some(tag);
        self.dirty = true;
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
            View::Mixes => self.load_mixes_from_the_top(),
            View::MixTracks => {
                if let Some((id, _)) = self.open_mix.clone() {
                    self.load_mix_tracks_from_the_top(id);
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

    /// Ask for the first page of the listener's mixes, discarding any loaded.
    fn load_mixes_from_the_top(&mut self) {
        self.mixes_paging.restart(0);
        self.loading = true;
        self.ask(ToWorker::LoadMixes {
            offset: 0,
            limit: worker::MIXES_PAGE,
        });
    }

    /// Ask for the first page of a mix's tracks, discarding any loaded.
    ///
    /// No `known_total` to pass, unlike a playlist: a mix row carries no track
    /// count, so this view learns its own length only when its first page of
    /// tracks arrives.
    fn load_mix_tracks_from_the_top(&mut self, mix_id: String) {
        self.mix_tracks.clear();
        self.mix_tracks_paging.restart(0);
        self.loading = true;
        self.ask(ToWorker::LoadMixTracks {
            mix_id,
            offset: 0,
            limit: worker::MIX_TRACKS_PAGE,
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
            View::Mixes => &self.mixes_paging,
            View::MixTracks => &self.mix_tracks_paging,
            View::Search => &self.search_paging,
        }
    }

    fn paging_mut(&mut self) -> &mut Paging {
        match self.view {
            View::Favorites => &mut self.favorites_paging,
            View::Playlists => &mut self.playlists_paging,
            View::PlaylistTracks => &mut self.playlist_tracks_paging,
            View::Mixes => &mut self.mixes_paging,
            View::MixTracks => &mut self.mix_tracks_paging,
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
            Task::Mixes { offset } => Some((&mut self.mixes_paging, *offset)),
            Task::MixTracks { mix_id, offset } => {
                let open = self.open_mix.as_ref().is_some_and(|(m, _)| m == mix_id);
                open.then_some((&mut self.mix_tracks_paging, *offset))
            }
            Task::Search { query, offset } => {
                (*query == self.search_asked).then_some((&mut self.search_paging, *offset))
            }
            // A favorite belongs to no listing: it changes one row's state, not
            // how much of a list has been fetched.
            // Neither does a playlist edit: it changes one row, or makes one,
            // or takes one away. None of that is a page of a listing.
            // A radio belongs to no listing either: it extends the play queue,
            // which has no page and no offset to latch.
            Task::Startup
            | Task::Resolve
            | Task::Radio { .. }
            | Task::SetFavorite { .. }
            | Task::CreatePlaylist
            | Task::RenamePlaylist { .. }
            | Task::DeletePlaylist { .. }
            | Task::AddToPlaylist { .. }
            | Task::RemoveFromPlaylist { .. } => None,
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
            View::Mixes => self.mixes.len(),
            View::MixTracks => self.mix_tracks.len(),
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
            View::Mixes => ToWorker::LoadMixes {
                offset,
                limit: worker::MIXES_PAGE,
            },
            View::MixTracks => ToWorker::LoadMixTracks {
                mix_id: self.open_mix.as_ref()?.0.clone(),
                offset,
                limit: worker::MIX_TRACKS_PAGE,
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
        // The listing *is* the favorite state, since nothing on a track says
        // so, and the set follows the rows the way `absorb` does. Taken from the
        // arriving page rather than from the rows afterwards: the rows are the
        // whole listing loaded so far, and rebuilding from them would also throw
        // away a change made from another view - a track favorited out of the
        // search results, whose own page of the listing has not been reached.
        if offset == 0 {
            // The service's answer replaces what priel believed, this included.
            // Without it a track taken off the favorites on another device
            // would keep its heart until priel was restarted.
            self.favorite_ids.clear();
        }
        self.favorite_ids.extend(page.items.iter().map(|t| t.id));
        let mut rows = std::mem::take(&mut self.favorites);
        self.favorites_paging.absorb(&mut rows, offset, page);
        self.favorites = rows;
        self.loading = false;
        let loaded = self.favorites.len();
        self.notice = Some(match rows_missing(loaded, self.favorites_paging.total) {
            // Worded like the heading: the two figures are named, not
            // juxtaposed. `42 of 417` leaves the reader to work out which is
            // which, which is the whole of #23.
            Some(total) if usize::try_from(total).is_ok_and(|t| t == loaded) => {
                format!("all {total} favorites loaded")
            }
            Some(total) => format!("{loaded} loaded of {total} favorites"),
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

    /// A page of the listener's mixes arrived, matched by the offset it answers.
    fn on_mixes_page(&mut self, offset: u32, page: priel_core::Page<Mix>) {
        if self.mixes_paging.wanted != Some(offset) {
            log::debug!("dropping a mixes page at offset {offset}: nothing is waiting for it");
            return;
        }
        let mut rows = std::mem::take(&mut self.mixes);
        self.mixes_paging.absorb(&mut rows, offset, page);
        self.mixes = rows;
        self.loading = false;
        if self.view == View::Mixes {
            self.clamp_selection();
        }
    }

    /// A page of one mix's tracks arrived.
    ///
    /// Guarded on both halves of its identity for the reason the playlist
    /// tracks are: a reply for a mix the listener has left, and a superseded
    /// page of the mix that is open, are two different kinds of stale and only
    /// one of them is about the offset.
    fn on_mix_tracks_page(&mut self, mix_id: &str, offset: u32, page: priel_core::Page<Track>) {
        if self.open_mix.as_ref().is_none_or(|(m, _)| m != mix_id) {
            log::debug!("dropping tracks for {mix_id}: that mix is not open");
            return;
        }
        if self.mix_tracks_paging.wanted != Some(offset) {
            log::debug!("dropping {mix_id} tracks at offset {offset}: nothing is waiting for it");
            return;
        }
        let mut rows = std::mem::take(&mut self.mix_tracks);
        self.mix_tracks_paging.absorb(&mut rows, offset, page);
        self.mix_tracks = rows;
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
            // Named rather than juxtaposed; see the favorites notice.
            Some(total) if usize::try_from(total).is_ok_and(|t| t == loaded) => {
                format!("all {total} results loaded")
            }
            Some(total) => format!("{loaded} loaded of {total} results"),
            None => format!("{loaded} results"),
        });
        if self.view == View::Search {
            self.clamp_selection();
        }
    }

    /// How long the current listing is, whenever the service has said.
    ///
    /// Kept once everything is loaded rather than dropped, because the heading
    /// tells a complete listing from a partial one by comparing this figure
    /// with the rows in hand. That is what fixes `None` to one meaning - the
    /// service has never said - and it is the reverse of the older behaviour,
    /// where the number went missing to signal that the list was finished.
    #[must_use]
    pub fn rows_available(&self) -> Option<u32> {
        rows_missing(self.loaded_rows(), self.paging().total)
    }

    /// Track slice backing the current view (empty for the Playlists list).
    pub fn current_tracks(&self) -> &[Track] {
        match self.view {
            View::Favorites => &self.favorites,
            View::PlaylistTracks => &self.playlist_tracks,
            View::MixTracks => &self.mix_tracks,
            View::Search => &self.search_tracks,
            View::Playlists | View::Mixes => &[],
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
        } else if self.view == View::Mixes {
            // The subtitle is searched alongside the title because it is where
            // a mix says what is in it - the artists it was built around are
            // the thing a listener would think to type.
            for (i, m) in self.mixes.iter().enumerate() {
                if row_matches(&m.title, &m.subtitle, &f) {
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
        // A change the interface has already shown. Putting it back is what
        // makes showing it early safe.
        //
        // Unguarded, deliberately. A refusal that lands after the user has
        // pressed the key again looks like it should need a "only if nothing
        // has moved since" check, and does not: the state is one bit, so a
        // belief that has moved on is already `!wanted` and writing `!wanted`
        // over it changes nothing. A guard here would be a branch no input
        // could take, which is worse than none - it reads as protection that is
        // not there.
        if let Task::SetFavorite { track_id, wanted } = task {
            self.remember_favorite(*track_id, !*wanted);
        }
        // The other change shown before it was agreed to. Guarded by the uuid
        // rather than unguarded: unlike a favorite this is not one bit, and a
        // refusal arriving after a second rename would otherwise put back a
        // title two edits old.
        if let Task::RenamePlaylist { uuid, was } = task {
            self.retitle_playlist(uuid, was.clone());
        }
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
            // Every reply repaints the screen - except the background
            // sink-volume poll (an `AudioGraph`), which routinely finds the same
            // value with the report closed. It earns its repaint in its own arm
            // rather than taking the blanket one, so an idle poll costs no frame.
            if !matches!(msg, FromWorker::AudioGraph(_)) {
                self.dirty = true;
            }
            match msg {
                FromWorker::Favorites { offset, page } => self.on_favorites_page(offset, page),
                FromWorker::Playlists { offset, page } => self.on_playlists_page(offset, page),
                FromWorker::PlaylistTracks { uuid, offset, page } => {
                    self.on_playlist_tracks_page(&uuid, offset, page);
                }
                FromWorker::Mixes { offset, page } => self.on_mixes_page(offset, page),
                FromWorker::MixTracks {
                    mix_id,
                    offset,
                    page,
                } => self.on_mix_tracks_page(&mix_id, offset, page),
                FromWorker::Radio { mix_id, page } => self.on_radio_page(&mix_id, page),
                FromWorker::QueueFilled {
                    source,
                    offset,
                    page,
                } => self.on_queue_filled(&source, offset, page),
                FromWorker::SearchResults {
                    query,
                    offset,
                    page,
                } => self.on_search_page(&query, offset, page),
                FromWorker::Resolved(id, r) => self.on_resolved(id, &r),
                FromWorker::PlaylistCreated(made) => self.on_playlist_created(made),
                FromWorker::PlaylistDeleted { uuid } => self.on_playlist_deleted(&uuid),
                FromWorker::PlaylistTrackRemoved { uuid, track_id } => {
                    self.on_playlist_track_removed(&uuid, track_id);
                }
                FromWorker::PlaylistTrackAdded { title } => {
                    self.notice = Some(format!("Added to “{title}”."));
                }
                FromWorker::AudioGraph(read) => {
                    let before = self.sink_volume.clone();
                    self.note_sink_volume(&read);
                    // The Bluetooth codec rides these graph reads (the 5-second
                    // sink-volume poll and the report open); thread it to the
                    // player so the verdict grades a Bluetooth output. Only on a
                    // successful read - a transient NoStream must not clear a
                    // codec already known.
                    if let Ok(graph) = &read {
                        self.player.set_bt_codec(graph.bt_codec.clone());
                    }
                    self.audio_graph = Some(read);
                    // The reply can be longer than the request that opened the
                    // overlay left room for, so the scroll starts again rather
                    // than pointing past the end of the new reading.
                    self.graph_scroll = 0;
                    // The overlay shows the whole graph; the now-playing badge
                    // rests only on the sink volume. Repaint for the first
                    // whenever it is open, for the second only when the value
                    // moved - the 5-second background poll usually finds neither.
                    if self.mode == Mode::Graph || self.sink_volume != before {
                        self.dirty = true;
                    }
                }
                FromWorker::AudioSetUp(result) => self.on_audio_set_up(result),
                FromWorker::AudioRestarted(result) => self.on_audio_restarted(result),
                // The clock read at track start: hand it to the player, where the
                // verdict falls back to it when there is no ALSA readout.
                FromWorker::OutputClock(clock) => self.player.set_clock(clock),
                FromWorker::Cover { track_id, image } => self.on_cover(track_id, image),
                FromWorker::UpdateAvailable(tag) => self.on_update_available(tag),
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
            // The figure the progress bar actually shows (see `App::duration`),
            // not mpv's raw `duration`. For a segment stream mpv does not know
            // the length and estimates it, and that estimate *grows* about once
            // a second - a number the UI never displays, yet it was repainting
            // the whole window every time it ticked up. The listing's figure is
            // fixed, so the signature stops moving with it.
            duration: self.duration() as u64,
            paused: s.paused,
            playing: s.playing,
            loaded: s.loaded,
            volume: s.volume as u32,
            current_id: s.current_id,
            has_next: s.has_next,
            // Deliberately NOT the buffered-seconds count. mpv's
            // demuxer-cache-duration wobbles across whole-second boundaries
            // several times a second, and driving the redraw off it repaints
            // that often - a full-window GPU composite each time - for a number
            // nobody needs to the fraction of a second. The buffered readout
            // rides the once-a-second position update instead.
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
        // After the status and before the queue reacts to it: a `Seek` from the
        // desktop is answered against what is playing now rather than against
        // the tick before it.
        self.drain_bus();
        self.refresh_from_status();
        self.page_in_more();
        self.refresh_devices();
        self.publish();
    }

    /// The half of `refresh` that reacts to `self.status`, split out so tests can
    /// drive playback states the null player will never produce on its own.
    #[cfg(test)]
    fn refresh_for_test(&mut self) {
        self.drain_bus();
        self.refresh_from_status();
        self.publish();
    }

    // ---- the session bus ----

    /// Run what the desktop asked for since the last tick.
    ///
    /// Bounded, like every other loop fed from outside: a consumer holding down
    /// a media key may not hold the render thread, and what is left waits a
    /// tenth of a second for the next tick.
    fn drain_bus(&mut self) {
        for _ in 0..MAX_BUS_COMMANDS_PER_TICK {
            let Some(command) = self.bus.as_ref().and_then(Bus::next_command) else {
                return;
            };
            self.apply(command);
        }
    }

    /// Do what a consumer asked for.
    ///
    /// **Every arm calls the method a key already calls.** MPRIS is a third
    /// caller of an action that exists - `Next` is what `n` and the header
    /// control run - so there is one implementation of each and three ways in.
    /// An arm that did its own thing here would be an action with no way to
    /// reach it from the terminal.
    fn apply(&mut self, command: BusCommand) {
        match command {
            BusCommand::Next => self.user_next(),
            BusCommand::Previous => self.user_prev(),
            // Absolute, where the space bar is a toggle. Answering `Play` with
            // a toggle pauses a playing track when a panel applet's play button
            // is pressed twice.
            BusCommand::Play => self.player.set_paused(false),
            BusCommand::Pause => self.player.set_paused(true),
            BusCommand::PlayPause => self.player.toggle_pause(),
            BusCommand::SeekTo(position_us) => self.player.seek(seconds(position_us)),
            BusCommand::Shuffle(on) => self.set_shuffle(on),
            BusCommand::Loop(repeat) => self.set_repeat(repeat),
            BusCommand::Volume(unity) => self.player.set_volume(unity * 100.0),
            BusCommand::Quit => self.should_quit = true,
        }
        self.dirty = true;
    }

    /// Tell the desktop what changed, and hand the bus thread what to answer
    /// from.
    ///
    /// Nothing here runs without a bus, which is what keeps the media-server
    /// box paying nothing for a desktop it does not have.
    fn publish(&mut self) {
        if self.bus.is_none() {
            return;
        }
        let snapshot = self.bus_snapshot();
        if let Some(bus) = self.bus.as_ref() {
            // A jump is announced by `Seeked` and never by the position itself,
            // which spec 2.2 forbids putting in a property change.
            if let Some(position_us) = mpris::seeked(&self.published, &snapshot) {
                bus.emit(mpris::seeked_signal(position_us));
            }
            // **One signal, not two.** A gapless transition changes the metadata
            // while the playback status stays `Playing`, and two signals let a
            // consumer render the old title against the new position.
            let changed = mpris::changed(&self.published.now, &snapshot.now);
            if !changed.is_empty() {
                bus.emit(mpris::properties_changed(mpris::PLAYER_INTERFACE, changed));
            }
            bus.publish(snapshot.clone());
        }
        self.published = snapshot;
    }

    /// What the desktop is told, from what the app knows.
    ///
    /// The one place the queue's vocabulary becomes MPRIS's. Each capability is
    /// the condition the method it enables actually acts on, so a button the
    /// desktop offers is one that does something.
    fn bus_snapshot(&self) -> Snapshot {
        Snapshot {
            now: Now {
                track: self.now_playing.as_ref().map(|track| Entry {
                    path: self.track_path.clone(),
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    album: track.album.clone(),
                    length_us: i64::from(track.duration_secs) * 1_000_000,
                }),
                paused: self.status.paused,
                shuffle: self.shuffle,
                repeat: self.repeat,
                // Rounded, so that a float wobbling in its last bits is not a
                // property change announced ten times a second.
                volume: (self.status.volume * 10.0).round() / 1000.0,
                can_go_next: self.can_go_next(),
                // `user_prev` always does something with a queue behind it: at
                // the very least it starts the current track again.
                can_go_previous: !self.queue.is_empty(),
                // mpv reports no duration until a track is decoding, and a seek
                // bar over an unknown length is a control that does nothing.
                can_seek: self.status.duration > 0.0,
            },
            position_us: micros(self.status.position),
        }
    }

    /// The two branches of `user_next` that come to something, as one answer.
    ///
    /// It mirrors that method rather than describing it, down to the repeat that
    /// is skipped past, so a button the desktop offers is a button that does
    /// something.
    fn can_go_next(&self) -> bool {
        !self.queue.is_empty()
            && ((self.status.has_next && self.repeat != Repeat::One)
                || self.next_pos(self.repeat.skipped()).is_some())
    }

    /// Begin a new play of a queue entry, and mint the id it is known by.
    ///
    /// One play of one entry keeps one id throughout, and the same track twice
    /// in a queue gets two - which is what a consumer reads to decide whether
    /// to reset its position.
    fn mint_play(&mut self) {
        self.plays = self.plays.wrapping_add(1);
        self.track_path = mpris::track_path(self.plays);
        // A track is starting: read the server's graph clock once, here rather
        // than on the status poll. It is what the verdict falls back to on an
        // output with no ALSA readout - a Bluetooth sink - so a resample the
        // server did on our behalf is caught rather than hidden.
        self.ask(ToWorker::ReadClock);
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
        self.mint_play();
        self.expected_id = id;
        // We just advanced, so the end-of-track fallback must not also fire.
        self.advanced = true;
        if let Some(p) = self.queue.iter().position(|t| t.id == id) {
            self.queue_pos = p;
            self.follow_queue_cursor();
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
        self.poll_sink_volume();
        self.poll_cover();

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
            // Fetched on every visit, where the playlists are fetched once. A
            // playlist is still what the listener left it as; a mix is rebuilt
            // by the service under them, so a copy held from the last visit is
            // stale by construction rather than by bad luck. Arriving here is
            // already a deliberate action, and it costs one request.
            View::Mixes => self.load_mixes_from_the_top(),
            View::Search if self.search_tracks.is_empty() => {
                self.mode = Mode::Search; // start typing a query
            }
            _ => {}
        }
    }

    /// Leave a drill-down for the list it was opened from.
    ///
    /// The one place that knows which parent a nested view belongs to. `Esc`
    /// and the reference's own `Esc` both come through here, so the two cannot
    /// come to disagree about where back is. Anywhere else it does nothing -
    /// `Esc` has never quit and must not start.
    fn go_back(&mut self) {
        match self.view {
            View::PlaylistTracks => self.switch_view(View::Playlists),
            View::MixTracks => self.switch_view(View::Mixes),
            _ => {}
        }
    }

    /// Put the cursor in the search box, going to that view first if need be.
    ///
    /// The one way in: `i` and the `i` control in the reference overlay both
    /// come through here. Not guarded by the current view, because a control the
    /// mouse could reach only from one screen would be an action the keyboard
    /// had a shorter route to than the mouse - the asymmetry, backwards. Already
    /// on the search view it only reopens the box, leaving the results and the
    /// selection where they were.
    fn edit_search(&mut self) {
        if self.view != View::Search {
            self.switch_view(View::Search);
        }
        self.mode = Mode::Search;
    }

    fn cycle_view(&mut self) {
        let next = match self.view {
            View::Favorites => View::Playlists,
            View::Playlists | View::PlaylistTracks => View::Search,
            View::Search => View::Mixes,
            View::Mixes | View::MixTracks => View::Favorites,
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

    fn open_selected_mix(&mut self) {
        let vis = self.visible();
        if let Some(&idx) = vis.get(self.selected)
            && let Some(m) = self.mixes.get(idx).cloned()
        {
            self.open_mix = Some((m.id.clone(), m.title));
            self.view = View::MixTracks;
            self.selected = 0;
            self.list_offset = 0;
            self.filter.clear();
            self.load_mix_tracks_from_the_top(m.id);
        }
    }

    fn on_enter(&mut self) {
        // The queue is a list of tracks whatever view is behind it, so Enter
        // there always means play - never "open", which is what it means on the
        // two views that hold something to open.
        if self.focus() == Focus::Queue {
            self.play_queue_selected();
            return;
        }
        match self.view {
            View::Playlists => self.open_selected_playlist(),
            View::Mixes => self.open_selected_mix(),
            _ => self.play_selected(),
        }
    }

    /// Play the queue entry under the queue's cursor, forward or back.
    ///
    /// Straight to [`Self::load_fresh`] rather than through the skip keys: this
    /// names a place in the queue, and the skip keys name a direction from
    /// where it is now. Going back through what has played is what makes the
    /// history above the current track navigation rather than a readout.
    fn play_queue_selected(&mut self) {
        if let Some(p) = self.queue_at(self.queue_selected) {
            self.load_fresh(p);
        }
    }

    // ---- navigation ----

    /// Which list the keyboard is actually driving.
    ///
    /// Derived rather than stored, and that is what keeps the width out of the
    /// key handler: the queue can only hold the keyboard where the renderer
    /// published a region for it, and it publishes one only in the now-playing
    /// column, which exists at 120 columns and up unless it has been folded
    /// away. A terminal narrowed under the
    /// listener's fingers hands the keys back on the next frame with nothing
    /// having to notice.
    #[must_use]
    pub fn focus(&self) -> Focus {
        if self.queue_inner.height > 0 {
            self.focus_wanted
        } else {
            Focus::List
        }
    }

    /// Hand the keyboard to the other list. **The one way in**, for the key and
    /// for a click alike.
    fn cycle_focus(&mut self) {
        if self.queue_inner.height == 0 {
            // Three reasons there is nothing to hand the keyboard to, and they
            // want different things of the listener. Naming the width on a
            // two-hundred-column terminal with an empty queue would be telling
            // them to fix something that is not wrong, and naming it while the
            // column is merely folded away would send them to resize a terminal
            // that is already wide enough.
            self.notice = Some(
                if self.queue.is_empty() {
                    "Nothing is queued yet: press Enter on a track."
                } else if !self.queue_shown {
                    "The queue column is hidden: press W to bring it back."
                } else {
                    "The queue needs a terminal 120 columns wide, and rows to spare."
                }
                .into(),
            );
            return;
        }
        self.give_focus(match self.focus_wanted {
            Focus::List => Focus::Queue,
            Focus::Queue => Focus::List,
        });
    }

    /// Show or hide the queue's column. **The one way in**, for the key and for
    /// the header control alike.
    ///
    /// It changes an intent and nothing else. Whether there is a region on
    /// screen is still the renderer's answer, published as `queue_inner`, so
    /// hiding the column hands the keyboard back on the next frame by exactly
    /// the route narrowing the terminal does - there is no second path to keep
    /// in step with that one.
    fn toggle_queue_column(&mut self) {
        self.queue_shown = !self.queue_shown;
    }

    /// Hide the album cover, or ask for it back. **The one way in**, from the
    /// key and from the header control alike.
    ///
    /// An intent and nothing else, like [`App::toggle_queue_column`]: a terminal
    /// without the rows to spare draws no cover whatever this says, and that
    /// decision stays with the renderer so there is only one place it is made.
    fn toggle_cover(&mut self) {
        self.cover_shown = !self.cover_shown;
    }

    /// The cover to draw right now, if the one held belongs to the track that is
    /// playing.
    ///
    /// Asked rather than stored so that a track change needs no invalidation
    /// step: the moment `now_playing` moves on, the held cover stops answering,
    /// and whatever arrives for the new track replaces it.
    #[must_use]
    pub fn cover_for_now_playing(&self) -> Option<&crate::art::Image> {
        let playing = self.now_playing.as_ref()?.id;
        self.cover
            .as_ref()
            .and_then(|(id, image)| (*id == playing).then_some(image))
    }

    /// Point the keyboard at a region, clamping the cursor it finds there.
    ///
    /// The queue is the one list that changes length without anybody moving
    /// through it - the radio extends it, and a new one replaces it - so its
    /// cursor is checked on the way in rather than trusted.
    fn give_focus(&mut self, focus: Focus) {
        self.focus_wanted = focus;
        self.queue_selected = self.queue_selected.min(self.queue.len().saturating_sub(1));
    }

    fn move_down(&mut self, by: usize) {
        match self.focus() {
            Focus::List => {
                let n = self.visible().len();
                if n > 0 {
                    self.selected = (self.selected + by).min(n - 1);
                }
            }
            Focus::Queue => {
                let n = self.queue.len();
                if n > 0 {
                    self.queue_selected = (self.queue_selected + by).min(n - 1);
                }
            }
        }
    }
    fn move_up(&mut self, by: usize) {
        match self.focus() {
            Focus::List => self.selected = self.selected.saturating_sub(by),
            Focus::Queue => self.queue_selected = self.queue_selected.saturating_sub(by),
        }
    }
    /// The height of whichever region the keys are moving through, because a
    /// page is a screenful of the list being paged and not of the other one.
    ///
    /// Counted in *entries*, not screen rows: the browse list spends one row per
    /// entry, but the queue spends [`QUEUE_ROWS_PER_ENTRY`], so a page there is
    /// that many fewer entries than the column is tall.
    fn page_rows(&self) -> usize {
        match self.focus() {
            Focus::List => self.list_inner.height as usize,
            Focus::Queue => self.queue_inner.height as usize / QUEUE_ROWS_PER_ENTRY,
        }
    }
    fn half_page(&self) -> usize {
        (self.page_rows() / 2).max(1)
    }
    fn full_page(&self) -> usize {
        self.page_rows().max(1)
    }

    // ---- queue + gapless playback ----

    /// Build the play queue from the rows on screen, start at one of them, and
    /// fill the queue up to the whole listing in the background.
    ///
    /// Playing a listing means playing the *listing*, not the rows that happened
    /// to be paged in - so the queue starts as what is loaded and then grows onto
    /// the end until it holds all of it, which is what makes a shuffle cover
    /// every favorite rather than the first hundred. That fill is
    /// [`App::begin_queue_fill`], correlated to the queue's source so it goes on
    /// after the listener has navigated away.
    ///
    /// A page arriving for the *view* still does not join the queue - only the
    /// fill, which is the queue's own, grows it. Extending it from an incidental
    /// view page would change what plays next without being asked; extending it
    /// from the fill is finishing the job the listener started by pressing play.
    /// A filtered listing takes neither: it is a deliberate subset and stays the
    /// snapshot it was.
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
        self.set_queue(tracks);
        let p = vis_index.min(self.queue.len() - 1);
        if self.shuffle {
            // The row that was pointed at starts, and the rest follows it
            // dealt: a track the listener chose must not be dealt into the
            // middle of the order with rows above it that never played.
            self.deal_order_from(0);
            if let Some(row) = self.order.iter().position(|&i| i == p) {
                self.order.swap(0, row);
            }
        }
        self.load_fresh(p);
        self.begin_queue_fill();
    }

    /// Replace the queue, and with it everything that was true of the last one.
    ///
    /// The one place a queue is built from scratch, so that what the radio added
    /// to the last one - and the mark that said so - cannot outlive it. Growing
    /// a queue is a different thing and goes through [`App::on_radio_page`].
    fn set_queue(&mut self, tracks: Vec<Track>) {
        self.queue = tracks;
        self.queue_selected = 0;
        self.queue_offset = 0;
        self.radio_from = None;
        self.radio_asked = None;
        // A fresh queue cancels the fill of the last one: a page still on its way
        // for the queue that was here is no longer wanted, and is dropped on
        // arrival because its source no longer matches. The caller that built
        // this queue from a listing starts its own fill.
        self.queue_fill = None;
        // A queue and its order are made together. The callers that want a
        // shuffled one deal this one again, which is a different decision from
        // "what is in the queue" and belongs to them.
        self.lay_listing_order();
    }

    /// The listing the current view would build a queue from, if it is one that
    /// can be paged on in the background.
    ///
    /// The playable track listings and nothing else: the playlist and mix
    /// *indexes* are not queues, and a view with no query or nothing open has no
    /// source to page.
    fn queue_source(&self) -> Option<QueueSource> {
        match self.view {
            View::Favorites => Some(QueueSource::Favorites),
            View::PlaylistTracks => Some(QueueSource::Playlist(
                self.open_playlist.as_ref()?.0.clone(),
            )),
            View::MixTracks => Some(QueueSource::Mix(self.open_mix.as_ref()?.0.clone())),
            View::Search if !self.search_asked.is_empty() => {
                Some(QueueSource::Search(self.search_asked.clone()))
            }
            _ => None,
        }
    }

    /// The page size to fill each source with, matched to how the view pages it.
    fn fill_limit(source: &QueueSource) -> u32 {
        match source {
            QueueSource::Favorites => worker::FAVORITES_PAGE,
            QueueSource::Playlist(_) => worker::PLAYLIST_TRACKS_PAGE,
            QueueSource::Mix(_) => worker::MIX_TRACKS_PAGE,
            QueueSource::Search(_) => worker::SEARCH_PAGE,
        }
    }

    /// Start filling the queue in the background up to the whole of its listing.
    ///
    /// Called by the two paths that build a queue from the visible list. It is
    /// what turns "play the rows that happened to be loaded" into "play the
    /// listing": the queue keeps the rows it started with and grows onto the end
    /// until it holds the listing or reaches [`QUEUE_MAX`].
    ///
    /// Does nothing when there is a **filter** active: the queue is then the
    /// filtered subset the listener chose to see, and paging the unfiltered
    /// listing in behind it would fill the queue with rows the filter hides.
    /// Does nothing either when the queue already holds the whole listing, which
    /// is the ordinary small-playlist case.
    fn begin_queue_fill(&mut self) {
        if !self.filter.is_empty() {
            return;
        }
        let Some(source) = self.queue_source() else {
            return;
        };
        let total = self.paging().total.min(QUEUE_MAX_U32);
        let Ok(have) = u32::try_from(self.queue.len()) else {
            return;
        };
        if have >= total {
            return; // the queue is already the whole listing
        }
        self.queue_fill = Some(QueueFill {
            source,
            next: have,
            total,
            inflight: false,
        });
        self.request_next_fill();
    }

    /// Ask for the next page of the queue's fill, if one is due.
    ///
    /// One page in flight at a time, chained from the reply rather than driven
    /// off the tick: the fill is a burst that runs once when a listing is
    /// played, not a thing to re-check ten times a second forever.
    fn request_next_fill(&mut self) {
        let Some(fill) = self.queue_fill.as_ref() else {
            return;
        };
        if fill.inflight {
            return;
        }
        if fill.next >= fill.total || self.queue.len() >= QUEUE_MAX {
            self.queue_fill = None; // the listing is in, or the ceiling is reached
            return;
        }
        let source = fill.source.clone();
        let offset = fill.next;
        let limit = Self::fill_limit(&source);
        if let Some(fill) = self.queue_fill.as_mut() {
            fill.inflight = true;
        }
        self.ask(ToWorker::FillQueue {
            source,
            offset,
            limit,
        });
    }

    /// A page of the queue's listing arrived; grow the queue by it.
    ///
    /// Matched by source, so a page for a queue that has since been replaced is
    /// dropped. The new rows go on the end of the queue and are dealt into the
    /// not-yet-played part of the play order by [`App::extend_order`], exactly as
    /// a radio page is - so a shuffle keeps "no track twice until every other
    /// has played" across the fill, and the rows already on screen do not move.
    fn on_queue_filled(
        &mut self,
        source: &QueueSource,
        offset: u32,
        page: priel_core::Page<Track>,
    ) {
        let still_ours = self
            .queue_fill
            .as_ref()
            .is_some_and(|f| &f.source == source && f.next == offset);
        if !still_ours {
            return;
        }
        if let Some(fill) = self.queue_fill.as_mut() {
            fill.inflight = false;
        }
        // An empty page from the service is the listing's true end: it has
        // exactly `offset` rows, indices 0..offset and nothing beyond. Told
        // apart from the ceiling case below, where the page had rows but no
        // room, which is not an end.
        let service_end = page.items.is_empty();
        let room = QUEUE_MAX.saturating_sub(self.queue.len());
        let added: Vec<Track> = page.items.into_iter().take(room).collect();
        if added.is_empty() {
            if service_end {
                // The fill paged this listing to its end while the view stayed
                // lazy, so `offset` is the count the view's heading should show -
                // authoritative over the service's advertised over-count.
                self.confirm_listing_total(source, offset);
            }
            self.queue_fill = None; // the listing gave nothing more, or the ceiling
            return;
        }
        let Ok(grew_by) = u32::try_from(added.len()) else {
            self.queue_fill = None;
            return;
        };
        let grown_from = self.queue.len();
        self.queue.extend(added);
        self.extend_order(grown_from);
        if let Some(fill) = self.queue_fill.as_mut() {
            fill.next = fill.next.saturating_add(grew_by);
        }
        self.dirty = true;
        // The preload reads from the grown queue: a fill that lands while the
        // last loaded track is playing with nothing queued behind it is what
        // makes the next one there to load. Gated so a fill page cannot pile a
        // second preload onto one already committed - the duplicate that looped
        // the second track under shuffle.
        if self.wants_preload() {
            self.schedule_next();
        }
        self.request_next_fill();
    }

    /// Correct the view that pages `source`'s listing to its true length,
    /// learnt from the queue fill reaching the listing's end.
    ///
    /// Only when that view is still showing the same listing: a fill that
    /// finished after the listener moved to another playlist has nothing on
    /// screen to correct, and writing the count into whatever view is open now
    /// would put the wrong figure there. Favourites is always the favourites
    /// view; the other three must match the listing the view currently holds.
    fn confirm_listing_total(&mut self, source: &QueueSource, count: u32) {
        let loaded = match source {
            QueueSource::Favorites => self.favorites.len(),
            QueueSource::Playlist(id) if self.open_playlist.as_ref().map(|p| &p.0) == Some(id) => {
                self.playlist_tracks.len()
            }
            QueueSource::Mix(id) if self.open_mix.as_ref().map(|m| &m.0) == Some(id) => {
                self.mix_tracks.len()
            }
            QueueSource::Search(query) if &self.search_asked == query => self.search_tracks.len(),
            // The fill's listing is not the one on screen, so there is nothing
            // here to correct.
            _ => return,
        };
        let loaded = u32::try_from(loaded).unwrap_or(u32::MAX);
        // The guards above already dropped every mismatched view, so this maps
        // the source straight to its paging.
        let paging = match source {
            QueueSource::Favorites => &mut self.favorites_paging,
            QueueSource::Playlist(_) => &mut self.playlist_tracks_paging,
            QueueSource::Mix(_) => &mut self.mix_tracks_paging,
            QueueSource::Search(_) => &mut self.search_paging,
        };
        paging.confirm(count, loaded);
        self.dirty = true;
    }

    fn load_fresh(&mut self, pos: usize) {
        if pos >= self.queue.len() {
            return;
        }
        self.mint_play();
        self.queue_pos = pos;
        self.follow_queue_cursor();
        self.next_intended = None;
        let t = self.queue[pos].clone();
        self.current_target = Some(t.id);
        self.expected_id = t.id;
        self.now_playing = Some(t.clone());
        self.now_meta = StreamMeta::default();
        self.ask(ToWorker::Resolve(t.id));
    }

    /// The queue entry at this row of the play order, or `None` past the end.
    ///
    /// **The one translation from a row to a track.** The panel, the preload
    /// and every key that names a place rather than a direction all ask it, so
    /// no two of them can come to disagree about what is where.
    ///
    /// The listing order is the answer whenever the order does not fit the
    /// queue. An order a different queue's length is an order built for a
    /// different queue, and falling back to the listing is defined behaviour
    /// where indexing on through it is not.
    #[must_use]
    pub fn queue_at(&self, row: usize) -> Option<usize> {
        if self.order.len() == self.queue.len() {
            self.order.get(row).copied()
        } else {
            (row < self.queue.len()).then_some(row)
        }
    }

    /// Which row of the play order the music is on.
    ///
    /// Derived from `queue_pos` rather than stored beside it. Two fields that
    /// have to move together are two fields that one day will not, and this
    /// file already keeps the queue in lockstep with mpv's playlist - a third
    /// copy of where the music is would be the one to get wrong.
    #[must_use]
    pub fn playing_row(&self) -> usize {
        if self.order.len() == self.queue.len() {
            self.order
                .iter()
                .position(|&i| i == self.queue_pos)
                .unwrap_or(self.queue_pos)
        } else {
            self.queue_pos
        }
    }

    /// Deal the play order again from this row on, leaving the rows above it
    /// exactly where they are.
    ///
    /// **What is behind the listener stays behind them.** The rows above the
    /// one playing are what has already played in this order, which is what the
    /// panel dims and what `H` walks back through; dealing them again would
    /// move played tracks under the current one, where the panel says they are
    /// still to come.
    fn deal_order_from(&mut self, row: usize) {
        if row < self.order.len() {
            self.order[row..].shuffle(&mut rand::thread_rng());
        }
    }

    /// Lay the listing order over the queue: row *n* is queue entry *n*.
    ///
    /// The order every queue starts with, and the one the shuffle is turned off
    /// back to. It can be gone back to at all because the queue was never
    /// reordered - which is the reason the order is a second structure.
    fn lay_listing_order(&mut self) {
        self.order = (0..self.queue.len()).collect();
    }

    /// Take the entries the queue has just grown by into the play order.
    ///
    /// They go on the end of it, dealt among themselves under the shuffle,
    /// rather than into a fresh deal of everything still to come: a radio page
    /// landing must not rearrange the rows the listener is already reading.
    fn extend_order(&mut self, grown_from: usize) {
        // An order the wrong length was built for a different queue, and is not
        // one to extend. Laying the listing order over what was there is what
        // `queue_at` would have fallen back to anyway.
        if self.order.len() != grown_from {
            self.order = (0..grown_from).collect();
        }
        let mut added: Vec<usize> = (grown_from..self.queue.len()).collect();
        if self.shuffle {
            added.shuffle(&mut rand::thread_rng());
        }
        self.order.extend(added);
    }

    /// Keep the queue's cursor on what is playing, unless the listener has the
    /// keyboard there.
    ///
    /// Called from the two places `queue_pos` moves and nowhere else, so the
    /// column is a readout of where the music is right up until somebody starts
    /// driving it - and from that moment the music stops moving the cursor out
    /// from under their fingers.
    fn follow_queue_cursor(&mut self) {
        if self.focus() != Focus::Queue {
            self.queue_selected = self.playing_row();
        }
    }

    /// Ask for the entry after this one, so mpv has it before it is needed.
    ///
    /// **What is asked for here is what will play.** It used to be a guess: the
    /// shuffle picked afresh at the advance, so the track fetched during the
    /// last minute of the current one was almost never the track that turned
    /// up - a whole track downloaded for nothing, and a gap exactly where the
    /// gapless handover should have been. The order is laid out in advance, so
    /// this and the advance now ask the same question and get the same answer.
    fn schedule_next(&mut self) {
        if self.queue.is_empty() {
            self.next_intended = None;
            return;
        }
        if let Some(p) = self.next_pos(self.repeat) {
            // Repeat-one lands on the track already playing, and that is a
            // preload like any other: mpv's playlist takes a second entry for
            // it, so the repeat is gapless exactly as a change of track is.
            let id = self.queue[p].id;
            self.next_intended = Some(id);
            self.ask(ToWorker::Resolve(id));
        } else {
            // Nothing follows this track: the end of the play order, the
            // shuffled one exactly as the listing one, with no repeat to start
            // it again. The one moment the radio has anything to answer.
            self.next_intended = None;
            self.extend_with_radio();
        }
    }

    /// Whether a preload should be scheduled right now.
    ///
    /// The same decision the tick-driven path makes in [`Self::decide`], reused
    /// so a caller that grows the queue - a fill page or a radio page - cannot
    /// append a second preload on top of one already committed to mpv's
    /// playlist. That duplicate entry is what let a shuffle loop the second
    /// track: mpv chews through `[A, B, B, B, ...]` and each `B -> B` is a
    /// same-id transition the app cannot see. Gating the growers here, rather
    /// than changing `decide`, keeps `decide` the one place the call is made.
    fn wants_preload(&self) -> bool {
        Self::decide(&self.tick()).preload
    }

    /// Ask for the radio of whatever is playing, once.
    ///
    /// Called from the preload path rather than from the end-of-track fallback,
    /// and that is the point: the request goes out while the last track is still
    /// playing, so the answer is normally in the queue in time for the ordinary
    /// gapless preload to take it. Nothing here loads or plays anything.
    fn extend_with_radio(&mut self) {
        if !self.continue_radio {
            return;
        }
        let Some(seed) = self.queue.get(self.queue_pos) else {
            return;
        };
        // Once per track, whatever the answer was. See `radio_asked`.
        if self.radio_asked == Some(seed.id) {
            return;
        }
        self.radio_asked = Some(seed.id);
        if seed.mix_id.is_empty() {
            let title = seed.title.clone();
            log::info!("no radio mix on track {}; the queue ends here", seed.id);
            self.notice = Some(format!("No radio for “{title}”. The queue ends here."));
            return;
        }
        if self.queue.len() >= RADIO_MAX {
            self.notice = Some("The queue is as long as it goes. Play something to go on.".into());
            return;
        }
        self.ask(ToWorker::LoadRadio {
            mix_id: seed.mix_id.clone(),
            limit: worker::RADIO_PAGE,
        });
    }

    /// Put a radio's tracks on the end of the queue.
    ///
    /// The queue is a snapshot and a page landing does not join it - that rule
    /// is untouched, and this is not a page. It is the listener's standing
    /// answer to "and then?", applied at the one moment there is no other
    /// answer, and it is marked as the service's suggestion for as long as it
    /// plays.
    fn on_radio_page(&mut self, mix_id: &str, page: priel_core::Page<Track>) {
        let Some(seed) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        // Correlated by what the reply is for, never by it having arrived. A
        // radio asked for at the end of one queue can land after the listener
        // has started another, and appending it there extends a queue nobody
        // asked to extend, with a mix built round a track that is not playing.
        if seed.mix_id != mix_id {
            log::debug!("dropping the radio for {mix_id}: it is not what is playing");
            return;
        }
        let room = RADIO_MAX.saturating_sub(self.queue.len());
        let added: Vec<Track> = page
            .items
            .into_iter()
            // A track's own mix leads with that track, and replaying what just
            // finished reads as a fault rather than as a suggestion.
            .filter(|t| t.id != seed.id)
            .take(room)
            .collect();
        if added.is_empty() {
            self.notice = Some("The radio had nothing to add. The queue ends here.".into());
            return;
        }
        if self.radio_from.is_none() {
            self.radio_from = Some(self.queue.len());
        }
        let grown_from = self.queue.len();
        self.queue.extend(added);
        self.extend_order(grown_from);
        self.notice = Some(format!(
            "The radio for “{}” follows it: the service's suggestion, not yours.",
            seed.title
        ));
        self.dirty = true;
        // The ordinary preload, which is what carries every other track change -
        // gated by the same decision, so a radio page cannot pile a second
        // preload onto one already committed.
        if self.wants_preload() {
            self.schedule_next();
        }
        // If the track ran out before this arrived, the end-of-track fallback
        // has already fired and set the guard that stops it firing again. The
        // thing it was waiting for has now turned up, so the guard is cleared
        // and the *existing* decision starts the radio on the next tick.
        // Loading a track from here instead would be the second mechanism this
        // feature must not grow.
        //
        // Unconditional, deliberately: whether audio is still flowing is
        // `decide`'s question and it already asks it. Asking it here as well
        // would be a second place that has to be right about the same thing.
        self.advanced = false;
    }

    /// Is what is playing the service's suggestion rather than the listener's
    /// choice?
    ///
    /// Positional, so skipping back into what was actually chosen stops saying
    /// it was suggested.
    #[must_use]
    pub fn playing_from_radio(&self) -> bool {
        self.suggested(self.queue_pos)
    }

    /// Is the entry at this place in the queue the service's suggestion rather
    /// than the listener's choice?
    ///
    /// The same positional rule [`Self::playing_from_radio`] answers with, asked
    /// of any entry rather than only of the one playing - one rule with two
    /// callers, so the mark on a queue row and the word beside the playing
    /// track cannot come to disagree. Out-of-range indices answer `false`: an
    /// entry that is not there was suggested by nobody.
    /// Has the radio put anything in this queue?
    ///
    /// What the legend beside the queue's heading is drawn on: a mark is
    /// explained where there is a mark to explain and nowhere else, because a
    /// thirty-four cell column has no room for a glossary of what is not there.
    #[must_use]
    pub fn queue_has_suggestions(&self) -> bool {
        self.radio_from
            .is_some_and(|start| start < self.queue.len())
    }

    #[must_use]
    pub fn suggested(&self, index: usize) -> bool {
        self.radio_from
            .is_some_and(|start| index >= start && index < self.queue.len())
    }

    fn advance_fresh(&mut self) {
        if let Some(p) = self.next_pos(self.repeat) {
            self.load_fresh(p);
        }
    }

    /// Where the queue goes after the entry playing now, or `None` where it
    /// ends there.
    ///
    /// **One rule with three callers** - the gapless preload, the end-of-track
    /// fallback and a deliberate skip - so they cannot come to disagree about
    /// what a repeat means, which would show up only at the moment something
    /// else had already gone wrong. `repeat` is an argument rather than read
    /// from `self` because a skip asks a different question: see
    /// [`Repeat::skipped`].
    ///
    /// **The shuffle says what the play order is; the repeat says whether that
    /// order ends.** The two are answers to different questions, and only one
    /// of them is answered here. The shuffle has no branch in this function at
    /// all any more: it decides what the order *is*, laid out in advance, and
    /// this asks the same question of a shuffled order as of a listing one.
    ///
    /// The comment this replaced predicted that a real order would return
    /// `None` from the shuffle's branch and fall through to the repeat question
    /// the ordered end asks. It does - and the branch that would have returned
    /// it turned out not to be needed, because "the row after this one" already
    /// says everything the shuffle had to say.
    fn next_pos(&self, repeat: Repeat) -> Option<usize> {
        if self.queue.is_empty() {
            return None;
        }
        // Repeat-one is about *this* track, so it outranks the order under
        // either shape: there is no next track to pick when the answer is "this
        // one again".
        if repeat == Repeat::One {
            return Some(self.queue_pos);
        }
        let row = self.playing_row();
        if let Some(next) = self.queue_at(row + 1) {
            return Some(next);
        }
        // The end of the order. Repeat-all starts it again - and in a queue of
        // one that lands on the track already playing, which is what makes
        // repeat-all and repeat-one the same thing there on purpose rather than
        // by accident.
        if repeat == Repeat::All {
            return self.queue_at(0);
        }
        None
    }

    fn goto_top(&mut self) {
        match self.focus() {
            Focus::List => self.selected = 0,
            Focus::Queue => self.queue_selected = 0,
        }
    }

    fn goto_bottom(&mut self) {
        match self.focus() {
            Focus::List => self.selected = self.visible().len().saturating_sub(1),
            Focus::Queue => self.queue_selected = self.queue.len().saturating_sub(1),
        }
    }

    fn start_filter(&mut self) {
        self.mode = Mode::Filter;
        self.filter.clear();
        self.selected = 0;
    }

    // ---- favorites ----

    /// Is this track one priel knows the user has kept?
    ///
    /// The renderer's only question. `false` covers both "not a favorite" and
    /// "priel has not been told", which the doc on `favorite_ids` explains: with
    /// no favorite flag on a track there is no third answer to give, and a
    /// glyph that meant "unknown" would still have to be clickable and would
    /// still do the same thing when clicked.
    #[must_use]
    pub fn is_favorite(&self, track_id: u64) -> bool {
        self.favorite_ids.contains(&track_id)
    }

    /// Add the track to the user's favorites, or take it back off.
    ///
    /// **The one way in.** The `f` and `F` keys, the `[f]` control on the
    /// keyboard row, the reference overlay and the heart beside the playing
    /// track all arrive here, so no two of them can come to disagree about what
    /// the action is.
    ///
    /// **Optimistic on purpose.** The heart changes now and is put back by
    /// [`Self::on_failed`] if the service refuses. A control that waited for a
    /// round trip before answering would read as broken - this is the only
    /// keystroke in priel whose whole effect is one bit - and one bit is the
    /// cheapest thing there is to undo. The failure is not swallowed: it reaches
    /// the notice line the way every other refusal does.
    fn toggle_favorite(&mut self, track_id: u64) {
        let wanted = !self.is_favorite(track_id);
        self.remember_favorite(track_id, wanted);
        self.ask(ToWorker::SetFavorite {
            track_id,
            favorite: wanted,
        });
    }

    /// Record what priel believes about a track, without asking anything.
    fn remember_favorite(&mut self, track_id: u64, favorite: bool) {
        if favorite {
            self.favorite_ids.insert(track_id);
        } else {
            self.favorite_ids.remove(&track_id);
        }
    }

    /// The track under the cursor, if this view has tracks at all.
    ///
    /// Through `visible()` rather than straight into the backing vec: selection
    /// is an index into the filtered rows, and reading the backing vec with it
    /// would favorite a different track than the highlighted one whenever a
    /// filter was on.
    fn selected_track_id(&self) -> Option<u64> {
        let visible = self.visible();
        let index = *visible.get(self.selected)?;
        self.current_tracks().get(index).map(|t| t.id)
    }

    fn favorite_selected(&mut self) {
        if let Some(id) = self.selected_track_id() {
            self.toggle_favorite(id);
        }
    }

    fn favorite_now_playing(&mut self) {
        if let Some(id) = self.now_playing.as_ref().map(|t| t.id) {
            self.toggle_favorite(id);
        }
    }

    // ---- editing playlists ----

    /// The playlist under the cursor, if this view lists playlists.
    ///
    /// Through `visible()` for the reason [`Self::selected_track_id`] is: the
    /// selection indexes the filtered rows, and reading the backing vec with it
    /// would rename or delete a different playlist than the highlighted one
    /// whenever a filter was on. That is a wrong *delete*, so it is not a
    /// tidiness point.
    fn selected_playlist(&self) -> Option<Playlist> {
        let visible = self.visible();
        let index = *visible.get(self.selected)?;
        self.playlists.get(index).cloned()
    }

    /// Start making a playlist. **The one way in**, from `N` and from the
    /// reference's control.
    fn new_playlist(&mut self) {
        self.open_prompt(Pending::NewPlaylist, String::new());
    }

    /// Start renaming the highlighted playlist. **The one way in.**
    ///
    /// The box opens holding the current title rather than empty: a rename is
    /// usually an edit of what is there, and an empty box would make the
    /// commonest case the most typing.
    fn rename_selected_playlist(&mut self) {
        if self.view != View::Playlists {
            self.notice = Some("Renaming works on the playlists list.".into());
            return;
        }
        let Some(playlist) = self.selected_playlist() else {
            return;
        };
        let was = playlist.title.clone();
        self.open_prompt(
            Pending::Rename {
                uuid: playlist.uuid,
                was: was.clone(),
            },
            was,
        );
    }

    /// Ask before taking away whatever is highlighted. **The one way in**, from
    /// `X` and from the reference's control.
    ///
    /// Which thing that is depends on the view, and both answers are destructive
    /// enough to be confirmed rather than done: in the playlist list it is the
    /// playlist, and inside one it is the track. Anywhere else there is nothing
    /// this could mean, and saying so beats doing nothing silently.
    fn remove_selected(&mut self) {
        match self.view {
            View::Playlists => {
                let Some(playlist) = self.selected_playlist() else {
                    return;
                };
                self.open_confirm(Pending::DeletePlaylist {
                    uuid: playlist.uuid,
                    title: playlist.title,
                });
            }
            View::PlaylistTracks => {
                let Some((uuid, _)) = self.open_playlist.clone() else {
                    return;
                };
                let Some(track_id) = self.selected_track_id() else {
                    return;
                };
                let title = self
                    .current_tracks()
                    .iter()
                    .find(|t| t.id == track_id)
                    .map_or_else(String::new, |t| t.title.clone());
                self.open_confirm(Pending::RemoveTrack {
                    uuid,
                    track_id,
                    title,
                });
            }
            _ => {
                self.notice = Some("Nothing here can be removed - open a playlist first.".into());
            }
        }
    }

    /// Put the highlighted track into a playlist. **The one way in**, from `a`
    /// and from the reference's control.
    ///
    /// Opens a picker rather than acting, because "which playlist" has no
    /// answer the interface could guess: the one on screen is usually not the
    /// one the listener means, since tracks are collected from favorites and
    /// from search.
    fn add_selected_to_playlist(&mut self) {
        let Some(track_id) = self.selected_track_id() else {
            self.notice = Some("Highlight a track first.".into());
            return;
        };
        self.add_track = Some(track_id);
        self.add_selected = 0;
        self.add_offset = 0;
        self.mode = Mode::AddTo;
        // The picker lists what has been fetched. Somebody who has never opened
        // the playlists tab has fetched nothing, so ask now rather than showing
        // an empty picker that is not the truth.
        if self.playlists.is_empty() {
            self.load_playlists_from_the_top();
        }
    }

    /// Put a modal on screen asking for a name.
    fn open_prompt(&mut self, pending: Pending, text: String) {
        self.pending = Some(pending);
        self.prompt_text = text;
        self.mode = Mode::Prompt;
    }

    /// Put a modal on screen asking whether to go through with something.
    fn open_confirm(&mut self, pending: Pending) {
        self.pending = Some(pending);
        self.mode = Mode::Confirm;
    }

    /// Take the modal down without doing what it asked about.
    ///
    /// **`pending` is cleared here.** Leaving it set would let the next
    /// confirmation answer for a change the user had already backed out of.
    fn cancel_modal(&mut self) {
        self.pending = None;
        self.prompt_text.clear();
        self.add_track = None;
        self.mode = Mode::Normal;
    }

    /// What the name prompt is asking, for the renderer to put in its title.
    #[must_use]
    pub fn prompt_question(&self) -> Option<String> {
        match self.pending.as_ref()? {
            Pending::NewPlaylist => Some("Name the new playlist".into()),
            Pending::Rename { was, .. } => Some(format!("Rename “{was}”")),
            Pending::DeletePlaylist { .. } | Pending::RemoveTrack { .. } => None,
        }
    }

    /// What the confirmation is asking, for the renderer to put on screen.
    ///
    /// The words name the thing, never a uuid or an index: a question the reader
    /// cannot check is a question they will answer yes to out of habit. The
    /// delete says it cannot be undone, because it cannot.
    #[must_use]
    pub fn confirm_question(&self) -> Option<Vec<String>> {
        match self.pending.as_ref()? {
            Pending::DeletePlaylist { title, .. } => Some(vec![
                format!("Delete the playlist “{title}”?"),
                "It goes from the account, not just from priel, and".into(),
                "there is no way to bring it back.".into(),
            ]),
            Pending::RemoveTrack { title, .. } => {
                let from = self
                    .open_playlist
                    .as_ref()
                    .map_or_else(String::new, |(_, name)| name.clone());
                Some(vec![
                    format!("Take “{title}” out of “{from}”?"),
                    "The track stays in the catalogue; only this".into(),
                    "playlist changes.".into(),
                ])
            }
            Pending::NewPlaylist | Pending::Rename { .. } => None,
        }
    }

    /// What saying yes will do, in the words the control is labelled with.
    ///
    /// Named after the effect rather than "OK" or "yes": the label is the last
    /// thing read before the click, and it is the only place the reader is told
    /// which of the two buttons is the one that destroys something.
    #[must_use]
    pub fn confirm_verb(&self) -> Option<&'static str> {
        match self.pending.as_ref()? {
            Pending::DeletePlaylist { .. } => Some("delete it"),
            Pending::RemoveTrack { .. } => Some("remove it"),
            Pending::NewPlaylist | Pending::Rename { .. } => None,
        }
    }

    /// Accept what is typed. **The one way in**, from `Enter` and from the
    /// prompt's own control.
    ///
    /// A name of nothing but spaces is refused rather than sent: the service
    /// accepts it and the listener ends up with a row they cannot tell from the
    /// next one.
    fn submit_prompt(&mut self) {
        let title = self.prompt_text.trim().to_string();
        if title.is_empty() {
            self.notice = Some("A playlist needs a name.".into());
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.prompt_text.clear();
        self.mode = Mode::Normal;
        match pending {
            Pending::NewPlaylist => {
                // Waits, and has to: the uuid is the service's to choose, so
                // there is no row this could show early. The notice is what
                // stands in for the row until the reply arrives.
                self.notice = Some(format!("Creating “{title}”…"));
                self.ask(ToWorker::CreatePlaylist { title });
            }
            Pending::Rename { uuid, was } => {
                // Optimistic, like a favorite and for the same reasons: one
                // field, visible immediately, and put back exactly by
                // `on_failed` if the service refuses.
                self.retitle_playlist(&uuid, title.clone());
                self.ask(ToWorker::RenamePlaylist { uuid, title, was });
            }
            Pending::DeletePlaylist { .. } | Pending::RemoveTrack { .. } => {}
        }
    }

    /// Go through with the change the confirmation was asking about. **The one
    /// way in**, from `y` and from the confirmation's own control.
    ///
    /// **Neither of these is optimistic, and that is the deliberate difference
    /// from a favorite.** Both wait for the service to say it happened before
    /// the row leaves the screen. A favorite is one bit and the same keystroke
    /// puts it back; a deleted playlist is gone, and a row that vanished and
    /// then quietly reappeared would be read as a glitch rather than as the
    /// refusal it was.
    fn confirm_yes(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.mode = Mode::Normal;
        match pending {
            Pending::DeletePlaylist { uuid, title } => {
                self.notice = Some(format!("Deleting “{title}”…"));
                self.ask(ToWorker::DeletePlaylist { uuid, title });
            }
            Pending::RemoveTrack { uuid, track_id, .. } => {
                self.ask(ToWorker::RemoveFromPlaylist { uuid, track_id });
            }
            Pending::NewPlaylist | Pending::Rename { .. } => {}
        }
    }

    /// Give the row for `uuid` a new title, asking nothing.
    ///
    /// Shared by the optimistic rename and by the revert, so the two cannot
    /// come to write to different places.
    fn retitle_playlist(&mut self, uuid: &str, title: String) {
        if let Some(row) = self.playlists.iter_mut().find(|p| p.uuid == uuid) {
            row.title = title;
        }
    }

    /// Send the highlighted track to the chosen playlist. **The one way in**,
    /// from `Enter` in the picker and from a click on a row.
    fn choose_add_target(&mut self, index: usize) {
        let Some(track_id) = self.add_track else {
            return;
        };
        let Some(playlist) = self.playlists.get(index) else {
            return;
        };
        let (uuid, title) = (playlist.uuid.clone(), playlist.title.clone());
        self.add_track = None;
        self.mode = Mode::Normal;
        self.ask(ToWorker::AddToPlaylist {
            uuid,
            title,
            track_id,
        });
    }

    fn add_down(&mut self, by: usize) {
        let last = self.playlists.len().saturating_sub(1);
        self.add_selected = self.add_selected.saturating_add(by).min(last);
    }

    fn add_up(&mut self, by: usize) {
        self.add_selected = self.add_selected.saturating_sub(by);
    }

    /// Which picker row is highlighted, for the renderer.
    #[must_use]
    pub fn add_selected_row(&self) -> usize {
        self.add_selected
    }

    /// Resolve a click in the playlist picker.
    ///
    /// A click that lands on no row closes the picker, the way the output and
    /// theme pickers do. Safe here for the same reason it is safe there: the
    /// picker adds, it does not remove, and closing it does nothing.
    fn click_add_target(&mut self, col: u16, row: u16) {
        match self
            .add_rows
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, i)| *i)
        {
            Some(index) => {
                self.add_selected = index;
                self.choose_add_target(index);
            }
            None => self.cancel_modal(),
        }
    }

    /// A playlist now exists that did not before.
    ///
    /// Put at the top of the list rather than left for a reload to find. The
    /// listing is ordered newest first, so that is where the service would put
    /// it too, and a reload would throw away a filter the listener had typed
    /// and their place in a list they had scrolled.
    fn on_playlist_created(&mut self, made: Playlist) {
        self.notice = Some(format!("Created “{}”.", made.title));
        self.playlists.insert(0, made);
        // The listing is one longer than the service said it was when the page
        // arrived. Left alone, paging would ask for the last row twice.
        self.playlists_paging.total = self.playlists_paging.total.saturating_add(1);
        if self.view == View::Playlists {
            self.clamp_selection();
        }
    }

    /// A playlist is gone, and the service has said so.
    ///
    /// Only now does the row leave the screen. If the listener is inside the
    /// playlist that was deleted they are taken back out of it, because the
    /// view they are in no longer stands for anything.
    fn on_playlist_deleted(&mut self, uuid: &str) {
        self.playlists.retain(|p| p.uuid != uuid);
        self.playlists_paging.total = self.playlists_paging.total.saturating_sub(1);
        if self.open_playlist.as_ref().is_some_and(|(u, _)| u == uuid) {
            self.open_playlist = None;
            self.playlist_tracks.clear();
            if self.view == View::PlaylistTracks {
                self.switch_view(View::Playlists);
            }
        }
        self.notice = Some("Deleted.".into());
        self.clamp_selection();
    }

    /// A track is out of a playlist, and the service has said so.
    ///
    /// Guarded on the playlist still being the open one: a removal that lands
    /// after the listener has moved to another playlist must not take a row out
    /// of *that* one.
    fn on_playlist_track_removed(&mut self, uuid: &str, track_id: u64) {
        if self.open_playlist.as_ref().is_none_or(|(u, _)| u != uuid) {
            return;
        }
        self.playlist_tracks.retain(|t| t.id != track_id);
        self.playlist_tracks_paging.total = self.playlist_tracks_paging.total.saturating_sub(1);
        self.notice = Some("Removed from the playlist.".into());
        self.clamp_selection();
    }

    fn play_selected(&mut self) {
        self.start_queue_at(self.selected);
    }

    fn user_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        // The entry mpv already has is the right answer unless it is this same
        // track again: a skip out of repeat-one has to move on, so the preloaded
        // repeat is passed over and the next track is loaded outright.
        if self.status.has_next && self.repeat != Repeat::One {
            self.player.skip_next();
        } else if let Some(p) = self.next_pos(self.repeat.skipped()) {
            self.load_fresh(p);
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
        // Back up the play order, which is the row the panel draws above this
        // one. One rule under both orders: the shuffle used to answer this with
        // another random pick, so "previous" was a track that had not played.
        let p = self
            .queue_at(self.playing_row().saturating_sub(1))
            .unwrap_or(self.queue_pos);
        self.load_fresh(p);
    }

    fn toggle_shuffle(&mut self) {
        self.set_shuffle(!self.shuffle);
    }

    /// Will the radio actually follow this queue?
    ///
    /// [`Self::continue_radio`] says the listener asked for it; this says
    /// whether it can happen at all. A repeating queue reaches no end, so it
    /// cannot - and the control then paints itself off rather than claiming
    /// something that will not happen. One place that reads both flags, so the
    /// control and the notice cannot come to different answers, and it writes
    /// neither: the toggles stay independent.
    #[must_use]
    pub fn radio_follows(&self) -> bool {
        self.continue_radio && self.repeat == Repeat::Off
    }

    /// Turn carrying on past the end of the queue on, or off.
    ///
    /// The one implementation behind the `c` key and the header control, so the
    /// two cannot drift. Turning it off governs whether the queue is *extended*
    /// again; it does not reach into what is already loaded, because the next
    /// entry is in mpv's playlist by then and a second path into that is the
    /// one thing the gapless pipeline must not grow.
    fn toggle_continue(&mut self) {
        self.continue_radio = !self.continue_radio;
        // Asking again is the point of turning it back on, so the record of
        // what was already asked goes with it.
        self.radio_asked = None;
        self.notice = Some(
            match (self.continue_radio, self.repeat) {
                // A read of the other toggle, never a write to it: turning this
                // on under a repeat is the one moment the listener would
                // otherwise be told something that will not happen.
                (true, Repeat::All | Repeat::One) => {
                    "Radio ON, but the repeat comes first: this queue has no end."
                }
                (true, Repeat::Off) => {
                    "Radio ON: the queue carries on with the service's suggestions."
                }
                (false, _) => "Radio OFF: the queue ends where it ends.",
            }
            .into(),
        );
    }

    /// Move on to the next of the three repeat states.
    ///
    /// The one implementation behind the `e` key and the header control, which
    /// is what stops the two drifting. It cycles where the desktop sets, in
    /// exactly the way `toggle_shuffle` cycles where `set_shuffle` sets.
    fn cycle_repeat(&mut self) {
        self.set_repeat(self.repeat.next());
    }

    /// Say how the queue repeats.
    ///
    /// Absolute rather than a cycle because MPRIS's `LoopStatus` is: one
    /// implementation with three callers rather than three implementations - the
    /// key and the control ask for the state after this one, and the desktop
    /// asks for the state it wants.
    ///
    /// **It does not reach into what mpv already has.** The next entry is in
    /// mpv's playlist by the time this is pressed, and a second path into that
    /// playlist is the one thing the gapless pipeline must not grow - the same
    /// reasoning [`Self::toggle_continue`] records. A change is answered by the
    /// next preload decision, which is the only path there is.
    ///
    /// It writes nothing but its own state, and `continue_radio` least of all:
    /// a repeating queue reaches no end, so the radio is never asked, and that
    /// falls out of the queue rather than out of one toggle editing another.
    fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
        self.notice = Some(
            match repeat {
                Repeat::Off => "Repeat OFF: the queue ends where it ends.",
                Repeat::All => "Repeat ALL: the queue starts again at the top.",
                Repeat::One => "Repeat ONE: this track plays again.",
            }
            .into(),
        );
    }

    /// Turn shuffle on, or off.
    ///
    /// Absolute rather than a toggle because MPRIS's `Shuffle` is, and there is
    /// one implementation with two callers rather than two implementations: the
    /// `s` key asks for the opposite of what is in force, and the desktop asks
    /// for what it wants.
    ///
    /// **Both ways lay a play order over the queue, and neither touches the
    /// queue.** Turning it off is a return to the listing order with the track
    /// still playing, which is only possible because the listing order was
    /// never overwritten; turning it on again deals the rows that have not
    /// played yet, and leaves what is behind the listener behind them.
    fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.notice = Some(
            if self.shuffle {
                "Shuffle ON"
            } else {
                "Shuffle OFF"
            }
            .into(),
        );
        if self.shuffle {
            self.deal_order_from(self.playing_row() + 1);
        } else {
            self.lay_listing_order();
        }
        // The track playing has a new row, and the cursor is a readout of where
        // the music is until somebody starts driving it.
        self.follow_queue_cursor();
        if self.shuffle && self.now_playing.is_none() {
            // Build a queue from the current track view and start at the head
            // of a freshly dealt order - which is where the randomness lives
            // now that there is an order to be random in.
            let tracks: Vec<Track> = {
                let vis = self.visible();
                let items = self.current_tracks();
                vis.iter().filter_map(|&i| items.get(i).cloned()).collect()
            };
            if !tracks.is_empty() {
                self.set_queue(tracks);
                self.deal_order_from(0);
                if let Some(p) = self.queue_at(0) {
                    self.load_fresh(p);
                }
                self.begin_queue_fill();
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
            Mode::SetupAudio => self.on_key_setup(key),
            Mode::Devices => self.on_key_devices(key),
            Mode::Themes => self.on_key_themes(key),
            Mode::Credentials => self.on_key_credentials(key),
            Mode::Login => self.on_key_login(key),
            Mode::Prompt => self.on_key_prompt(key),
            Mode::Confirm => self.on_key_confirm(key),
            Mode::AddTo => self.on_key_add_to(key),
            Mode::Normal => self.on_key_normal(key),
        }
    }

    /// The help overlay is modal: anything that reads as "done" dismisses it, and
    /// nothing else leaks through to the list underneath.
    /// Show the reference, from the top. **The one way in.**
    fn open_help(&mut self) {
        self.mode = Mode::Help;
        self.help_scroll = 0;
    }

    /// Keys while the reference is up.
    ///
    /// It scrolls, with the same j/k and g/G the log overlay uses. That is not
    /// decoration: the reference is how bindings are discovered, and it now
    /// holds more rows than a 24-row terminal can show. Clipping it would
    /// silently delete the keys at the bottom, which is the bug that once lost
    /// `[q]` off the end of the bottom row.
    fn on_key_help(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q' | ' ') => {
                self.mode = Mode::Normal;
                self.help_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.help_scroll = self.help_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::Char('g') => self.help_scroll = 0,
            // The renderer knows how far down the content actually goes and
            // clamps this on the next frame, the way it clamps `device_offset`.
            KeyCode::Char('G') => self.help_scroll = usize::MAX,
            _ => {}
        }
        self.dirty = true;
    }

    /// Open the recent diagnostics.
    ///
    /// The one way in: `M` and the `M` control in the reference overlay both
    /// come through here, so the two cannot drift apart. Always opens on the
    /// newest line - the reason for opening this is almost always something that
    /// just happened.
    fn open_log(&mut self) {
        self.mode = Mode::Log;
        self.log_scroll = 0;
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

    /// The whole chain's verdict: what is happening to the samples, and what
    /// that rests on.
    ///
    /// The one place the row and the report both read, so the word on the row
    /// and the section headed `Verdict` cannot come to different conclusions.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.status
            .verdict(self.now_meta.bit_depth, &self.sink_volume)
    }

    /// Take the sink's volume out of a chain reading, whatever it turned out to
    /// be.
    ///
    /// Each arm is a different statement and none may stand in for another. A
    /// bypassed graph is a chain with no server sink on it at all, which counts
    /// as fully evidenced; any other failure read nothing, which does not.
    fn note_sink_volume(&mut self, read: &Result<AudioGraph, GraphError>) {
        self.sink_volume = match read {
            Ok(g) => g.volume.clone(),
            Err(GraphError::Bypassed) => SinkVolume::Absent,
            Err(_) => SinkVolume::Unread,
        };
    }

    /// Keep the sink's volume current enough for the row to be graded on it.
    ///
    /// Reading it means running `pw-dump` and waiting for it, so the request
    /// goes to the worker like every other one and is spaced out rather than
    /// made on every tick. Nothing is asked for while nothing is playing, and
    /// nothing is asked for on the direct path - there is no server sink there
    /// to have a volume, which is a reading in itself.
    fn poll_sink_volume(&mut self) {
        if self.status.bypasses_sound_server() {
            self.sink_volume = SinkVolume::Absent;
            return;
        }
        if !self.status.loaded {
            return;
        }
        if self
            .sink_volume_asked
            .is_some_and(|t| t.elapsed() < SINK_VOLUME_INTERVAL)
        {
            return;
        }
        self.sink_volume_asked = Some(Instant::now());
        let _ = self.worker.tx.send(ToWorker::ReadAudioGraph);
    }

    /// Ask the worker for the playing track's cover, once per track.
    ///
    /// The trigger for the whole feature, and a poll rather than a hook on each
    /// place `now_playing` is set: a track can begin from a queue advance, from
    /// mpv adopting one on its own, or from a direct play, and one poll catches
    /// all three where three hooks would be three chances to miss one.
    ///
    /// Asked at most once per track: `cover_asked` records the id, so a request
    /// in flight or one that came back empty is not re-sent every tick. A track
    /// whose listing named no cover is never asked - there is nothing to fetch -
    /// and the request never leaves this thread with a blocking call on it,
    /// because the fetch and the decode are the worker's.
    fn poll_cover(&mut self) {
        let Some(track) = self.now_playing.as_ref() else {
            return;
        };
        if self.cover_asked == Some(track.id) || track.cover.is_empty() {
            return;
        }
        self.cover_asked = Some(track.id);
        let _ = self.worker.tx.send(ToWorker::FetchCover {
            track_id: track.id,
            cover_id: track.cover.clone(),
        });
    }

    /// Take a decoded cover the worker sent, if its track is still the one
    /// playing.
    ///
    /// Dropped rather than stored when the track has moved on: a cover that
    /// finished decoding just after its track stopped would otherwise be drawn
    /// over the one now playing. `cover_for_now_playing` asks the same question
    /// at draw time, so this is belt and braces - but storing a stale cover at
    /// all is worth avoiding, because it would sit in memory keyed to a track
    /// that will not come back.
    fn on_cover(&mut self, track_id: u64, image: crate::art::Image) {
        if self.now_playing.as_ref().is_some_and(|t| t.id == track_id) {
            self.cover = Some((track_id, image));
        }
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
            let bypassed = Err(GraphError::Bypassed);
            self.note_sink_volume(&bypassed);
            self.audio_graph = Some(bypassed);
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
            // Offer to permit the rates the device can do that the server is
            // blocking. Does nothing when there are none, exactly as the footer
            // key that fires it is only shown when there are.
            KeyCode::Char('A') => self.begin_setup(),
            _ => {}
        }
        self.dirty = true;
    }

    fn graph_scroll_max(&self) -> usize {
        self.graph_rows().len().saturating_sub(1)
    }

    /// The rates the device can do that the server is not set to use, and the
    /// whole list a drop-in would permit, from the graph that is already read.
    ///
    /// `None` when there is no graph or nothing is blocked - the one case the
    /// "set up audio" offer is not made, so the key and the footer that reach it
    /// answer with nothing rather than an empty file.
    fn setup_targets(&self) -> Option<(Vec<u32>, Vec<u32>)> {
        let Some(Ok(graph)) = &self.audio_graph else {
            return None;
        };
        let blocked = graph.blocked_supported_hz();
        if blocked.is_empty() {
            return None;
        }
        let permitted = graph.clock.permitted_hz().unwrap_or_default();
        let allowed = priel_player::setup::desired_allowed_hz(&permitted, &blocked);
        Some((blocked, allowed))
    }

    /// Whether the "set up audio" offer applies right now.
    ///
    /// What the graph overlay's footer key hangs on, so the offer and the action
    /// behind it agree about when there is anything to do.
    #[must_use]
    pub fn setup_available(&self) -> bool {
        self.setup_targets().is_some()
    }

    /// The "set up audio" flow while its overlay is up, for the renderer to draw.
    #[must_use]
    pub(crate) fn setup(&self) -> Option<&Setup> {
        self.setup.as_ref()
    }

    /// Open the "set up audio" preview, or do nothing when there is nothing to
    /// add. The rates are settled here, once, from the graph already read.
    fn begin_setup(&mut self) {
        let Some((adding_hz, allowed_hz)) = self.setup_targets() else {
            return;
        };
        self.setup = Some(Setup {
            adding_hz,
            allowed_hz,
            step: SetupStep::Confirm,
        });
        self.mode = Mode::SetupAudio;
        self.dirty = true;
    }

    /// The "set up audio" overlay: a preview to approve, then the write, then the
    /// restart that makes it take effect - each step advanced by one key, and the
    /// two with a request in flight swallowing keys until the reply lands.
    fn on_key_setup(&mut self, key: KeyEvent) {
        let Some(setup) = &self.setup else {
            self.mode = Mode::Normal;
            return;
        };
        match &setup.step {
            SetupStep::Confirm => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    let allowed_hz = setup.allowed_hz.clone();
                    self.setup_step(SetupStep::Writing);
                    self.ask(ToWorker::SetUpAudio { allowed_hz });
                }
                KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
                    // Back to the report it was opened from, not out of it: the
                    // reading is still there, so nothing needs reading again.
                    self.setup = None;
                    self.mode = Mode::Graph;
                }
                _ => {}
            },
            SetupStep::Restart { path } => match key.code {
                KeyCode::Char('r' | 'R') => {
                    self.setup_step(SetupStep::Restarting);
                    self.ask(ToWorker::RestartAudio);
                }
                KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc | KeyCode::Enter => {
                    self.notice = Some(format!(
                        "Written to {path}. Restart PipeWire to use the new rates."
                    ));
                    self.setup = None;
                    self.mode = Mode::Normal;
                }
                _ => {}
            },
            SetupStep::Done { .. } => {
                self.setup = None;
                self.mode = Mode::Normal;
            }
            // A request is in flight; the overlay says so and the keys wait.
            SetupStep::Writing | SetupStep::Restarting => {}
        }
        self.dirty = true;
    }

    /// Move the flow to `step`, if it is still up.
    fn setup_step(&mut self, step: SetupStep) {
        if let Some(setup) = &mut self.setup {
            setup.step = step;
        }
    }

    /// The drop-in was written, or not. Only a flow still waiting on the write
    /// moves - a reply that outlived its overlay is dropped, as everywhere here.
    fn on_audio_set_up(&mut self, result: Result<String, String>) {
        if !matches!(
            &self.setup,
            Some(Setup {
                step: SetupStep::Writing,
                ..
            })
        ) {
            return;
        }
        self.setup_step(match result {
            Ok(path) => SetupStep::Restart { path },
            Err(e) => SetupStep::Done {
                message: format!("Could not write it: {e}"),
            },
        });
        self.dirty = true;
    }

    /// The sound server was restarted, or not. Only a flow still waiting on the
    /// restart moves.
    fn on_audio_restarted(&mut self, result: Result<(), String>) {
        if !matches!(
            &self.setup,
            Some(Setup {
                step: SetupStep::Restarting,
                ..
            })
        ) {
            return;
        }
        self.setup_step(match result {
            Ok(()) => SetupStep::Done {
                message: "Done. PipeWire restarted; the new rates are live.".to_string(),
            },
            Err(e) => SetupStep::Done {
                message: format!(
                    "Written, but the restart failed: {e}. Restart PipeWire yourself."
                ),
            },
        });
        self.dirty = true;
    }

    /// How far down the audio-graph overlay is scrolled.
    #[must_use]
    pub fn graph_offset(&self) -> usize {
        self.graph_scroll
    }

    /// The output report, top to bottom.
    ///
    /// **Every section renders what it can, independently of the rest.** This
    /// used to short-circuit on the first thing that could not be read, so a
    /// direct output - which has no graph by design - produced two sentences
    /// and nothing else, hiding the volume stages from the listeners with the
    /// cleanest chain on the machine and the strongest reason to check it.
    #[must_use]
    pub fn graph_rows(&self) -> Vec<GraphRow> {
        let source = self.status.decoded_format(self.now_meta.bit_depth);
        let mut rows = self.verdict_rows();
        rows.extend(self.device_rows());
        rows.extend(self.volume_rows());
        rows.extend(self.chain_rows(source));
        match &self.audio_graph {
            Some(Ok(g)) => {
                // After the chain, because it is the answer to it: a rate the
                // server was never permitted to use is refused before any node
                // on the path sees a sample, which is how the chain can diverge
                // nowhere and something still move.
                rows.extend(clock_rows(&g.clock, &g.supported_hz, source));
                // Last, because it is the one section that is true whatever the
                // rest of them found: a chain that alters nothing is still a
                // chain the sound server owns and can reshape when the next
                // application starts.
                rows.extend(holder_rows(&g.holder));
            }
            // The one failure that is not a failure: there is no graph because
            // priel has the device itself. The player knows which device it is
            // holding, so this section is told rather than left to infer it.
            Some(Err(GraphError::Bypassed)) => rows.extend(holder_rows(&DeviceHolder::Direct {
                device: self.status.audio_device.clone(),
            })),
            None | Some(Err(_)) => {}
        }
        rows
    }

    /// The grade, and what it rests on. The same one the row carries.
    ///
    /// Drawn as plain prose rather than in the grade's colour, because the
    /// glyph in front of the word is what carries the meaning - on a light
    /// theme, a dark one, a monochrome terminal, and to the red/green
    /// deficiency these grades already lean on.
    /// Whether the active Bluetooth codec is the best the device offers.
    ///
    /// From the active codec (the status) and the available ones (the last-read
    /// graph). `None` when the output is not Bluetooth, or when the available
    /// codecs are not known - in which case priel cannot call the active one
    /// optimal, and the verdict treats it as improvable rather than claim it is
    /// best. Ranks by [`crate::ui::codec_rank`]; a tie counts as best.
    pub(crate) fn bt_codec_standing(&self) -> Option<BtStanding> {
        let active = self.status.bt_codec.as_deref()?;
        let Some(Ok(graph)) = &self.audio_graph else {
            return None;
        };
        let best = graph
            .bt_available
            .iter()
            .max_by_key(|p| crate::ui::codec_rank(&p.codec))?;
        if crate::ui::codec_rank(active) >= crate::ui::codec_rank(&best.codec) {
            Some(BtStanding::Best)
        } else {
            Some(BtStanding::Improvable {
                better: best.codec.clone(),
            })
        }
    }

    /// The Bluetooth standing as the verdict glyph and colour read it:
    /// `Some(false)` best, `Some(true)` a better codec available, `None` not
    /// applicable or unknown.
    pub(crate) fn bt_improvable(&self) -> Option<bool> {
        self.bt_codec_standing()
            .map(|s| matches!(s, BtStanding::Improvable { .. }))
    }

    fn verdict_rows(&self) -> Vec<GraphRow> {
        // `verdict_words` answers with an empty string when there is nothing to
        // grade, and on the bottom row that is exactly right - it is what
        // suppresses the badge while nothing is playing. A *heading* with
        // nothing under it is a different statement: it reads as a section that
        // failed to load. So the report says what the silence means, the same
        // way `access_words` already does, and the shared function is left
        // alone rather than taught to answer two questions at once.
        let words = crate::ui::verdict_words(
            self.verdict(),
            self.status.bt_codec.as_deref(),
            self.bt_improvable(),
        );
        let words = if words.is_empty() {
            "nothing playing".to_string()
        } else {
            words
        };
        vec![reading("  Verdict", words)]
    }

    /// What is being played into, and how it is being held.
    ///
    /// Access lives here rather than on the bottom row: it is a session-long
    /// setting rather than something that changes per track, and the row it
    /// used to sit on had no width left to spare.
    fn device_rows(&self) -> Vec<GraphRow> {
        let mut rows = vec![note(""), note("  Device")];
        rows.push(reading(
            "    output",
            crate::ui::device_readout(&self.status),
        ));
        rows.push(reading("    access", crate::ui::access_words(&self.status)));
        if let Some(codec) = self.status.bt_codec.as_deref() {
            // A Bluetooth output: name the link codec, which is what its sound
            // quality rests on and why the verdict cannot be bit-perfect.
            rows.push(reading("    codec", crate::ui::codec_label(codec)));
            // And whether a better one is on offer: the one thing a listener can
            // do for a Bluetooth link's fidelity. Silent when already on the
            // best, or when the available codecs could not be read.
            if let Some(BtStanding::Improvable { better }) = self.bt_codec_standing() {
                rows.push(reading(
                    "    available",
                    format!(
                        "{} — a higher-quality codec",
                        crate::ui::codec_label(&better)
                    ),
                ));
            }
        }
        rows
    }

    /// Every stage that can change the level, in the order the samples meet
    /// them.
    ///
    /// All three are always listed, including the ones that are absent and the
    /// ones that could not be read - a stage missing from this list would read
    /// as a stage at unity, which is the guess this whole section exists to
    /// stop.
    fn volume_rows(&self) -> Vec<GraphRow> {
        vec![
            note(""),
            note("  Volume"),
            reading("    priel", crate::ui::own_volume_words(self.status.volume)),
            reading("    stream", crate::ui::stream_volume_words(&self.status)),
            reading("    sink", crate::ui::sink_volume_words(&self.sink_volume)),
        ]
        .into_iter()
        .chain(
            crate::ui::sink_volume_note(&self.sink_volume)
                .map(|words| reading("    applied", words)),
        )
        .collect()
    }

    /// What sits between priel and the device, or why there is nothing to draw.
    fn chain_rows(&self, source: SourceFormat) -> Vec<GraphRow> {
        let mut rows = vec![note(""), note("  Chain")];
        match &self.audio_graph {
            None => rows.push(note("    Reading the graph…")),
            Some(Err(e)) => {
                rows.push(note(&format!("    {e}")));
                rows.extend(e.hint().map(|hint| note(&format!("    {hint}"))));
            }
            Some(Ok(g)) => {
                // The same reading the row is graded from, so the report and
                // the verdict above it cannot disagree about one track.
                let observed = self.status.fidelity(self.now_meta.bit_depth).alteration();
                let blame = g.attribute(source, observed);
                rows.extend(path_rows(g, blame));
                rows.extend(blame_row(g, blame, observed));
            }
        }
        rows
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
    /// The one way in: `d` and the header's `◎` control both come through here,
    /// so the two cannot drift apart.
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
        self.chosen.exclusive = Some(self.exclusive);
        self.notice = Some(if self.exclusive {
            "Asking for the device exclusively — kept for next time; --shared overrides it"
                .to_string()
        } else {
            "Sharing the output device again — kept for next time; --exclusive overrides it"
                .to_string()
        });
        self.dirty = true;
    }

    /// What the pickers chose this session, for `main` to write out.
    ///
    /// Empty unless a picker was actually used: a value that came from a flag is
    /// for that run alone, and quietly making one permanent is the surprise the
    /// settings file is designed to avoid.
    #[must_use]
    pub fn chosen(&self) -> &Settings {
        &self.chosen
    }

    /// Has exclusive use of the device been asked for?
    ///
    /// What was *asked*, not what was got: the player answers the second
    /// through `status.access`, and only that may reach the badge.
    #[must_use]
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    /// Move the output to the device on this row, and remember it.
    ///
    /// The one place that changes the output: the Enter key and a click on a
    /// row both arrive here, so the two cannot drift apart. What is remembered
    /// is the **identifier**, never the description: only the first is what
    /// `--device` and the settings file accept. Recorded rather than written,
    /// for the reason `choose_theme` gives.
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
        self.chosen.device = Some(name);
        self.notice = Some(format!("Output: {label} — kept for next time"));
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
                // Vim's own window key. Taken rather than invented because a
                // VIM-first client should spend a letter only where vim has
                // nothing to say, and `Tab` - the other obvious candidate - is
                // already the view cycle.
                KeyCode::Char('w') => self.cycle_focus(),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.go_back(),
            KeyCode::Tab => self.cycle_view(),
            KeyCode::Char('1') => self.switch_view(View::Favorites),
            KeyCode::Char('2') => self.switch_view(View::Playlists),
            KeyCode::Char('3') => self.switch_view(View::Search),
            KeyCode::Char('4') => self.switch_view(View::Mixes),
            KeyCode::Char('i') => self.edit_search(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_up(1),
            KeyCode::Char('J') => self.move_down(self.full_page()),
            KeyCode::Char('K') => self.move_up(self.full_page()),
            KeyCode::Char('g') => self.goto_top(),
            KeyCode::Char('G') => self.goto_bottom(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('M') => self.open_log(),
            KeyCode::Char('D') => self.open_graph(),
            KeyCode::Char('d') => self.open_devices(),
            KeyCode::Char('t') => self.open_themes(),
            KeyCode::Char('A') => self.start_login(),
            KeyCode::Enter => self.on_enter(),
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('c') => self.toggle_continue(),
            KeyCode::Char('e') => self.cycle_repeat(),
            KeyCode::Char('n' | 'L') => self.user_next(),
            KeyCode::Char('p' | 'H') => self.user_prev(),
            KeyCode::Char('h') | KeyCode::Left => self.player.seek_relative(-5.0),
            KeyCode::Char('l') | KeyCode::Right => self.player.seek_relative(5.0),
            KeyCode::Char('+' | '=') => self.volume_step(5.0),
            KeyCode::Char('-') => self.volume_step(-5.0),
            KeyCode::Char('0') => self.volume_unity(),
            KeyCode::Char('/') => self.start_filter(),
            KeyCode::Char('f') => self.favorite_selected(),
            KeyCode::Char('F') => self.favorite_now_playing(),
            KeyCode::Char('r') => self.reload_view(),
            KeyCode::Char('a') => self.add_selected_to_playlist(),
            KeyCode::Char('N') => self.new_playlist(),
            KeyCode::Char('R') => self.rename_selected_playlist(),
            KeyCode::Char('X') => self.remove_selected(),
            // The window key's shifted sibling: `Ctrl-W` moves the keyboard
            // between the two regions and this one takes the second region
            // away, so the pair names the same thing rather than spending a
            // second idiom on it.
            KeyCode::Char('W') => self.toggle_queue_column(),
            KeyCode::Char('C') => self.toggle_cover(),
            _ => {}
        }
    }

    /// Keys while a playlist name is being typed.
    ///
    /// The same shape as the filter and the search box, so there is one idiom
    /// for a line of text rather than three. `Ctrl-U` clears it, as it does in
    /// the paste box.
    fn on_key_prompt(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('u') {
                self.prompt_text.clear();
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Backspace => {
                self.prompt_text.pop();
            }
            // Bounded, like everything else built from what arrives from
            // outside. A held key would otherwise grow this without end.
            KeyCode::Char(c) if self.prompt_text.chars().count() < PLAYLIST_NAME_MAX => {
                self.prompt_text.push(c);
            }
            _ => {}
        }
    }

    /// Keys while a confirmation is up.
    ///
    /// **`Enter` is not yes.** Every other modal in priel accepts on `Enter`,
    /// and that is exactly why this one must not: a listener who has just
    /// pressed `Enter` to open a playlist and pressed it again out of rhythm
    /// would delete it. Going through with it takes the one key that means
    /// nothing else here, and everything unrecognised is ignored rather than
    /// treated as either answer.
    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') => self.confirm_yes(),
            KeyCode::Char('n' | 'q') | KeyCode::Esc => self.cancel_modal(),
            _ => {}
        }
    }

    /// Keys while a playlist is being chosen for a track.
    ///
    /// The picker idiom, unchanged from the output and theme pickers: `j`/`k`
    /// and `g`/`G` move, `Enter` chooses, `Esc` backs out. A second idiom for
    /// the same gesture would be its own bug.
    fn on_key_add_to(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.cancel_modal(),
            KeyCode::Char('j') | KeyCode::Down => self.add_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.add_up(1),
            KeyCode::Char('J') => self.add_down(self.full_page()),
            KeyCode::Char('K') => self.add_up(self.full_page()),
            KeyCode::Char('g') => self.add_selected = 0,
            KeyCode::Char('G') => self.add_selected = self.playlists.len().saturating_sub(1),
            KeyCode::Enter => self.choose_add_target(self.add_selected),
            _ => {}
        }
        self.dirty = true;
    }

    /// Answer a click while a modal overlay is up. `true` if it was handled.
    ///
    /// Each overlay owns the pointer while it is on screen: nothing behind one
    /// may be reached through it, and the two that ask a question answer only
    /// where their own controls were painted.
    /// A click on a key an overlay printed in its footer, if that is what it is.
    ///
    /// The bottom row's rule - every key printed is itself the button - applied
    /// to the overlays. Asked before anything else those overlays do with a
    /// click, because two of them answer *any* click by closing: before this,
    /// clicking `j k scroll` in the report closed the report.
    ///
    /// Only these five. The consent, sign-in, prompt and confirm screens already
    /// route every click through their own hit boxes, and asking twice would
    /// dispatch one click as two.
    fn footer_key_clicked(&mut self, m: MouseEvent) -> bool {
        if !matches!(
            self.mode,
            Mode::Log | Mode::Graph | Mode::SetupAudio | Mode::Devices | Mode::Themes | Mode::AddTo
        ) || !matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
        {
            return false;
        }
        let Some(h @ Hit::Key(_)) = self.hit_at(m.column, m.row) else {
            return false;
        };
        self.dispatch(h);
        self.dirty = true;
        true
    }

    fn on_mouse_overlay(&mut self, m: MouseEvent) -> bool {
        if self.footer_key_clicked(m) {
            return true;
        }
        // The picker answers like the other two: a row chooses, anything else
        // closes it. Adding a track to a playlist takes nothing away, so a
        // click that misses can safely mean "never mind".
        if self.mode == Mode::AddTo {
            match m.kind {
                MouseEventKind::ScrollDown => self.add_down(1),
                MouseEventKind::ScrollUp => self.add_up(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    self.click_add_target(m.column, m.row);
                }
                _ => {}
            }
            self.dirty = true;
            return true;
        }
        // The prompt and the confirmation answer only where their own controls
        // are, which is the consent screen's rule and is here for a stronger
        // reason: one of these controls deletes a playlist, and a click that
        // landed anywhere near it must not be read as an answer. Scrolling and
        // dragging are swallowed too - neither means yes.
        if matches!(
            self.mode,
            Mode::Credentials | Mode::Login | Mode::Prompt | Mode::Confirm
        ) {
            // These two offer controls rather than a way out, and are the one
            // place a click off a control means nothing at all: a stray click is
            // not consent to download a credential, and not an abandoned
            // sign-in. Both screens replace the frame's hit boxes with their
            // own, so nothing behind them can be reached from here either.
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
                && let Some(h) = self.hit_at(m.column, m.row)
            {
                self.dispatch(h);
                self.dirty = true;
            }
            return true;
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
            return true;
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
            return true;
        }
        if self.mode == Mode::Devices {
            match m.kind {
                MouseEventKind::ScrollDown => self.device_down(1),
                MouseEventKind::ScrollUp => self.device_up(1),
                MouseEventKind::Down(MouseButton::Left) => self.click_device(m.column, m.row),
                _ => return true,
            }
            self.dirty = true;
            return true;
        }
        if self.mode == Mode::Themes {
            let last = theme::OFFERED.len().saturating_sub(1);
            match m.kind {
                MouseEventKind::ScrollDown => {
                    self.theme_selected = (self.theme_selected + 1).min(last);
                }
                MouseEventKind::ScrollUp => {
                    self.theme_selected = self.theme_selected.saturating_sub(1);
                }
                MouseEventKind::Down(MouseButton::Left) => self.click_theme(m.column, m.row),
                _ => return true,
            }
            self.dirty = true;
            return true;
        }
        if self.mode == Mode::Help {
            // The reference is priel's menu: every key it lists is a control, so
            // a click that lands on one runs it and anything else just
            // dismisses. Either way the overlay closes, and the mode is put back
            // *before* dispatching so an action that opens another overlay is
            // not closed again by its own click. The renderer replaces the hit
            // boxes with the overlay's own while it is up, so nothing behind it
            // can be reached from here. Scrolling the list behind it would be
            // odd, so only a click is answered.
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                let landed = self.hit_at(m.column, m.row);
                self.mode = Mode::Normal;
                if let Some(h) = landed {
                    self.dispatch(h);
                }
                self.dirty = true;
            }
            return true;
        }
        // The setup overlay routes only through its footer keys (handled at the
        // top); every other click on it is swallowed here, so a misclick cannot
        // act or reach the list behind the modal.
        matches!(self.mode, Mode::SetupAudio)
    }

    /// A mouse event, routed by what is on screen.
    ///
    /// Split in two because it had grown past being readable in one piece: an
    /// overlay that is up owns the pointer entirely, so that half answers first
    /// and says whether it did.
    pub fn on_mouse(&mut self, m: MouseEvent) {
        if self.on_mouse_overlay(m) {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown => self.wheel(m.column, m.row, 1),
            MouseEventKind::ScrollUp => self.wheel(m.column, m.row, 0),
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

    /// The wheel, aimed at whichever list is under the pointer.
    ///
    /// It gives that list the keyboard for the same reason a click does: a
    /// gesture aimed at a region is a request for that region, and a wheel that
    /// scrolled the *other* list would be the one mouse action on screen whose
    /// answer depends on something the pointer is not touching.
    fn wheel(&mut self, col: u16, row: u16, down: u8) {
        if hit(self.queue_inner, col, row) {
            self.give_focus(Focus::Queue);
        } else {
            self.give_focus(Focus::List);
        }
        if down > 0 {
            self.move_down(1);
        } else {
            self.move_up(1);
        }
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
            // Straight back through the front door: whatever the key does in
            // whatever mode is up, this does, because it is that key.
            Hit::Key(code) => self.on_key(KeyEvent::new(code, KeyModifiers::NONE)),
            Hit::MoveUp => self.move_up(1),
            Hit::MoveDown => self.move_down(1),
            Hit::HalfPageUp => self.move_up(self.half_page()),
            Hit::HalfPageDown => self.move_down(self.half_page()),
            Hit::PageUp => self.move_up(self.full_page()),
            Hit::PageDown => self.move_down(self.full_page()),
            Hit::Top => self.goto_top(),
            Hit::Bottom => self.goto_bottom(),
            Hit::Enter => self.on_enter(),
            Hit::Back => self.go_back(),
            Hit::Shuffle => self.toggle_shuffle(),
            Hit::Repeat => self.cycle_repeat(),
            Hit::Continue => self.toggle_continue(),
            Hit::VolUp => self.volume_step(5.0),
            Hit::VolDown => self.volume_step(-5.0),
            Hit::VolUnity => self.volume_unity(),
            Hit::Filter => self.start_filter(),
            Hit::FavoriteSelected => self.favorite_selected(),
            Hit::FavoriteNowPlaying => self.favorite_now_playing(),
            Hit::EditSearch => self.edit_search(),
            Hit::Reload => self.reload_view(),
            Hit::CycleView => self.cycle_view(),
            Hit::CycleFocus => self.cycle_focus(),
            Hit::QueueColumn => self.toggle_queue_column(),
            Hit::CoverArt => self.toggle_cover(),
            Hit::Help => self.open_help(),
            Hit::Log => self.open_log(),
            Hit::Graph => self.open_graph(),
            Hit::Devices => self.open_devices(),
            Hit::Themes => self.open_themes(),
            Hit::SignIn => self.start_login(),
            Hit::FetchCredentials => self.fetch_credentials(),
            Hit::DeclineCredentials => self.decline_credentials(),
            Hit::SubmitLogin => self.submit_login(),
            Hit::ReopenBrowser => self.reopen_browser(),
            Hit::ClearPaste => self.clear_paste(),
            Hit::CancelLogin => self.cancel_login(),
            Hit::NewPlaylist => self.new_playlist(),
            Hit::RenamePlaylist => self.rename_selected_playlist(),
            Hit::RemoveSelected => self.remove_selected(),
            Hit::AddToPlaylist => self.add_selected_to_playlist(),
            Hit::SubmitPrompt => self.submit_prompt(),
            Hit::CancelPrompt | Hit::ConfirmNo => self.cancel_modal(),
            Hit::ConfirmYes => self.confirm_yes(),
            Hit::Quit => self.should_quit = true,
        }
    }

    /// The control under the pointer, if any. The renderer rebuilds `hits` every
    /// frame, so this only ever answers with something currently on screen.
    fn hit_at(&self, col: u16, row: u16) -> Option<Hit> {
        self.hits
            .iter()
            .find(|(r, _)| hit(*r, col, row))
            .map(|(_, h)| *h)
    }

    /// What a click at this cell means. Pure: it reads the rects the renderer
    /// wrote and decides nothing else, so a test can ask it the question a user
    /// asks with the pointer.
    ///
    /// Order matters. A control that was painted over the list or the bar owns
    /// its own cells, so the hit boxes answer first.
    pub fn click_at(&self, col: u16, row: u16) -> Click {
        if let Some(h) = self.hit_at(col, row) {
            return Click::Control(h);
        }
        if hit(self.progress_rect, col, row) {
            return match self.seek_target(col) {
                Some(secs) => Click::Seek(secs),
                None => Click::Nothing,
            };
        }
        if hit(self.list_inner, col, row) {
            let vi = self.list_offset + (row - self.list_inner.y) as usize;
            if vi < self.visible().len() {
                return Click::Row(vi);
            }
        }
        if hit(self.queue_inner, col, row) {
            // The entry, not the screen row: an entry is `QUEUE_ROWS_PER_ENTRY`
            // rows tall, so a click on its title or on the artist beneath it
            // lands on the same entry.
            let qi = self.queue_offset + (row - self.queue_inner.y) as usize / QUEUE_ROWS_PER_ENTRY;
            if qi < self.queue.len() {
                return Click::QueueRow(qi);
            }
        }
        Click::Nothing
    }

    fn on_click(&mut self, col: u16, row: u16) {
        let landed = self.click_at(col, row);
        match landed {
            Click::Control(h) => self.dispatch(h),
            Click::Seek(secs) => self.player.seek(secs),
            // Clicking into a region is asking for it, which is the gesture
            // nobody has to be taught and the mouse's half of the focus key.
            Click::Row(vi) => {
                self.give_focus(Focus::List);
                self.selected = vi;
                self.on_row_click(landed);
            }
            Click::QueueRow(qi) => {
                self.give_focus(Focus::Queue);
                self.queue_selected = qi;
                self.on_row_click(landed);
            }
            Click::Nothing => {}
        }
    }

    /// One click puts the cursor on a row; a second one within the double-click
    /// window acts on it. Shared by both lists so the gesture is one gesture.
    fn on_row_click(&mut self, landed: Click) {
        let now = Instant::now();
        let is_double = matches!(
            self.last_click,
            Some((was, t)) if was == landed && now.duration_since(t) < Duration::from_millis(400)
        );
        if is_double {
            self.on_enter();
            self.last_click = None;
        } else {
            self.last_click = Some((landed, now));
        }
    }

    /// How long the playing track is, in seconds.
    ///
    /// **The listing's figure, not mpv's.** The segment protocol advertises no
    /// size until the download finishes - `size` answers -1 while `total` is
    /// `None` - so mpv has an unknown-length stream and estimates the duration
    /// from what it has demuxed. That estimate *grows*, which had the total time
    /// counting up beside the position and, worse, had a click near the end of
    /// the bar seeking to a fraction of a length that was not the track's.
    ///
    /// mpv's figure is the fallback, for a source whose length it does know and
    /// for a track the listing never described.
    #[must_use]
    pub fn duration(&self) -> f64 {
        match &self.now_playing {
            Some(t) if t.duration_secs > 0 => f64::from(t.duration_secs),
            _ => self.status.duration,
        }
    }

    /// The position a pointer at column `col` names, or `None` when there is no
    /// bar on screen or no track to seek within.
    fn seek_target(&self, col: u16) -> Option<f64> {
        let pr = self.progress_rect;
        let total = self.duration();
        if pr.width == 0 || total <= 0.0 {
            return None;
        }
        let rel = f64::from(col.saturating_sub(pr.x)) / f64::from(pr.width);
        Some(rel.clamp(0.0, 1.0) * total)
    }

    fn seek_to_x(&self, col: u16) {
        if let Some(secs) = self.seek_target(col) {
            self.player.seek(secs);
        }
    }

    pub fn queue_indicator(&self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(format!("{}/{}", self.playing_row() + 1, self.queue.len()))
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

/// How many rates go on one row before the list is carried onto the next.
///
/// The overlay clips rather than wrapping, so a list long enough to run past
/// the box would lose its tail silently - and the tail of a rate list is where
/// the hi-res rates are.
const RATES_PER_ROW: usize = 8;

/// What the server may clock at, whether this track fits, and what to change.
///
/// Pure, like `path_rows`: the decision arrives already made from
/// `ClockRates::advise` and this only lays it out.
///
/// Silent when the dump published no setting *and* nothing is playing, by the
/// same rule the blame sentence follows - there is no question and no answer,
/// and a section saying so twice on every idle reading is noise. One of the two
/// is enough to be worth a row: what the server permits is a fact about the
/// machine even between tracks, and an unreadable setting is worth admitting
/// once there is a rate it would have been compared against.
fn clock_rows(clock: &ClockRates, supported_hz: &[u32], source: SourceFormat) -> Vec<GraphRow> {
    let permitted_hz = clock.permitted_hz();
    if permitted_hz.is_none() && source.rate_hz == 0 {
        return Vec::new();
    }
    let advice = clock.advise(source.rate_hz, supported_hz);
    let mut rows = vec![note(""), note("  Server clock")];

    match permitted_hz.as_deref() {
        None => rows.push(reading("    permitted", "unknown".to_string())),
        Some(rates_hz) => {
            for (chunk, rates) in rates_hz.chunks(RATES_PER_ROW).enumerate() {
                let label = if chunk == 0 { "    permitted" } else { "" };
                rows.push(reading(label, crate::ui::fmt_khz_list(rates)));
            }
        }
    }

    if source.rate_hz > 0 {
        let refused = matches!(
            advice,
            RateAdvice::Missing { .. } | RateAdvice::Pinned { .. } | RateAdvice::Unsupported { .. }
        );
        let mut detail = crate::ui::fmt_khz(source.rate_hz);
        if refused {
            detail.push_str("  not permitted");
        }
        rows.push(GraphRow {
            label: "    this track".to_string(),
            detail,
            // The same red the badge and the accused node use: the server
            // refusing this rate is what the badge is reporting, and a second
            // colour for one answer would read as a second opinion.
            kind: if refused {
                GraphRowKind::Culprit
            } else {
                GraphRowKind::Note
            },
        });
    }

    rows.extend(advice.lines().iter().map(|line| note(&format!("  {line}"))));

    // The rates the device can do that the server is not set to use: the "set up
    // audio" offer's reason, shown whether or not this one track needed them, so
    // the setup can be done once rather than a track at a time.
    let blocked: Vec<u32> = permitted_hz
        .as_deref()
        .map(|allowed| {
            supported_hz
                .iter()
                .copied()
                .filter(|hz| !allowed.contains(hz))
                .collect()
        })
        .unwrap_or_default();
    if !blocked.is_empty() {
        rows.push(note(&format!(
            "  Your DAC can also do {}, which PipeWire isn't set to use.",
            crate::ui::fmt_khz_list(&blocked)
        )));
    }
    rows
}

/// What has the output device open, and what it would take to reserve it.
///
/// Pure, like `clock_rows`: the determination arrives already made - from the
/// dump, through `AudioGraph::holder`, or from the player on the direct path -
/// and this only lays it out.
///
/// Always drawn, unlike the two sections above it. Those answer a question the
/// reader had; this one is the question they did not know to ask, and a
/// well-behaved server passing samples through untouched is exactly the case
/// where it would otherwise never come up.
fn holder_rows(holder: &DeviceHolder) -> Vec<GraphRow> {
    let mut rows = vec![note(""), note("  Output device")];
    match holder {
        DeviceHolder::NoDevice => rows.push(reading(
            "    held by",
            "no output device on this chain".to_string(),
        )),
        DeviceHolder::Unknown { sink } => {
            rows.push(reading("    held by", "unknown".to_string()));
            rows.push(reading("    chain ends at", sink.clone()));
        }
        DeviceHolder::Direct { device } => {
            rows.push(reading("    held by", "priel".to_string()));
            rows.push(reading("    device", device.clone()));
        }
        DeviceHolder::Server(held) => {
            // The session manager rather than the server itself is what opened
            // the PCM, and naming it is the difference between a fact and a
            // slogan. Where the dump did not carry the client, the sentence
            // stops at what it did say.
            let by = held.opened_by.as_deref().map_or_else(
                || "the sound server".to_string(),
                |who| format!("the sound server ({who})"),
            );
            rows.push(reading("    held by", by));
            rows.push(reading("    device", held.sink.clone()));
            // Labelled `pcm` rather than `card`: `hw:2,0` is the PCM the server
            // opened, and the card is the `alsa_card.` name the rule below
            // matches on. Calling both of them the card is how a reader ends up
            // putting one where the other belongs.
            rows.extend(held.pcm.as_ref().map(|pcm| reading("    pcm", pcm.clone())));
        }
    }
    rows.extend(holder.lines().iter().map(|line| note(&format!("  {line}"))));
    rows
}

/// A labelled reading: what it is on the left, what it says on the right.
fn reading(label: &str, detail: String) -> GraphRow {
    GraphRow {
        label: label.to_string(),
        detail,
        kind: GraphRowKind::Note,
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
/// How long a listing is, whenever the service has said.
///
/// Kept once `loaded` has caught up rather than dropped, so that a caller can
/// say a listing is complete instead of leaving it to be read off a number that
/// has gone missing. Completeness is information, and the heading now spends
/// words on it (`all 18 loaded`) for the same reason the access badge names the
/// shared case rather than leaving it to an absence.
///
/// `None` only when the service never said - a total of zero rows loaded is not
/// a total of zero rows.
fn rows_missing(loaded: usize, total: u32) -> Option<u32> {
    let known = usize::try_from(total).ok()?;
    (known > 0 || loaded == 0).then_some(total)
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
    use priel_player::graph::{ClockRates, HeldDevice, SinkLevels};
    use priel_player::hw::HwParams;
    use priel_player::{Fidelity, OutputAccess};

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
            ..Track::default()
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

    fn mix(id: &str, title: &str) -> priel_core::Mix {
        priel_core::Mix {
            id: id.into(),
            title: title.into(),
            subtitle: "Miles Davis and more".into(),
        }
    }

    fn stream(id: u64) -> ResolvedStream {
        ResolvedStream {
            source: PlayableSource::Direct(format!("http://127.0.0.1:1/{id}")),
            sample_rate: 192_000,
            bit_depth: 24,
            codec: "flac".into(),
            quality: "HI_RES_LOSSLESS".into(),
            replay_gain_db: 0.0,
            peak: 0.0,
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

    fn wheel_down(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
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
    fn a_track_starting_reads_the_graph_clock_and_hands_the_reply_to_the_player() {
        // Goal: the verdict's fallback where there is no ALSA readout - a
        // Bluetooth sink - is the graph clock, read once when a track starts
        // (not on the status poll) and handed to the player. Playing a row must
        // ask the worker for it, and the reply must be forwarded without
        // disturbing the app.
        let mut r = rig();
        r.app.favorites = vec![track(1, "A", "X")];
        r.app.selected = 0;
        r.app.on_key(code(KeyCode::Enter));
        assert!(
            requests(&r)
                .iter()
                .any(|c| matches!(c, ToWorker::ReadClock)),
            "a fresh play asks the worker for the clock"
        );

        r.to_app
            .send(FromWorker::OutputClock(Some(ClockRates {
                allowed_hz: Some(vec![48_000]),
                current_hz: Some(48_000),
                forced_hz: None,
            })))
            .expect("send");
        r.app.drain_worker(); // handed to the player, no panic
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
            Some(4),
            "and the heading still says how long the list is, so it can read \
             `all 4 loaded` rather than leaving completeness to an absence"
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

    // ---- favorites ----

    /// Favorites loaded and drawn, with the cursor on the first row. Every row
    /// in that listing is a favorite by definition, which is the only thing that
    /// ever says so.
    fn favorites_loaded(r: &mut Rig, ids: std::ops::Range<u64>) {
        r.app.start();
        r.to_app.send(favorites_page(0, ids, 3)).unwrap();
        r.app.drain_worker();
        r.app.list_inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        let _ = requests(r);
    }

    fn favorite_requests(reqs: &[ToWorker]) -> Vec<(u64, bool)> {
        reqs.iter()
            .filter_map(|c| match c {
                ToWorker::SetFavorite { track_id, favorite } => Some((*track_id, *favorite)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_favorites_listing_is_the_only_thing_that_says_a_track_is_kept() {
        // Goal: no track on the wire carries a favorite flag, so the rows of
        // that one listing are the whole source of truth. Results from anywhere
        // else say nothing about it, and must not be read as saying "no".
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        assert!(r.app.is_favorite(2), "a row of the favorites listing");

        searched(&mut r, "blue", 40..42, 2);
        assert!(
            !r.app.is_favorite(40),
            "a search hit priel has been told nothing about"
        );
    }

    #[test]
    fn the_heart_changes_before_the_service_has_answered() {
        // Goal: the deliberate optimism. One bit, changed now and put back if it
        // is refused - a control that waited for a round trip would read as
        // broken for the length of one.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.on_key(key('f'));

        assert!(!r.app.is_favorite(1), "the heart empties on the keystroke");
        assert_eq!(
            favorite_requests(&requests(&r)),
            vec![(1, false)],
            "and the request goes out carrying the state that is on screen"
        );
    }

    #[test]
    fn a_track_the_listing_never_mentioned_is_added_rather_than_removed() {
        // Goal: the other direction. The key is one action whose meaning comes
        // from what priel currently believes, so a search hit is added.
        let mut r = rig();
        searched(&mut r, "blue", 40..42, 2);
        r.app.selected = 0;
        r.app.on_key(key('f'));

        assert!(r.app.is_favorite(40));
        assert_eq!(favorite_requests(&requests(&r)), vec![(40, true)]);
    }

    #[test]
    fn the_key_and_the_control_run_the_same_thing() {
        // Goal: parity is enforced by there being one method, not by two paths
        // being kept in step. Asserting it here is what stops a later edit
        // giving the control its own copy.
        let mut by_key = rig();
        favorites_loaded(&mut by_key, 1..4);
        by_key.app.on_key(key('f'));

        let mut by_click = rig();
        favorites_loaded(&mut by_click, 1..4);
        by_click.app.dispatch(Hit::FavoriteSelected);

        assert_eq!(
            favorite_requests(&requests(&by_key)),
            favorite_requests(&requests(&by_click))
        );
        assert_eq!(by_key.app.is_favorite(1), by_click.app.is_favorite(1));
    }

    #[test]
    fn the_playing_track_has_a_control_of_its_own() {
        // Goal: the row under the cursor and the track in the speakers are
        // routinely different tracks. A single action would leave whichever one
        // the user was not looking at unreachable.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.on_key(code(KeyCode::Enter)); // play row 1
        r.app.selected = 2; // and move the cursor off it
        let _ = requests(&r);

        r.app.dispatch(Hit::FavoriteNowPlaying);
        assert_eq!(
            favorite_requests(&requests(&r)),
            vec![(1, false)],
            "the playing track, not the highlighted one"
        );
    }

    #[test]
    fn a_refused_change_puts_the_heart_back() {
        // Goal: what makes the optimism honest. Without this the user is left
        // believing something happened that did not, which is worse than a
        // stale listing - a listing at least corrects itself on reload.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.on_key(key('f'));
        assert!(!r.app.is_favorite(1));

        r.to_app
            .send(unreachable(Task::SetFavorite {
                track_id: 1,
                wanted: false,
            }))
            .unwrap();
        r.app.drain_worker();

        assert!(r.app.is_favorite(1), "the heart comes back");
        assert!(
            r.app.notice.as_deref().unwrap_or_default().contains("⚠"),
            "and the user is told: {:?}",
            r.app.notice
        );
    }

    #[test]
    fn a_refusal_that_arrives_late_does_not_undo_a_newer_change() {
        // Goal: two presses in quick succession send two requests, and the
        // failure for the first can land after the second has been shown. The
        // newer state has to stand. It does so without a guard, because the
        // state is one bit: the belief has already moved to `!wanted`, and
        // putting `!wanted` back over it is a no-op.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.on_key(key('f')); // off
        r.app.on_key(key('f')); // and on again
        assert!(r.app.is_favorite(1));

        r.to_app
            .send(unreachable(Task::SetFavorite {
                track_id: 1,
                wanted: false,
            }))
            .unwrap();
        r.app.drain_worker();

        assert!(
            r.app.is_favorite(1),
            "the newer state stands; only the change that failed is undone"
        );
    }

    #[test]
    fn a_track_taken_off_the_favorites_keeps_its_row_until_the_list_is_reloaded() {
        // Goal: the deliberate answer to what happens to a loaded page. The row
        // stays and only its heart changes, for two reasons that both bite.
        // Dropping it would move every row below it out from under the cursor -
        // including the one just acted on, which makes the undo unreachable -
        // and `Paging::absorb` requires the next page to continue where the
        // loaded rows end, so a hole in the middle would silently skip a row.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.selected = 1;
        r.app.on_key(key('f'));

        assert_eq!(ids(&r.app.favorites), vec![1, 2, 3], "the row stays put");
        assert_eq!(r.app.selected, 1, "and so does the cursor");
        assert!(!r.app.is_favorite(2), "only the heart changed");
    }

    #[test]
    fn reloading_the_favorites_takes_the_state_from_the_service_again() {
        // Goal: the set mirrors the rows, so the first page *replaces* it rather
        // than adding to it. An id kept across a reload is a track taken off the
        // favorites somewhere else that would wear a heart until priel was
        // restarted - and a listing shorter than the one before it is the only
        // way that shows up.
        let mut r = rig();
        favorites_loaded(&mut r, 1..4);
        r.app.on_key(key('f')); // priel now believes 1 is not a favorite
        assert!(!r.app.is_favorite(1));

        r.app.on_key(key('r'));
        r.to_app.send(favorites_page(0, 1..3, 2)).unwrap();
        r.app.drain_worker();

        assert!(r.app.is_favorite(1), "the service is believed over priel");
        assert!(
            !r.app.is_favorite(3),
            "and a row the listing no longer has loses its heart"
        );
    }

    #[test]
    fn a_later_page_of_favorites_adds_to_what_is_known_rather_than_replacing_it() {
        // Goal: the set mirrors the rows both ways round. Only the first page
        // replaces; a second page that also cleared would empty every heart
        // above it the moment the user scrolled past the first hundred.
        let mut r = rig();
        r.app.start();
        r.to_app.send(favorites_page(0, 1..3, 4)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        r.app.refresh();

        r.to_app.send(favorites_page(2, 3..5, 4)).unwrap();
        r.app.drain_worker();

        assert!(r.app.is_favorite(1), "the first page keeps its hearts");
        assert!(r.app.is_favorite(4), "and the second page brings its own");
    }

    #[test]
    fn a_view_with_no_track_under_the_cursor_asks_for_nothing() {
        // Goal: the playlists list holds playlists, and the key has nothing to
        // act on there. Sending a request for whatever id happened to be at that
        // index in another view's rows is the bug this guards.
        let mut r = rig();
        playlists_loaded(&mut r, &["mix"], 1);
        r.app.on_key(key('f'));
        assert!(favorite_requests(&requests(&r)).is_empty());
    }

    #[test]
    fn the_filter_decides_which_track_the_key_acts_on() {
        // Goal: selection is an index into the *filtered* rows. Reading the
        // backing vec with it would favorite a different track than the
        // highlighted one whenever a filter was on.
        let mut r = rig();
        r.app.start();
        r.to_app
            .send(FromWorker::Favorites {
                offset: 0,
                page: priel_core::Page {
                    items: vec![track(1, "Blue", "A"), track(2, "Red", "B")],
                    total: 2,
                },
            })
            .unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        r.app.on_key(key('/'));
        for c in "Red".chars() {
            r.app.on_key(key(c));
        }
        // Accepted first: while the box is open every key is text, `f` included.
        r.app.on_key(code(KeyCode::Enter));
        r.app.on_key(key('f'));

        assert_eq!(
            favorite_requests(&requests(&r)),
            vec![(2, false)],
            "the one row the filter left on screen"
        );
    }

    // ---- the session bus ----

    /// A rig whose app is on a bus with both ends in the test's hands. No
    /// socket is opened and no thread is started: `Bus::rigged` is the same
    /// seam `App::rigged` is.
    struct OnTheBus {
        rig: Rig,
        emitted: Receiver<crate::bus::wire::Message>,
        consumer: Sender<BusCommand>,
    }

    impl OnTheBus {
        fn new() -> Self {
            let mut rig = rig();
            let (bus, emitted, consumer) = Bus::rigged();
            rig.app.bus = Some(bus);
            Self {
                rig,
                emitted,
                consumer,
            }
        }

        /// Everything the app announced, as `(member, changed property names)`.
        fn announced(&self) -> Vec<(String, Vec<String>)> {
            std::iter::from_fn(|| self.emitted.try_recv().ok())
                .map(|signal| {
                    let member = signal.member().unwrap_or_default().to_owned();
                    let names = signal
                        .body
                        .iter()
                        .find_map(|arg| match arg {
                            crate::bus::wire::Arg::Dict(fields) => {
                                Some(fields.iter().map(|(name, _)| name.clone()).collect())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    (member, names)
                })
                .collect()
        }

        /// Queue three tracks and start the first, as pressing Enter does.
        fn playing(&mut self) -> &mut Self {
            self.rig.app.favorites = vec![
                track(1, "One", "A"),
                track(2, "Two", "B"),
                track(3, "Three", "C"),
            ];
            self.rig.app.start_queue_at(0);
            self.rig.app.status.playing = true;
            self.rig.app.status.duration = 300.0;
            self.rig.app.status.volume = 100.0;
            self.rig.app.refresh_for_test();
            let _ = self.announced();
            self
        }
    }

    /// Goal: **MPRIS is a third caller of an action, never a second
    /// implementation of one.** Every transport method has to end up in the
    /// same method the key binding calls, or the two drift and the desktop
    /// button stops matching the key. Method: drive each command and check the
    /// effect the key has.
    #[test]
    fn the_desktop_runs_the_very_methods_the_keys_do() {
        let mut on = OnTheBus::new();
        on.playing();
        assert_eq!(on.rig.app.queue_pos, 0);

        on.rig.app.apply(BusCommand::Next);
        assert_eq!(on.rig.app.queue_pos, 1, "Next is what `n` does");
        on.rig.app.apply(BusCommand::Previous);
        assert_eq!(on.rig.app.queue_pos, 0, "Previous is what `p` does");

        on.rig.app.apply(BusCommand::Shuffle(true));
        assert!(on.rig.app.shuffle, "Shuffle is what `s` does");
        on.rig.app.apply(BusCommand::Loop(Repeat::One));
        assert_eq!(
            on.rig.app.repeat,
            Repeat::One,
            "LoopStatus is what `e` does"
        );
        on.rig.app.apply(BusCommand::Quit);
        assert!(on.rig.app.should_quit, "Quit is what `q` does");
    }

    /// Goal: `Shuffle` is absolute where the key is a toggle, and there must be
    /// one implementation with two callers. Asking for a state already in force
    /// leaves it there, where a toggle would turn it off. Method: ask twice.
    #[test]
    fn a_shuffle_from_the_desktop_is_absolute_where_the_key_is_a_toggle() {
        let mut r = rig();
        r.app.apply(BusCommand::Shuffle(true));
        r.app.apply(BusCommand::Shuffle(true));
        assert!(r.app.shuffle, "asking twice is not a cycle");
        r.app.on_key(key('s'));
        assert!(!r.app.shuffle, "and the key still toggles");
    }

    /// Goal: `LoopStatus` is absolute where the key cycles, in the way `Shuffle`
    /// already is: three states make the difference plainer than two, because a
    /// cycle asked for twice lands somewhere the consumer never named. Method:
    /// ask twice, then check the key still cycles from where the desktop left
    /// it, and that the state reaches the desktop's own snapshot.
    #[test]
    fn a_repeat_from_the_desktop_is_absolute_where_the_key_cycles() {
        let mut r = rig();
        r.app.apply(BusCommand::Loop(Repeat::All));
        r.app.apply(BusCommand::Loop(Repeat::All));
        assert_eq!(r.app.repeat, Repeat::All, "asking twice is not a cycle");
        assert_eq!(r.app.bus_snapshot().now.repeat, Repeat::All);
        r.app.on_key(key('e'));
        assert_eq!(r.app.repeat, Repeat::One, "and the key carries on from it");
    }

    /// Goal: commands arrive over a channel the app drains on its own tick, so
    /// the bus thread never touches app state and the render thread never
    /// blocks on the bus. Method: post a command as a consumer would and let
    /// the ordinary refresh pick it up.
    #[test]
    fn what_a_consumer_asks_for_is_run_on_the_apps_own_tick() {
        let mut on = OnTheBus::new();
        on.playing();
        on.consumer
            .send(BusCommand::Next)
            .expect("the app is listening");
        assert_eq!(on.rig.app.queue_pos, 0, "not until the tick");
        on.rig.app.refresh_for_test();
        assert_eq!(on.rig.app.queue_pos, 1);
    }

    /// Goal: **a gapless change is one announcement.** The metadata changes
    /// while the playback status stays `Playing`, and two signals let a
    /// consumer render the old title against the new position. Method: move
    /// mpv on to the next track the way a transition does, and count what went
    /// out.
    #[test]
    fn a_gapless_change_is_announced_once_and_stays_playing() {
        let mut on = OnTheBus::new();
        on.playing();
        // mpv moved on by itself, which is what a gapless transition looks like.
        on.rig.app.status.current_id = 2;
        on.rig.app.refresh_for_test();

        let announced = on.announced();
        assert_eq!(announced.len(), 1, "one signal, not two: {announced:?}");
        assert_eq!(announced[0].0, "PropertiesChanged");
        assert_eq!(
            announced[0].1,
            vec!["Metadata".to_owned()],
            "the status did not change, so it is not announced"
        );
        assert_eq!(
            on.rig
                .app
                .published
                .now
                .track
                .as_ref()
                .map(|t| t.title.as_str()),
            Some("Two")
        );
    }

    /// Goal: a quiet tick costs the bus nothing. priel refreshes ten times a
    /// second for hours, and announcing an unchanged property set at that rate
    /// is the traffic-hog bug the position rule exists to prevent. Method: tick
    /// twice over with only the position moved.
    #[test]
    fn a_tick_where_only_the_position_moved_announces_nothing() {
        let mut on = OnTheBus::new();
        on.playing();
        for tenth in 1..5 {
            on.rig.app.status.position = f64::from(tenth) / 10.0;
            on.rig.app.refresh_for_test();
        }
        assert!(on.announced().is_empty(), "{:?}", on.announced());
        assert_eq!(
            on.rig.app.published.position_us, 400_000,
            "the position is published for a Get all the same"
        );
    }

    /// Goal: a jump is announced by `Seeked`, which is what a consumer needs
    /// because the position itself may never be put in a property change.
    /// Method: move the position further than playing could account for.
    #[test]
    fn a_jump_within_one_track_is_announced_as_a_seek() {
        let mut on = OnTheBus::new();
        on.playing();
        on.rig.app.status.position = 120.0;
        on.rig.app.refresh_for_test();
        let announced = on.announced();
        assert_eq!(announced.len(), 1, "{announced:?}");
        assert_eq!(announced[0].0, "Seeked");
    }

    /// Goal: **the same track twice in a queue must not collide**, because a
    /// consumer resets its position when the id changes - and one play of one
    /// entry must keep one id throughout. Method: play the same track twice
    /// and compare the ids, then tick without changing anything.
    #[test]
    fn every_play_of_an_entry_gets_an_id_of_its_own() {
        let mut on = OnTheBus::new();
        on.playing();
        let first = on.rig.app.track_path.clone();
        assert!(!first.is_empty());

        on.rig.app.refresh_for_test();
        assert_eq!(on.rig.app.track_path, first, "one play, one id");

        // A gapless transition is a new entry, so it is a new id too - the
        // consumer resetting its position is the whole point of the change.
        on.rig.app.status.current_id = 2;
        on.rig.app.refresh_for_test();
        let adopted = on.rig.app.track_path.clone();
        assert_ne!(adopted, first, "mpv moved on, so the id moved with it");

        // The same entry, played again.
        on.rig.app.start_queue_at(0);
        assert_ne!(
            on.rig.app.track_path, adopted,
            "a second play of the same track is a second id"
        );
    }

    /// Wait for the player thread to catch up with a command. Only the real
    /// backend has one to catch up: the stub applies nothing at all, which is
    /// why this and its callers are behind the feature.
    #[cfg(feature = "libmpv")]
    fn settle(app: &App) {
        std::thread::sleep(Duration::from_millis(300));
        let _ = app.player.status();
    }

    /// Goal: **`Play` and `Pause` are absolute where the space bar is a
    /// toggle.** Answering `Play` with a toggle pauses a playing track when a
    /// panel applet's play button is pressed twice, which is exactly the bug
    /// this arrangement exists to prevent. Method: ask for the same state
    /// twice each way, through the real player.
    #[cfg(feature = "libmpv")]
    #[test]
    fn play_and_pause_from_the_desktop_are_absolute_and_play_pause_is_not() {
        let mut r = rig();
        r.app.apply(BusCommand::Pause);
        settle(&r.app);
        assert!(r.app.player.status().paused);
        r.app.apply(BusCommand::Pause);
        settle(&r.app);
        assert!(r.app.player.status().paused, "asking twice is not a cycle");

        r.app.apply(BusCommand::Play);
        settle(&r.app);
        assert!(!r.app.player.status().paused);
        r.app.apply(BusCommand::Play);
        settle(&r.app);
        assert!(
            !r.app.player.status().paused,
            "Play on a playing track leaves it playing"
        );

        r.app.apply(BusCommand::PlayPause);
        settle(&r.app);
        assert!(
            r.app.player.status().paused,
            "and PlayPause is still the toggle the space bar is"
        );
    }

    /// Goal: **no session bus means no bus, and it must cost nothing at all** -
    /// that is the media-server box the whole design exists for. Method: the
    /// ordinary rig, which has no bus, driven through everything that would
    /// touch one.
    #[test]
    fn a_machine_with_no_session_bus_pays_nothing_for_one() {
        let mut r = rig();
        r.app.favorites = vec![track(1, "One", "A")];
        r.app.start_queue_at(0);
        r.app.status.playing = true;
        r.app.status.position = 42.0;
        r.app.refresh_for_test();
        r.app.refresh_for_test();
        assert!(r.app.bus.is_none());
        assert_eq!(
            r.app.published,
            Snapshot::default(),
            "with no bus, no snapshot is even built"
        );
    }

    /// Goal: the capability flags are read straight off the queue to enable the
    /// skip buttons, so each has to be the condition the method it enables
    /// actually acts on - a button that is offered and does nothing is worse
    /// than one that is greyed out. Method: an empty queue, then a full one.
    #[test]
    fn the_skip_buttons_follow_what_the_queue_can_actually_do() {
        let mut on = OnTheBus::new();
        on.rig.app.refresh_for_test();
        let bare = on.rig.app.bus_snapshot();
        assert!(!bare.now.can_go_next, "nothing queued, nothing to skip to");
        assert!(!bare.now.can_go_previous);
        assert!(!bare.now.can_seek, "and no duration to seek within");

        on.playing();
        let full = on.rig.app.bus_snapshot();
        assert!(full.now.can_go_next, "two tracks behind the first");
        assert!(full.now.can_go_previous, "which at worst starts it again");
        assert!(full.now.can_seek);

        // The last track of a queue with nothing preloaded behind it.
        on.rig.app.queue_pos = 2;
        assert!(!on.rig.app.bus_snapshot().now.can_go_next);
        // And the shuffle does not change that. It says what the play order is,
        // not that there is always more of it: the last row of a shuffled order
        // is as much the end of the queue as the last row of the listing, which
        // is what "no track repeats until every other has played" means.
        on.rig.app.shuffle = true;
        assert!(
            !on.rig.app.bus_snapshot().now.can_go_next,
            "the end of an order is the end of an order"
        );
        on.rig.app.queue_pos = 1;
        assert!(
            on.rig.app.bus_snapshot().now.can_go_next,
            "with a row still under it, there is somewhere to go"
        );
    }

    /// Goal: what the desktop reads is what priel is playing. The scales differ
    /// on both sides (a percentage against unity, seconds against microseconds)
    /// and getting either wrong is silent. Method: publish a track and read the
    /// whole snapshot back off the bus's own end.
    #[test]
    fn the_bus_thread_is_handed_what_is_playing_in_the_units_it_publishes() {
        let mut on = OnTheBus::new();
        on.playing();
        on.rig.app.status.position = 30.0;
        on.rig.app.status.volume = 50.0;
        on.rig.app.refresh_for_test();

        let published = on.rig.app.bus.as_ref().expect("a bus").published();
        let entry = published.now.track.expect("something is playing");
        assert_eq!(entry.title, "One");
        assert_eq!(entry.artist, "A");
        assert_eq!(entry.length_us, 100_000_000, "the listing gives seconds");
        assert_eq!(published.position_us, 30_000_000);
        assert!(
            (published.now.volume - 0.5).abs() < 1e-9,
            "unity is 1.0 where priel counts to 100: {}",
            published.now.volume
        );
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
    ///
    /// The holder is `Unknown` because a hand-built path carries no dump behind
    /// it to name the device from, which is exactly what the parser reports for
    /// a sink with nothing behind it. Tests that are about the holder set their
    /// own.
    fn chain() -> AudioGraph {
        AudioGraph {
            holder: DeviceHolder::Unknown {
                sink: "Studio DAC".into(),
            },
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
    fn the_bluetooth_standing_compares_the_active_codec_to_what_the_device_offers() {
        // Goal: whether the active codec is the best the device offers - what
        // colours the verdict yellow (best, nothing to do) or red (a better one
        // exists), and what the report names as available.
        let available = vec![
            priel_player::graph::BtProfile {
                codec: "sbc".into(),
                profile_index: 1,
            },
            priel_player::graph::BtProfile {
                codec: "aac".into(),
                profile_index: 2,
            },
            priel_player::graph::BtProfile {
                codec: "aptx".into(),
                profile_index: 3,
            },
            priel_player::graph::BtProfile {
                codec: "aptx_hd".into(),
                profile_index: 4,
            },
        ];
        let mut r = rig();
        let mut g = chain();
        g.bt_available = available;
        r.app.audio_graph = Some(Ok(g));

        // On the best available codec: Best, and the verdict reads "not improvable".
        r.app.status.bt_codec = Some("aptx_hd".into());
        assert_eq!(r.app.bt_codec_standing(), Some(BtStanding::Best));
        assert_eq!(r.app.bt_improvable(), Some(false));

        // On a lesser codec: Improvable, naming the best available.
        r.app.status.bt_codec = Some("sbc".into());
        assert_eq!(
            r.app.bt_codec_standing(),
            Some(BtStanding::Improvable {
                better: "aptx_hd".into()
            })
        );
        assert_eq!(r.app.bt_improvable(), Some(true));

        // Not a Bluetooth output at all: no standing.
        r.app.status.bt_codec = None;
        assert_eq!(r.app.bt_codec_standing(), None);

        // Bluetooth but the available codecs are unknown: no standing - priel
        // cannot call the active one best or offer a better.
        r.app.status.bt_codec = Some("aptx_hd".into());
        let mut unknown = chain();
        unknown.bt_available = Vec::new();
        r.app.audio_graph = Some(Ok(unknown));
        assert_eq!(r.app.bt_codec_standing(), None);
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
        let text = overlay_text(&r.app);
        assert!(
            text.contains("Reading the graph"),
            "the chain section says what it is waiting for: {text}"
        );
        assert!(
            text.contains("Verdict") && text.contains("Volume"),
            "and every other section answers for itself meanwhile: {text}"
        );
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

    // ---- the output report ----

    /// The report's rows as one blob, for asserting what it says.
    fn report(app: &App) -> String {
        app.graph_rows()
            .iter()
            .map(|r| format!("{}  {}", r.label, r.detail))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_section_of_the_report_renders_what_it_can_on_its_own() {
        // Goal: the overlay used to short-circuit entirely on the direct path,
        // which hid the volume section from exclusive users - the people who
        // care most, and the ones with the cleanest chain to show. Each section
        // now answers for itself: there is no chain to draw and everything else
        // is still true.
        let mut r = rig();
        r.app.status = PlaybackStatus {
            audio_device: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            ao_volume: None,
            access: OutputAccess::Exclusive,
            ..through_server()
        };
        r.app.on_key(key('D'));
        let out = report(&r.app);

        assert!(out.contains("Verdict"), "{out}");
        assert!(out.contains("bit-perfect"), "{out}");
        assert!(out.contains("Device"), "{out}");
        assert!(out.contains("exclusive"), "the access moved here: {out}");
        assert!(out.contains("Volume"), "the section that was hidden: {out}");
        assert!(out.contains("priel"), "{out}");
        assert!(out.contains("Chain"), "{out}");
        assert!(out.contains("no graph"), "{out}");
    }

    #[test]
    fn a_sink_attenuating_in_software_is_reported_in_percent_decibels_and_bits() {
        // Goal: "how much did I lose" is the question nothing on screen answers
        // today. The percentage is the one a mixer shows (the cube root of the
        // linear gain - a slider at 30% sits at 0.027 in the node), the decibel
        // figure is the one that compares, and the bits are what it cost - by the
        // one-bit-per-6-dB rule the README already records. The loss is measured
        // from the linear gain, not the percentage.
        let mut r = rig();
        r.app.status = through_server();
        let mut g = chain();
        g.volume = SinkVolume::Read(SinkLevels {
            set: vec![0.027_001, 0.027_001],
            software: vec![0.027_001, 0.027_001],
            silenced: false,
        });
        r.app.on_key(key('D'));
        r.to_app.send(FromWorker::AudioGraph(Ok(g))).expect("send");
        r.app.drain_worker();
        let out = report(&r.app);

        assert!(out.contains("30%"), "the number a mixer shows: {out}");
        assert!(!out.contains("2.7%"), "not the raw linear value: {out}");
        assert!(
            out.contains("-31 dB"),
            "the loss, from the linear gain: {out}"
        );
        assert!(out.contains("5 bits"), "what it cost: {out}");
        assert!(
            out.contains("changing the audio samples"),
            "in plain terms, what it did: {out}"
        );
    }

    #[test]
    fn a_level_the_server_is_not_applying_is_shown_without_a_loss_it_did_not_cause() {
        // Goal: the reading measured on a real machine, and the reason this
        // needs two fields rather than one. The level is set to 30% and the
        // software is multiplying nothing, so quoting 31 dB of loss there would
        // invent a fault - and saying nothing at all would hide a level the
        // listener plainly did set.
        let mut r = rig();
        r.app.status = through_server();
        let mut g = chain();
        g.volume = SinkVolume::Read(SinkLevels {
            set: vec![0.027_001, 0.027_001],
            software: vec![1.0, 1.0],
            silenced: false,
        });
        r.app.on_key(key('D'));
        r.to_app.send(FromWorker::AudioGraph(Ok(g))).expect("send");
        r.app.drain_worker();
        let out = report(&r.app);

        assert!(out.contains("30%"), "the level is still shown: {out}");
        assert!(!out.contains("bits"), "no loss is claimed: {out}");
        assert!(
            out.contains("the audio samples are untouched"),
            "and the reader is told, plainly, why it costs nothing: {out}"
        );
    }

    #[test]
    fn a_desktop_slider_at_half_reads_as_a_mixer_shows_it_not_the_linear_value() {
        // Goal: PipeWire stores the sink volume as a linear amplitude - a slider
        // left at 50% sits at 0.125 in the node - but every mixer (KDE, wpctl,
        // PulseAudio) shows the cube root of it. Printing the raw 0.125 as
        // "12.5%" is a figure no mixer shows and does not match the desktop; the
        // cube root, "50%", is the number the listener actually set. The software
        // is not applying it, so the samples are untouched and nothing is lost.
        let mut r = rig();
        r.app.status = through_server();
        let mut g = chain();
        g.volume = SinkVolume::Read(SinkLevels {
            set: vec![0.125, 0.125],
            software: vec![1.0, 1.0],
            silenced: false,
        });
        r.app.on_key(key('D'));
        r.to_app.send(FromWorker::AudioGraph(Ok(g))).expect("send");
        r.app.drain_worker();
        let out = report(&r.app);

        assert!(out.contains("50%"), "the number the slider shows: {out}");
        assert!(!out.contains("12.5%"), "not the raw linear value: {out}");
        assert!(
            !out.contains("bits"),
            "the samples are untouched, no loss: {out}"
        );
        assert!(
            out.contains("the audio samples are untouched"),
            "and it says so plainly: {out}"
        );
    }

    #[test]
    fn a_stage_that_could_not_be_read_says_unknown_in_the_report_too() {
        // Goal: the same rule the row's mark follows, spelled out where there
        // is room for it. An empty cell would read as nothing being set.
        let mut r = rig();
        r.app.status = PlaybackStatus {
            ao_volume: None,
            ..through_server()
        };
        r.app.on_key(key('D'));
        let out = report(&r.app);
        assert!(out.contains("unknown"), "{out}");
    }

    // ---- the sink's volume, which the row is graded on ----

    /// A status playing through the sound server, with everything mpv can see
    /// at unity, so a test only has to move the stage it is about.
    fn through_server() -> PlaybackStatus {
        PlaybackStatus {
            loaded: true,
            playing: true,
            volume: 100.0,
            ao_volume: Some(100.0),
            sample_rate: 44_100,
            out_format: "s32".into(),
            in_sample_rate: 44_100,
            in_format: "s32".into(),
            audio_device: "pipewire/alsa_output.usb-x".into(),
            ..PlaybackStatus::default()
        }
    }

    #[test]
    fn the_row_is_graded_on_the_sink_volume_the_worker_last_read() {
        // Goal: the verdict on the bottom row has to include the stage nothing
        // else can see. The reading arrives through the same reply the overlay
        // is built from, so the two cannot disagree about one moment.
        let mut r = rig();
        r.app.status = through_server();
        assert!(
            r.app.verdict().needs_qualifying(),
            "before any reading the sink is a stage that went unread"
        );

        let mut g = chain();
        g.volume = SinkVolume::Read(SinkLevels {
            set: vec![0.5, 0.5],
            software: vec![0.5, 0.5],
            silenced: false,
        });
        r.to_app.send(FromWorker::AudioGraph(Ok(g))).expect("send");
        r.app.drain_worker();
        assert_eq!(
            r.app.verdict().fidelity,
            Fidelity::NearBitPerfect(Alteration::SinkVolumeScaled),
            "a sink attenuating in software is no longer reported as clean"
        );
    }

    #[test]
    fn the_direct_path_has_no_sink_stage_to_have_missed() {
        // Goal: decision nine. priel holding the card itself is the cleanest
        // chain there is, and a permanent question mark on it would teach the
        // reader to ignore the mark everywhere it means something.
        let mut r = rig();
        r.app.status = PlaybackStatus {
            audio_device: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            ao_volume: None,
            ..through_server()
        };
        r.app.refresh_for_test();
        let v = r.app.verdict();
        assert_eq!(v.fidelity, Fidelity::BitPerfect);
        assert!(!v.needs_qualifying(), "nothing here went unread");
        assert!(
            r.from_app.try_recv().is_err(),
            "and there is no graph to ask pw-dump about"
        );
    }

    #[test]
    fn the_sink_volume_is_re_read_on_a_cadence_rather_than_every_tick() {
        // Goal: reading it runs pw-dump, and the tick that would ask for it
        // runs ten times a second. A request per tick would fork a subprocess
        // per tick and hold the worker off the listener's own requests.
        let mut r = rig();
        r.app.status = through_server();
        r.app.refresh_for_test();
        assert!(
            matches!(r.from_app.try_recv(), Ok(ToWorker::ReadAudioGraph)),
            "the first tick with something playing asks"
        );
        r.app.refresh_for_test();
        r.app.refresh_for_test();
        assert!(
            r.from_app.try_recv().is_err(),
            "and the ones straight after it do not"
        );
    }

    #[test]
    fn nothing_is_asked_for_while_nothing_is_playing() {
        // Goal: an idle priel has no chain to read and no verdict to reach, so
        // it must not run a subprocess every few seconds for the whole session
        // it is left open.
        let mut r = rig();
        r.app.refresh_for_test();
        assert!(r.from_app.try_recv().is_err());
    }

    #[test]
    fn a_graph_that_could_not_be_read_leaves_the_sink_unread_rather_than_clean() {
        // Goal: "say unknown rather than guess". pw-dump missing says nothing
        // about the sink's volume, and treating a failed read as unity would
        // hand out the flattering answer on every machine without the tools.
        let mut r = rig();
        r.app.status = through_server();
        r.to_app
            .send(FromWorker::AudioGraph(Err(GraphError::NotInstalled)))
            .expect("send");
        r.app.drain_worker();
        assert!(r.app.verdict().needs_qualifying());
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
        let text = overlay_text(&r.app);
        assert!(text.contains("pw-dump was not found"), "{text}");
        assert!(
            text.contains("ships with PipeWire"),
            "and what to do about it: {text}"
        );
        assert!(
            !r.app
                .graph_rows()
                .iter()
                .any(|row| row.kind == GraphRowKind::Node && row.label.contains("  (device)")),
            "there is no chain to draw: {text}"
        );
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
        // The device row, not the last row: the ownership section is drawn
        // below the chain and this is about what the chain says.
        let device = rows
            .iter()
            .rfind(|row| row.kind == GraphRowKind::Node)
            .expect("a node row");
        assert_eq!(device.detail, "no format yet");
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

    /// Both columns, as the overlay draws them: a readout whose answer lives in
    /// the right-hand column is not visible in the labels alone.
    fn overlay_text(app: &App) -> String {
        app.graph_rows()
            .iter()
            .map(|row| format!("{}  {}", row.label, row.detail))
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
        assert!(
            r.app
                .graph_rows()
                .iter()
                .all(|row| row.kind != GraphRowKind::Culprit),
            "nothing is accused: {}",
            overlay_text(&r.app)
        );
        let text = overlay_text(&r.app);
        assert!(
            !text.contains("nothing on this path did it"),
            "no admission about a comparison that was never asked for: {text}"
        );
        assert!(
            !text.contains("Not enough was negotiated"),
            "and no admission about a track there is none of: {text}"
        );
    }

    /// The same chain with the server's clock settings published alongside it,
    /// as one `pw-dump` carries both. The stream carries 32-bit words so the
    /// chain itself is clean for the 24-bit track `playing_hires` sets up, and
    /// the only thing left to find is the setting.
    fn chain_clocked(allowed_hz: &[u32], current_hz: u32) -> AudioGraph {
        let mut g = chain();
        g.path[0].format = Some("S32LE".into());
        g.clock = ClockRates {
            allowed_hz: Some(allowed_hz.to_vec()),
            current_hz: Some(current_hz),
            forced_hz: None,
        };
        g
    }

    #[test]
    fn a_rate_the_server_may_not_use_is_named_with_the_change_that_would_add_it() {
        // Goal: the missing half of the diagnosis. When the permitted list
        // omits the track's rate the server has no choice but to resample, and
        // the badge that follows looks like a priel problem when it is a
        // one-line configuration problem. So: the list, the track's rate, that
        // the two do not meet, and exactly what to write where.
        let r = playing_hires(chain_clocked(&[48_000], 48_000));
        let text = overlay_text(&r.app);
        let rows = r.app.graph_rows();
        let permitted = rows
            .iter()
            .find(|row| row.label.contains("permitted"))
            .expect("the permitted rates are listed");
        assert_eq!(permitted.detail, "48 kHz", "{text}");
        let track = rows
            .iter()
            .find(|row| row.label.contains("this track"))
            .expect("and the rate the track wants");
        assert!(track.detail.starts_with("44.1 kHz"), "{}", track.detail);
        assert!(track.detail.contains("not permitted"), "{}", track.detail);
        assert_eq!(
            track.kind,
            GraphRowKind::Culprit,
            "drawn the colour the badge uses for the same finding: {text}"
        );
        assert!(
            text.contains("default.clock.allowed-rates = [ 44100 48000 ]"),
            "the whole setting, copyable: {text}"
        );
        assert!(
            text.contains("~/.config/pipewire/pipewire.conf.d/"),
            "and where it goes: {text}"
        );
        assert!(text.contains("Restart the sound server"), "{text}");
    }

    #[test]
    fn a_rate_the_server_may_use_is_stated_and_then_left_alone() {
        // Goal: advice printed over a working setup teaches the reader to
        // ignore it. The two readouts still appear - they are what makes the
        // silence meaningful - and nothing else does.
        let r = playing_hires(chain_clocked(&[44_100, 48_000], 48_000));
        let text = overlay_text(&r.app);
        assert!(text.contains("44.1 / 48 kHz"), "{text}");
        assert!(text.contains("this track"), "{text}");
        assert!(
            !text.contains("not permitted"),
            "44.1 kHz is on the list: {text}"
        );
        assert!(
            !text.contains("allowed-rates"),
            "so there is nothing to change: {text}"
        );
        assert!(
            r.app
                .graph_rows()
                .iter()
                .all(|row| row.kind != GraphRowKind::Culprit),
            "and nothing to accuse: {text}"
        );
    }

    // ---- set up audio ----

    /// Permitted 44.1/48; the device also does 88.2 and 176.4, which are blocked.
    fn chain_with_blocked() -> AudioGraph {
        let mut g = chain_clocked(&[44_100, 48_000], 48_000);
        g.supported_hz = vec![44_100, 48_000, 88_200, 176_400];
        g
    }

    #[test]
    fn the_report_says_which_rates_the_device_can_do_that_the_server_blocks() {
        // Goal: the offer's reason, shown whether or not this one track needed
        // the rates, so the setup is a thing you do once rather than per track.
        let r = playing_hires(chain_with_blocked());
        let text = overlay_text(&r.app);
        assert!(text.contains("Your DAC can also do"), "{text}");
        assert!(text.contains("88.2"), "{text}");
        assert!(r.app.setup_available(), "and the offer applies");
    }

    #[test]
    fn set_up_audio_is_not_offered_when_nothing_is_blocked() {
        // Goal: a device whose rates are all permitted has nothing to set up, and
        // the key that would do it does nothing.
        let mut r = playing_hires(chain_clocked(&[44_100, 48_000], 48_000));
        assert!(!r.app.setup_available());
        r.app.on_key(key('A'));
        assert_eq!(r.app.mode, Mode::Graph, "the key does nothing here");
        assert!(r.app.setup.is_none());
    }

    #[test]
    fn the_setup_key_opens_the_preview_with_the_whole_list_to_permit() {
        // Goal: A opens the confirm, and the list it would write is everything
        // already permitted plus the device's blocked rates - added, never
        // dropped, so it cannot take a rate from anything else using it.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        assert_eq!(r.app.mode, Mode::SetupAudio);
        let setup = r.app.setup.as_ref().expect("a flow is up");
        assert_eq!(setup.step, SetupStep::Confirm);
        assert_eq!(setup.adding_hz, vec![88_200, 176_400]);
        assert_eq!(setup.allowed_hz, vec![44_100, 48_000, 88_200, 176_400]);
    }

    #[test]
    fn confirming_writes_the_whole_list_and_waits_for_the_reply() {
        // Goal: yes sends the write with the list the preview showed, and the
        // overlay says it is working until the reply lands.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        let _ = requests(&r);
        r.app.on_key(key('y'));
        let reqs = requests(&r);
        assert!(
            matches!(
                reqs.as_slice(),
                [ToWorker::SetUpAudio { allowed_hz }]
                    if *allowed_hz == vec![44_100, 48_000, 88_200, 176_400]
            ),
            "the write carries the whole list"
        );
        assert!(matches!(
            r.app.setup.as_ref().map(|s| &s.step),
            Some(SetupStep::Writing)
        ));
    }

    #[test]
    fn a_written_file_offers_the_restart_and_r_asks_for_it() {
        // Goal: the file landing turns the overlay into the restart offer, the
        // restart is asked for only on the explicit key, and its success is said.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        r.app.on_key(key('y'));
        let _ = requests(&r);
        r.to_app
            .send(FromWorker::AudioSetUp(Ok(
                "/x/pipewire.conf.d/99-priel-rates.conf".into(),
            )))
            .expect("send");
        r.app.drain_worker();
        assert!(matches!(
            r.app.setup.as_ref().map(|s| &s.step),
            Some(SetupStep::Restart { .. })
        ));
        r.app.on_key(key('r'));
        assert!(matches!(requests(&r).as_slice(), [ToWorker::RestartAudio]));
        assert!(matches!(
            r.app.setup.as_ref().map(|s| &s.step),
            Some(SetupStep::Restarting)
        ));
        r.to_app
            .send(FromWorker::AudioRestarted(Ok(())))
            .expect("send");
        r.app.drain_worker();
        match r.app.setup.as_ref().map(|s| &s.step) {
            Some(SetupStep::Done { message }) => assert!(message.contains("live"), "{message}"),
            _ => panic!("expected Done"),
        }
        r.app.on_key(key('q'));
        assert!(r.app.setup.is_none());
        assert_eq!(r.app.mode, Mode::Normal);
    }

    #[test]
    fn declining_the_restart_keeps_the_file_and_says_how_to_finish() {
        // Goal: the file is written; declining the restart is not a failure, so
        // the notice says what is left to do rather than pretending it is done.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        r.app.on_key(key('y'));
        r.to_app
            .send(FromWorker::AudioSetUp(Ok("/x/99-priel-rates.conf".into())))
            .expect("send");
        r.app.drain_worker();
        let _ = requests(&r);
        r.app.on_key(key('n'));
        assert!(r.app.setup.is_none());
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(
            r.app
                .notice
                .clone()
                .unwrap_or_default()
                .contains("Restart PipeWire"),
            "{:?}",
            r.app.notice
        );
        assert!(requests(&r).is_empty(), "and nothing was restarted");
    }

    #[test]
    fn a_write_that_failed_says_so_and_asks_for_no_restart() {
        // Goal: a write that could not happen ends the flow with the reason, and
        // never offers the restart of a file that is not there.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        r.app.on_key(key('y'));
        let _ = requests(&r);
        r.to_app
            .send(FromWorker::AudioSetUp(Err("permission denied".into())))
            .expect("send");
        r.app.drain_worker();
        match r.app.setup.as_ref().map(|s| &s.step) {
            Some(SetupStep::Done { message }) => {
                assert!(message.contains("permission denied"), "{message}");
            }
            _ => panic!("expected Done"),
        }
        r.app.on_key(key('q'));
        assert!(r.app.setup.is_none());
        assert!(requests(&r).is_empty(), "a failed write asks for nothing");
    }

    #[test]
    fn a_restart_that_failed_says_the_file_landed_but_it_did_not() {
        // Goal: the honest split - the file is there, the restart is not - so the
        // listener knows the rates are written but not yet live.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        r.app.on_key(key('y'));
        r.to_app
            .send(FromWorker::AudioSetUp(Ok("/x/99-priel-rates.conf".into())))
            .expect("send");
        r.app.drain_worker();
        r.app.on_key(key('r'));
        let _ = requests(&r);
        r.to_app
            .send(FromWorker::AudioRestarted(Err("unit not found".into())))
            .expect("send");
        r.app.drain_worker();
        match r.app.setup.as_ref().map(|s| &s.step) {
            Some(SetupStep::Done { message }) => {
                assert!(message.contains("Written"), "{message}");
                assert!(message.contains("unit not found"), "{message}");
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn cancelling_the_confirm_returns_to_the_report_without_writing() {
        // Goal: no is not a write. It goes back to the report it was opened from,
        // which is still read, so nothing is asked of the worker.
        let mut r = playing_hires(chain_with_blocked());
        r.app.on_key(key('A'));
        let _ = requests(&r);
        r.app.on_key(key('n'));
        assert!(r.app.setup.is_none());
        assert_eq!(r.app.mode, Mode::Graph, "back to the report");
        assert!(requests(&r).is_empty(), "nothing written");
    }

    #[test]
    fn a_clock_setting_that_could_not_be_read_is_reported_as_unknown() {
        // Goal: the discipline the whole overlay turns on. A dump that named no
        // permitted rates leaves priel with nothing to advise, and inventing a
        // list to advise against would send the reader to change a setting on
        // no evidence at all.
        let r = playing_hires(chain());
        let text = overlay_text(&r.app);
        let rows = r.app.graph_rows();
        let permitted = rows
            .iter()
            .find(|row| row.label.contains("permitted"))
            .expect("the row is still there, saying what it does not know");
        assert_eq!(permitted.detail, "unknown", "{text}");
        assert!(
            !text.contains("allowed-rates"),
            "no advice from no data: {text}"
        );
        assert!(!text.contains("not permitted"), "{text}");
    }

    #[test]
    fn an_idle_overlay_with_nothing_known_about_the_clock_says_nothing_about_it() {
        // Goal: the same rule the blame sentence follows. With no track and no
        // published setting there is no question and no answer, and a section
        // saying so twice on every idle reading is noise.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain())))
            .expect("send");
        r.app.drain_worker();
        assert!(
            !overlay_text(&r.app).contains("permitted"),
            "{}",
            overlay_text(&r.app)
        );
    }

    #[test]
    fn an_idle_overlay_still_lists_the_rates_the_server_published() {
        // Goal: with nothing playing there is no comparison to make, but what
        // the server may clock at is a fact about the machine and is worth
        // reading before a track starts.
        let mut r = rig();
        r.app.on_key(key('D'));
        r.to_app
            .send(FromWorker::AudioGraph(Ok(chain_clocked(&[44_100], 44_100))))
            .expect("send");
        r.app.drain_worker();
        let text = overlay_text(&r.app);
        assert!(text.contains("permitted"), "{text}");
        assert!(
            !text.contains("this track"),
            "and no row about a track there is none of: {text}"
        );
    }

    #[test]
    fn the_gap_the_chain_left_is_answered_by_the_servers_own_setting() {
        // Goal: the two halves of one diagnosis, in the order they are read.
        // The chain diverges nowhere and the hardware still moved, because the
        // server refused the rate before any node on the path saw a sample.
        // "Nothing on this path did it" is where the reader used to stop.
        let mut r = playing_hires(chain_clocked(&[48_000], 48_000));
        r.app.status.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        let text = overlay_text(&r.app);
        let gap = text.find("nothing on this path").expect("the admitted gap");
        let why = text.find("not permitted").expect("and what explains it");
        assert!(gap < why, "the answer follows the question: {text}");
        assert!(text.contains("default.clock.allowed-rates"), "{text}");
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
        let last = r.app.graph_rows().len() - 1;
        r.app.on_key(key('G'));
        assert_eq!(r.app.graph_offset(), last, "G reaches the last line");
        r.app.on_key(key('j'));
        assert_eq!(r.app.graph_offset(), last, "and stops there");
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
        assert!(
            overlay_text(&r.app).contains("Reading the graph"),
            "and shows no chain until the new answer lands"
        );
    }

    // ---- what has the output device open ----

    /// The same chain with the sound server holding the card it ends on.
    ///
    /// Clocked at the track's own rate so the only `unknown` these tests can
    /// find is the one they are about: the clock section says `unknown` too
    /// when the dump published no rates, and a test that cannot tell the two
    /// apart passes on the wrong answer.
    fn chain_held_by_the_server(card_name: Option<&str>) -> AudioGraph {
        let mut g = chain_clocked(&[44_100], 44_100);
        g.holder = DeviceHolder::Server(HeldDevice {
            sink: "Studio DAC".into(),
            opened_by: Some("wireplumber".into()),
            pcm: Some("hw:2,0".into()),
            card_name: card_name.map(ToString::to_string),
        });
        g
    }

    #[test]
    fn the_overlay_names_the_sound_server_as_what_has_the_device_open() {
        // Goal: the prerequisite for wanting the DAC out of the graph at all.
        // A clean chain is still a chain the server owns and can reshape when
        // the next application starts, so this is reported next to it rather
        // than only when something is wrong.
        let r = playing_hires(chain_held_by_the_server(Some(
            "alsa_card.usb-Studio_DAC-00",
        )));
        let text = overlay_text(&r.app);
        assert!(
            text.contains("Output device"),
            "the section is there: {text}"
        );
        assert!(
            text.contains("the sound server (wireplumber)"),
            "what has it open, named from the dump: {text}"
        );
        assert!(text.contains("hw:2,0"), "and which device that is: {text}");
    }

    #[test]
    fn a_device_the_server_holds_comes_with_what_it_takes_to_reserve_it() {
        // Goal: the second half - the change, where it goes, and what is given
        // up. The trade is not a footnote: reserving the card silences every
        // other application on the machine, and someone who finds that out
        // afterwards was misled.
        let r = playing_hires(chain_held_by_the_server(Some(
            "alsa_card.usb-Studio_DAC-00",
        )));
        let text = overlay_text(&r.app);
        assert!(text.contains("wireplumber.conf.d"), "where: {text}");
        assert!(text.contains("device.disabled = true"), "what: {text}");
        assert!(
            text.contains("alsa_card.usb-Studio_DAC-00"),
            "which card: {text}"
        );
        assert!(
            text.contains("Nothing else on this machine"),
            "what it costs: {text}"
        );
        assert!(
            !text.contains("--exclusive"),
            "taking a free card already has a flag and a toggle: {text}"
        );
    }

    #[test]
    fn a_card_the_graph_did_not_name_gets_no_rule_on_screen() {
        // Goal: the honesty rule again. Knowing the server has the device and
        // knowing which card that is are two facts, and a rule matching a name
        // priel invented would disable something that was working.
        let r = playing_hires(chain_held_by_the_server(None));
        let text = overlay_text(&r.app);
        assert!(
            text.contains("the sound server"),
            "the holder is still named: {text}"
        );
        assert!(
            !text.contains("monitor.alsa.rules"),
            "and nothing is guessed at: {text}"
        );
    }

    #[test]
    fn a_holder_the_graph_cannot_determine_is_reported_as_unknown() {
        // Goal: an undeterminable owner is reported as undeterminable. The
        // nearest card on the machine is a guess, and the reader acts on this
        // by disabling something.
        let mut g = chain_clocked(&[44_100], 44_100);
        g.holder = DeviceHolder::Unknown {
            sink: "Studio DAC".into(),
        };
        let r = playing_hires(g);
        let text = overlay_text(&r.app);
        assert!(text.contains("held by  unknown"), "{text}");
        assert!(
            !text.contains("wireplumber.conf.d"),
            "nothing to advise about something unidentified: {text}"
        );
    }

    #[test]
    fn a_chain_that_reaches_no_device_says_that_rather_than_unknown() {
        // Goal: "nothing is at the end of this chain" is a different finding
        // from "something is and priel cannot tell what". Reporting the first
        // as the second puts an unanswerable question where there is no
        // question.
        let mut g = chain_clocked(&[44_100], 44_100);
        g.holder = DeviceHolder::NoDevice;
        let r = playing_hires(g);
        let text = overlay_text(&r.app);
        assert!(
            text.contains("no output device"),
            "the chain reaches none: {text}"
        );
        let held = r
            .app
            .graph_rows()
            .into_iter()
            .find(|row| row.label.trim() == "held by")
            .expect("the ownership section names a holder");
        assert_ne!(
            held.detail, "unknown",
            "which is not the same finding: {text}"
        );
    }

    #[test]
    fn a_device_priel_holds_itself_is_named_and_advised_about_nothing() {
        // Goal: on the direct path the device is already priel's alone, so
        // there is nothing to reserve. Advice printed over the destination
        // teaches the reader to ignore the section - and the overlay still owes
        // them which device it is that priel has.
        let mut r = rig();
        r.app.status.audio_device = "alsa/hw:CARD=AUDIO,DEV=0".into();
        r.app.on_key(key('D'));
        let text = overlay_text(&r.app);
        assert!(text.contains("Output device"), "{text}");
        assert!(text.contains("priel"), "who has it: {text}");
        assert!(
            text.contains("alsa/hw:CARD=AUDIO,DEV=0"),
            "and which device that is: {text}"
        );
        assert!(
            !text.contains("wireplumber.conf.d"),
            "there is nothing left to hand over: {text}"
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
    fn the_exclusive_toggle_is_remembered_and_says_so() {
        // Goal: taking a device from every other application on the machine is
        // the one setting a user must never be surprised by on the next start,
        // so the toggle says in words that it will be there. `x` settles on a
        // value; that value is what is written, not each flip on the way.
        let mut r = with_picker("pipewire/dac");
        r.app.notice = None;
        r.app.on_key(key('x'));
        assert_eq!(r.app.chosen().exclusive, Some(true));
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("kept"),
            "the notice says the choice outlives the session: {notice}"
        );
        r.app.on_key(key('x'));
        assert_eq!(
            r.app.chosen().exclusive,
            Some(false),
            "giving the device back is remembered too"
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
    fn choosing_a_device_closes_the_picker_and_remembers_the_choice() {
        // Goal: the identifier is the thing nobody wants to type twice, so the
        // picker records it for `main` to write out. It is the identifier that
        // is kept, never the description - only the first is what --device and
        // the file accept.
        let mut r = with_picker("auto");
        r.app.on_key(key('j'));
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.mode, Mode::Normal, "choosing closes the picker");
        assert_eq!(r.app.chosen().device.as_deref(), Some("pipewire/dac"));
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("pipewire/dac description"),
            "the chosen device is named: {notice}"
        );
        assert!(
            notice.contains("kept"),
            "and the choice outlives the session: {notice}"
        );
    }

    #[test]
    fn a_session_that_used_no_picker_has_nothing_to_remember() {
        // Goal: a value that came from a flag is for that run alone. Persisting
        // it would quietly make a one-off permanent, which is the surprise this
        // design exists to avoid - so `chosen` starts empty and only a picker
        // fills it.
        let mut r = rig();
        r.app.set_theme(ThemeName::Dracula);
        r.app.exclusive = true;
        assert!(
            r.app.chosen().is_empty(),
            "nothing was chosen: {:?}",
            r.app.chosen()
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

    // ---- the colour theme picker ----

    /// The picker, open, with the default palette in force.
    fn with_themes() -> Rig {
        let mut r = rig();
        r.app.on_key(key('t'));
        r
    }

    #[test]
    fn the_theme_picker_opens_on_its_key_and_closes_again() {
        // Goal: `t` is the binding, and the three ways out of every other
        // overlay have to work here too - a second idiom would be its own bug.
        for out in ['t', 'q'] {
            let mut r = with_themes();
            assert_eq!(r.app.mode, Mode::Themes);
            r.app.on_key(key(out));
            assert_eq!(r.app.mode, Mode::Normal, "{out} should close it");
        }
        let mut r = with_themes();
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal, "Esc should close it");
    }

    #[test]
    fn the_theme_picker_opens_on_the_palette_already_in_force() {
        // Goal: a list opens on the row that was last touched, and the device
        // picker already does exactly this.
        let mut r = rig();
        r.app.set_theme(ThemeName::OneLight);
        r.app.on_key(key('t'));
        assert_eq!(r.app.theme_selected(), 3, "one-light is the fourth offered");
    }

    #[test]
    fn choosing_a_theme_repaints_and_remembers_the_choice() {
        // Goal: the palette is the setting a listener changes once and expects
        // to find again, so the picker records it and says so. Both halves
        // matter: a picker that kept nothing used to have to apologise in its
        // own footer.
        let mut r = with_themes();
        r.app.on_key(key('j'));
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.theme(), Theme::of(ThemeName::GruvboxDark));
        assert_eq!(r.app.mode, Mode::Normal, "choosing closes the picker");
        assert_eq!(r.app.chosen().theme, Some(ThemeName::GruvboxDark));
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("gruvbox-dark"), "{notice}");
        assert!(notice.contains("kept"), "{notice}");
    }

    #[test]
    fn clicking_a_theme_row_does_what_the_enter_key_does() {
        // Goal: the mouse is a first-class addition, never a second
        // implementation. Both paths run `choose_theme` and nothing else.
        let mut r = with_themes();
        let row = Rect {
            x: 2,
            y: 5,
            width: 60,
            height: 1,
        };
        r.app.theme_rows = vec![(row, ThemeName::GruvboxLight)];
        r.app.on_mouse(click(4, 5));
        assert_eq!(r.app.theme(), Theme::of(ThemeName::GruvboxLight));
        assert_eq!(r.app.mode, Mode::Normal);
    }

    #[test]
    fn a_click_outside_the_theme_rows_dismisses_without_repainting() {
        // Goal: missing a row must not change the palette, exactly as missing a
        // device row must not move the output.
        let mut r = with_themes();
        r.app.notice = None;
        r.app.theme_rows = vec![(
            Rect {
                x: 2,
                y: 5,
                width: 60,
                height: 1,
            },
            ThemeName::Terminal,
        )];
        r.app.on_mouse(click(4, 9));
        assert_eq!(r.app.mode, Mode::Normal);
        assert_eq!(r.app.theme(), Theme::default(), "nothing was chosen");
        assert!(r.app.notice.is_none());
    }

    #[test]
    fn the_theme_picker_scrolls_with_the_same_keys_as_a_list() {
        // Goal: j/k/g/G mean here what they mean everywhere else, and both ends
        // are clamped rather than wrapping.
        let mut r = with_themes();
        let last = crate::theme::OFFERED.len() - 1;
        r.app.on_key(key('j'));
        assert_eq!(r.app.theme_selected(), 1);
        r.app.on_key(key('k'));
        assert_eq!(r.app.theme_selected(), 0);
        r.app.on_key(key('k'));
        assert_eq!(r.app.theme_selected(), 0, "and stops at the first");
        r.app.on_key(key('G'));
        assert_eq!(r.app.theme_selected(), last, "G reaches the last");
        r.app.on_key(key('j'));
        assert_eq!(r.app.theme_selected(), last, "and stops there");
        r.app.on_key(key('g'));
        assert_eq!(r.app.theme_selected(), 0);
    }

    #[test]
    fn the_theme_picker_swallows_the_keys_behind_it() {
        // Goal: modal in the same way every other overlay is. `s` would shuffle
        // the list underneath, and must not reach it.
        let mut r = with_themes();
        r.app.on_key(key('s'));
        assert!(
            !r.app.shuffle,
            "the list behind it must not have been touched"
        );
        assert_eq!(r.app.mode, Mode::Themes, "and it is still open");
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
        assert_eq!(r.app.view, View::Mixes, "the mixes are the fourth tab");
        assert_eq!(r.app.mode, Mode::Normal, "and leaves the input mode behind");
        r.app.on_key(code(KeyCode::Tab));
        assert_eq!(r.app.view, View::Favorites, "Tab wraps around");
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

    // ---- the mixes view ----

    fn mixes_page(offset: u32, ids: &[&str], total: u32) -> FromWorker {
        FromWorker::Mixes {
            offset,
            page: priel_core::Page {
                items: ids.iter().map(|i| mix(i, i)).collect(),
                total,
            },
        }
    }

    fn mix_tracks_page(
        mix_id: &str,
        offset: u32,
        ids: std::ops::Range<u64>,
        total: u32,
    ) -> FromWorker {
        FromWorker::MixTracks {
            mix_id: mix_id.into(),
            offset,
            page: track_page(ids, total),
        }
    }

    #[test]
    fn the_fourth_key_opens_the_mixes_view_and_asks_for_its_first_page() {
        // Goal: the mixes get a tab of their own rather than sharing the
        // playlists, because they are not editable and they are rebuilt under
        // the listener. A view nobody can reach is not a view.
        let mut r = rig();
        r.app.on_key(key('4'));
        assert_eq!(r.app.view, View::Mixes);
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadMixes { offset: 0, .. }]
        ));
    }

    #[test]
    fn coming_back_to_the_mixes_asks_the_service_again() {
        // Goal: the one place this listing differs from the others, and the
        // reason it is a view of its own. A playlist the user wrote is still
        // what they left, so it is fetched once; a mix is rebuilt by the service
        // without anyone asking, so a copy held from the last visit is stale by
        // construction. Leaving and coming back is the cheapest honest refresh
        // there is, and it costs one request.
        let mut r = rig();
        r.app.on_key(key('4'));
        r.to_app.send(mixes_page(0, &["m1"], 1)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        r.app.on_key(key('1'));
        r.app.on_key(key('4'));
        assert!(
            matches!(requests(&r)[..], [ToWorker::LoadMixes { offset: 0, .. }]),
            "a mix listing is never reused across a visit"
        );
    }

    #[test]
    fn opening_a_mix_loads_its_tracks_and_escape_comes_back_to_the_mixes() {
        // Goal: the second nested view, and Esc has to know which list it came
        // from. Sending it back to the playlists - the only thing Esc used to do
        // - would strand the listener somewhere they never were.
        let mut r = rig();
        r.app.mixes = vec![mix("0007a", "My Mix 1")];
        r.app.view = View::Mixes;
        r.app.on_key(code(KeyCode::Enter));

        assert_eq!(r.app.view, View::MixTracks);
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadMixTracks { mix_id: ref m, offset: 0, .. }] if m == "0007a"
        ));

        r.to_app.send(mix_tracks_page("0007a", 0, 4..7, 3)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.mix_tracks), vec![4, 5, 6]);

        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.view, View::Mixes);
    }

    #[test]
    fn a_mix_that_is_not_open_has_its_tracks_dropped() {
        // Goal: the identity guard the playlist tracks already have. A page for
        // the mix the listener has just left would otherwise be poured into the
        // one they have just opened.
        let mut r = rig();
        r.app.mixes = vec![mix("a", "A"), mix("b", "B")];
        r.app.view = View::Mixes;
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.to_app.send(mix_tracks_page("b", 0, 1..3, 2)).unwrap();
        r.app.drain_worker();
        assert!(
            r.app.mix_tracks.is_empty(),
            "rows for another mix must not land here"
        );
    }

    #[test]
    fn playing_inside_a_mix_builds_the_queue_from_the_rows_on_screen() {
        // Goal: a mix plays like any other listing. The queue comes from the
        // *visible* rows, so a local filter has to narrow it here too - that
        // indirection is the one this view could most easily get wrong, since
        // its rows arrive from a listing nobody curated.
        let mut r = rig();
        r.app.mixes = vec![mix("0007a", "My Mix 1")];
        r.app.view = View::Mixes;
        r.app.on_key(code(KeyCode::Enter));
        r.to_app.send(mix_tracks_page("0007a", 0, 1..4, 3)).unwrap();
        r.app.drain_worker();
        let _ = requests(&r);

        r.app.selected = 1;
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(ids(&r.app.queue), vec![1, 2, 3]);
        assert_eq!(r.app.queue_pos, 1, "and starts on the row that was chosen");
    }

    #[test]
    fn a_failed_mix_page_stops_that_listing_and_leaves_the_others_alone() {
        // Goal: the failure identity that #7 established. A mixes page that dies
        // must latch the mixes and nothing else, or a service that is refusing
        // one listing quietly stops another that is fine.
        let mut r = rig();
        r.app.on_key(key('4'));
        let _ = requests(&r);
        r.to_app
            .send(FromWorker::Failed {
                task: Task::Mixes { offset: 0 },
                fault: Fault::Refused,
                detail: "mixes: no".into(),
            })
            .unwrap();
        r.app.drain_worker();

        assert!(r.app.mixes_paging.stalled, "the mixes gave up");
        assert!(
            !r.app.favorites_paging.stalled,
            "and took no other listing with them"
        );

        // `r` is the retry, and it has to clear the latch.
        r.app.on_key(key('r'));
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadMixes { offset: 0, .. }]
        ));
        assert!(!r.app.mixes_paging.stalled);
    }

    #[test]
    fn scrolling_a_mix_to_the_end_asks_for_the_next_page_and_appends_it() {
        // Goal: a mix pages like every other listing. Its own row carries no
        // track count, unlike a playlist's, so the total can only come from the
        // first page of tracks - which makes the service's count the sole
        // end-of-list signal here.
        let mut r = rig();
        r.app.mixes = vec![mix("0007a", "My Mix 1")];
        r.app.view = View::Mixes;
        r.app.on_key(code(KeyCode::Enter));
        r.to_app.send(mix_tracks_page("0007a", 0, 0..3, 6)).unwrap();
        r.app.drain_worker();
        scrolled_to_the_end(&mut r.app);
        let _ = requests(&r);

        r.app.refresh();
        assert!(matches!(
            requests(&r)[..],
            [ToWorker::LoadMixTracks { offset: 3, .. }]
        ));

        r.to_app.send(mix_tracks_page("0007a", 3, 3..6, 6)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.mix_tracks), vec![0, 1, 2, 3, 4, 5]);
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

    // ---- carrying on past the end of the queue ----

    /// A track that names the radio mix the service builds around it.
    fn track_with_radio(id: u64, mix_id: &str) -> Track {
        Track {
            mix_id: mix_id.into(),
            ..track(id, "T", "A")
        }
    }

    /// The last track of a queue, playing and settled: the one moment at which
    /// "what comes after this?" has no answer.
    fn on_the_last_track(r: &mut Rig, tracks: Vec<Track>) {
        let last = tracks.len() - 1;
        let playing = tracks[last].clone();
        r.app.queue = tracks;
        r.app.queue_pos = last;
        r.app.expected_id = playing.id;
        r.app.status.current_id = playing.id;
        r.app.now_playing = Some(playing);
        r.app.status.playing = true;
        let _ = requests(r);
    }

    /// What was asked of the radio this tick.
    fn radios_asked(r: &Rig) -> Vec<String> {
        requests(r)
            .into_iter()
            .filter_map(|c| match c {
                ToWorker::LoadRadio { mix_id, .. } => Some(mix_id),
                _ => None,
            })
            .collect()
    }

    fn radio_page(mix_id: &str, ids: std::ops::Range<u64>) -> FromWorker {
        FromWorker::Radio {
            mix_id: mix_id.into(),
            page: track_page(ids, 0),
        }
    }

    #[test]
    fn the_queue_carries_on_with_the_radio_by_default() {
        // Goal: the default keeps the music going. A player that stops dead at
        // the end of a chosen queue is a surprising silence; the service's own
        // suggestions carry it on without the listener asking.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        assert!(r.app.continue_radio, "on until it is turned off");
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
    }

    #[test]
    fn turning_it_off_ends_the_queue_where_it_ends() {
        // Goal: the toggle's other direction. Off, the last track the listener
        // chose is the end and nothing is asked for.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.on_key(key('c')); // on by default, so this turns it off
        assert!(!r.app.continue_radio);
        r.app.refresh_for_test();
        assert!(radios_asked(&r).is_empty());
    }

    #[test]
    fn the_end_of_the_queue_asks_for_the_radio_of_the_track_that_ended_it() {
        // Goal: the mix is the one built around the track that just played, not
        // one chosen from a listing, which is why every row has to carry its own
        // id for it.
        let mut r = rig();
        on_the_last_track(
            &mut r,
            vec![track(1, "T", "A"), track_with_radio(2, "0016d")],
        );
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
    }

    #[test]
    fn the_radio_is_asked_for_once_however_many_ticks_the_track_lasts() {
        // Goal: the preload decision fires on every tick for as long as nothing
        // is queued behind the current track - which at the end of a queue is
        // every tick of the last one. Without a guard that is ten requests a
        // second for the length of a track.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        for _ in 0..5 {
            r.app.refresh_for_test();
        }
        assert_eq!(radios_asked(&r).len(), 1);
    }

    #[test]
    fn a_track_with_no_radio_ends_the_queue_and_says_why() {
        // Goal: absence is an ordinary answer - a mix's own rows are a shorter
        // shape than the other listings send - and stopping without a word
        // looks like the failure it is not.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track(1, "So What", "A")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        assert!(radios_asked(&r).is_empty(), "there is nothing to ask for");
        let said = r.app.notice.clone().unwrap_or_default();
        assert!(said.contains("So What"), "it names the track: {said}");
        assert!(said.contains("radio"), "and says what is missing: {said}");
    }

    #[test]
    fn the_radio_extends_the_queue_and_the_preload_path_takes_it_from_there() {
        // Goal: the transition into the radio is the gapless transition that
        // already exists. The queue grows, and the next entry goes into mpv's
        // playlist through the same preload that carries every other track
        // change - not through a second mechanism beside it.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        let _ = requests(&r);

        r.to_app.send(radio_page("0016d", 20..23)).unwrap();
        r.app.drain_worker();
        assert_eq!(
            ids(&r.app.queue),
            vec![1, 20, 21, 22],
            "the queue grew rather than being replaced"
        );
        assert_eq!(r.app.next_intended, Some(20));
        assert_eq!(
            resolved_ids(&requests(&r)),
            vec![20],
            "the ordinary preload"
        );
    }

    #[test]
    fn the_track_the_radio_was_built_around_does_not_play_twice() {
        // Goal: a track's own mix leads with that track. Appending it unchanged
        // replays what just finished, which reads as a bug rather than as a
        // suggestion.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(7, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        r.to_app
            .send(FromWorker::Radio {
                mix_id: "0016d".into(),
                page: priel_core::Page {
                    items: vec![track(7, "T", "A"), track(8, "T", "A")],
                    total: 0,
                },
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.queue), vec![7, 8]);
    }

    #[test]
    fn what_the_radio_added_is_marked_as_a_suggestion_and_the_rest_is_not() {
        // Goal: the interface has to say that what is playing is the service's
        // idea rather than the listener's. The mark is positional - everything
        // from where the queue was extended onwards - so it survives a skip
        // backwards into what was actually chosen.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        r.to_app.send(radio_page("0016d", 20..22)).unwrap();
        r.app.drain_worker();

        assert!(!r.app.playing_from_radio(), "still on what was chosen");
        r.app.queue_pos = 1;
        assert!(r.app.playing_from_radio(), "and now on what was suggested");
        r.app.queue_pos = 0;
        assert!(!r.app.playing_from_radio(), "back on what was chosen");
    }

    #[test]
    fn a_radio_that_lands_after_the_music_stopped_still_starts_it() {
        // Goal: the request goes out while the last track is still playing, so
        // it normally arrives in time to be preloaded. When it does not, the
        // queue has grown behind a fallback that has already fired and set its
        // guard. Clearing that guard is what lets the *existing* end-of-track
        // decision start the radio on the next tick; reaching past it to load a
        // track here would be the second mechanism this must not grow.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();

        r.app.status.playing = false;
        r.app.status.ended = true;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 0, "nowhere to go yet");

        r.to_app.send(radio_page("0016d", 20..22)).unwrap();
        r.app.drain_worker();
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 1, "the fallback took the new track");
        assert_eq!(r.app.now_playing.as_ref().map(|t| t.id), Some(20));
    }

    #[test]
    fn a_radio_that_could_not_be_fetched_says_so_and_does_not_spin() {
        // Goal: a failure here is not worth retrying ten times a second for the
        // rest of the track. It is reported once and the queue ends, which is
        // exactly what it would have done without the toggle.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::Failed {
                task: Task::Radio {
                    mix_id: "0016d".into(),
                },
                fault: Fault::Unreachable,
                detail: "the connection died".into(),
            })
            .unwrap();
        r.app.drain_worker();
        assert!(r.app.notice.is_some());
        for _ in 0..5 {
            r.app.refresh_for_test();
        }
        assert!(radios_asked(&r).is_empty(), "it does not ask again");
    }

    #[test]
    fn a_radio_for_a_track_that_is_no_longer_playing_is_dropped() {
        // Goal: replies are correlated by what they answer, never by the order
        // they arrive in. A radio asked for at the end of one queue can land
        // after the listener has started another, and appending it there
        // extends a queue nobody asked to extend, with a mix built round a
        // track that stopped playing before it turned up.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();

        on_the_last_track(&mut r, vec![track_with_radio(2, "0099f")]);
        r.to_app.send(radio_page("0016d", 20..23)).unwrap();
        r.app.drain_worker();
        assert_eq!(ids(&r.app.queue), vec![2], "the stale radio is dropped");
    }

    #[test]
    fn turning_it_off_and_on_again_asks_the_radio_again() {
        // Goal: a radio is asked about once per track, and turning the toggle
        // back on is a listener asking for the thing that did not work. Holding
        // the old answer would make a failed radio unretryable without either
        // skipping a track or restarting priel.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.refresh_for_test(); // on by default: asked once
        assert_eq!(radios_asked(&r).len(), 1);
        r.app.refresh_for_test();
        assert!(radios_asked(&r).is_empty(), "not while it stays on");

        r.app.on_key(key('c')); // off
        r.app.on_key(key('c')); // on again
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r).len(), 1, "asked for again");
    }

    #[test]
    fn a_queue_the_radio_built_is_not_endless() {
        // Goal: everything that grows from what arrives from outside gets a
        // bound. This one grows every time it runs out, so without a ceiling a
        // player left running overnight holds a vector nobody will ever reach
        // the end of.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.queue = (1..=RADIO_MAX as u64).map(|i| track(i, "T", "A")).collect();
        r.app.queue[RADIO_MAX - 1].mix_id = "0016d".into();
        r.app.queue_pos = RADIO_MAX - 1;
        r.app.refresh_for_test();
        assert!(
            radios_asked(&r).is_empty(),
            "a full queue does not ask for what it cannot hold"
        );
        assert!(
            r.app.notice.is_some(),
            "and it says so rather than stopping silently"
        );

        // And a page that was already in flight when it filled up cannot get
        // past the ceiling either.
        r.to_app.send(radio_page("0016d", 9000..9010)).unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.queue.len(), RADIO_MAX, "there was no room left");
    }

    #[test]
    fn a_new_queue_is_a_new_choice_and_forgets_the_radio() {
        // Goal: the queue is a snapshot taken when the listener pressed Enter.
        // Building a new one throws away what the last one was extended with,
        // and the mark that said so with it.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        r.to_app.send(radio_page("0016d", 20..22)).unwrap();
        r.app.drain_worker();
        r.app.queue_pos = 1;
        assert!(r.app.playing_from_radio());

        // The same track chosen again, with two ahead of it this time, so a
        // mark left over from the last queue would call this row a suggestion.
        r.app.favorites = vec![
            track(5, "A", "X"),
            track(6, "B", "Y"),
            track_with_radio(1, "0016d"),
        ];
        r.app.view = View::Favorites;
        r.app.selected = 2;
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(ids(&r.app.queue), vec![5, 6, 1]);
        assert_eq!(r.app.queue_pos, 2);
        assert!(!r.app.playing_from_radio(), "nothing here was suggested");

        // And the same track is asked about again: what was already asked is
        // part of the queue that asked it.
        r.app.current_target = None;
        r.app.status.playing = true;
        r.app.status.current_id = 1;
        let _ = requests(&r);
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
    }

    #[test]
    fn carrying_on_answers_to_a_click_and_to_its_key_alike() {
        // Goal: parity runs both ways, and the only way to keep the two paths
        // from drifting is for both to run the same method.
        let mut r = rig();
        r.app.on_key(key('c'));
        assert!(!r.app.continue_radio, "the key turns it off");
        r.app.dispatch(Hit::Continue);
        assert!(r.app.continue_radio, "and the control turns it back on");
        assert!(
            r.app.notice.clone().unwrap_or_default().contains("adio"),
            "each says which way it went: {:?}",
            r.app.notice
        );
    }

    // ---- repeating: no repeat, repeat all, repeat one ----

    /// What the queue asked to have resolved this tick, having settled first.
    fn preloaded(r: &Rig) -> Vec<u64> {
        resolved_ids(&requests(r))
    }

    #[test]
    fn the_three_repeat_states_cycle_from_a_key_and_from_the_control() {
        // Goal: one action with two ways in. Three states cannot be a boolean,
        // so the key cycles rather than toggling, and the control has to cycle
        // through the very same method or the two drift apart.
        let mut r = rig();
        assert_eq!(r.app.repeat, Repeat::Off, "off until it is asked for");
        r.app.on_key(key('e'));
        assert_eq!(r.app.repeat, Repeat::All);
        r.app.dispatch(Hit::Repeat);
        assert_eq!(r.app.repeat, Repeat::One, "the control carries on cycling");
        r.app.on_key(key('e'));
        assert_eq!(r.app.repeat, Repeat::Off, "and round to the start");
        assert!(
            r.app.notice.clone().unwrap_or_default().contains("epeat"),
            "each step says where it landed: {:?}",
            r.app.notice
        );
    }

    #[test]
    fn repeat_all_starts_the_queue_again_at_its_top() {
        // Goal: the end of a repeating queue is its own beginning, and it has to
        // arrive through the ordinary gapless preload rather than a second
        // mechanism - the preload is what makes the wrap seamless.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track(1, "A", "X"), track(2, "B", "Y")]);
        r.app.set_repeat(Repeat::All);
        r.app.refresh_for_test();
        assert_eq!(preloaded(&r), vec![1], "the top of the queue follows it");
    }

    #[test]
    fn repeat_one_preloads_the_track_that_is_playing_again() {
        // Goal: preloading the same track is not preloading the next one, and it
        // still has to be a preload: mpv's playlist gets a second entry for it,
        // so the repeat is gapless like any other transition.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track(1, "A", "X"), track(2, "B", "Y")]);
        r.app.queue_pos = 0;
        r.app.set_repeat(Repeat::One);
        r.app.refresh_for_test();
        assert_eq!(preloaded(&r), vec![1], "itself, not the track after it");
    }

    #[test]
    fn a_queue_of_one_repeats_the_same_way_under_both_states() {
        // Goal: with one track, "start the queue again" and "play this again"
        // are the same instruction. That has to be true on purpose rather than
        // by accident, because it is the case a wrap-around gets wrong.
        for repeat in [Repeat::All, Repeat::One] {
            let mut r = rig();
            on_the_last_track(&mut r, vec![track(1, "Alone", "X")]);
            r.app.set_repeat(repeat);
            r.app.refresh_for_test();
            assert_eq!(preloaded(&r), vec![1], "{repeat:?}");
        }
    }

    #[test]
    fn a_repeating_queue_never_reaches_the_radio() {
        // Goal: a queue set to repeat has no end, so there is nothing for the
        // radio to continue from. The two toggles stay independent - neither
        // writes to the other - and this falls out of the repeat always having
        // somewhere to go, which is the only place the radio is ever asked.
        for repeat in [Repeat::All, Repeat::One] {
            let mut r = rig();
            on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
            r.app.continue_radio = true;
            r.app.set_repeat(repeat);
            r.app.refresh_for_test();
            assert!(radios_asked(&r).is_empty(), "{repeat:?}");
            assert!(r.app.continue_radio, "and the radio was not turned off");
        }

        // The guard under test, removed: the same queue with no repeat does ask.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track_with_radio(1, "0016d")]);
        r.app.continue_radio = true;
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
    }

    #[test]
    fn repeat_one_outranks_the_shuffle_and_repeat_all_defers_to_it() {
        // Goal: the two interactions worth writing down. The shuffle says what
        // the play order is and the repeat says whether it ends, so repeat-one
        // outranks the shuffle - there is no next track to pick when the answer
        // is this one again - and repeat-all defers to it, because a shuffle
        // that picks afresh on every advance has no end to start again from.
        // That second half is a consequence of today's shuffle having no order
        // at all, not a rule of its own: give the shuffle a real order and its
        // exhausted case falls through to the same repeat question the ordered
        // end asks. This test asserts the shape that holds either way.
        let mut r = rig();
        r.app.queue = (1..=4).map(|i| track(i, "T", "A")).collect();
        r.app.queue_pos = 2;
        r.app.shuffle = true;

        r.app.set_repeat(Repeat::One);
        for _ in 0..25 {
            assert_eq!(r.app.next_pos(Repeat::One), Some(2), "this one again");
        }

        r.app.set_repeat(Repeat::All);
        for _ in 0..25 {
            let p = r.app.next_pos(Repeat::All).expect("somewhere to go");
            assert_ne!(p, 2, "the shuffle still moves somewhere else");
        }
    }

    #[test]
    fn the_end_of_track_fallback_honours_the_repeat_too() {
        // Goal: the fallback is what carries a track whose preload never
        // arrived, and it has to answer the same question the preload does - one
        // rule with two callers, or the two disagree about where the queue goes
        // exactly when something has already gone wrong.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track(1, "A", "X"), track(2, "B", "Y")]);
        r.app.set_repeat(Repeat::All);
        r.app.status.ended = true;
        r.app.status.playing = false;
        r.app.refresh_for_test();
        assert_eq!(r.app.queue_pos, 0, "back to the top from scratch");
    }

    #[test]
    fn a_deliberate_skip_leaves_a_track_that_is_repeating() {
        // Goal: repeat-one that could not be skipped out of is a trap with no
        // key to leave it. The natural end of the track repeats it; pressing
        // `L` says otherwise, so it moves on - and the entry mpv already has
        // preloaded is this same track, so the preloaded one must not be taken.
        let mut r = rig();
        r.app.queue = vec![track(1, "A", "X"), track(2, "B", "Y")];
        r.app.queue_pos = 0;
        r.app.now_playing = Some(track(1, "A", "X"));
        r.app.expected_id = 1;
        r.app.status.has_next = true;
        r.app.set_repeat(Repeat::One);
        let _ = requests(&r);

        r.app.on_key(key('L'));
        assert_eq!(r.app.queue_pos, 1, "the skip goes to the next track");
        assert_eq!(
            preloaded(&r),
            vec![2],
            "loaded from scratch, not skipped to"
        );
    }

    #[test]
    fn the_repeat_does_not_reach_into_what_mpv_already_has() {
        // Goal: the next entry is in mpv's playlist by the time this is pressed,
        // and a second path into that playlist is the one thing the gapless
        // pipeline must not grow - the same reasoning `toggle_continue` records.
        // So a change of repeat is answered by the next preload decision, and
        // asks for nothing on the tick it happens.
        let mut r = rig();
        on_the_last_track(&mut r, vec![track(1, "A", "X"), track(2, "B", "Y")]);
        r.app.status.has_next = true;
        r.app.set_repeat(Repeat::One);
        r.app.refresh_for_test();
        assert!(preloaded(&r).is_empty(), "mpv has one ready already");
    }

    // ---- the shuffle's play order ----

    /// The queue in the order it will be played, by track id.
    fn play_order(app: &App) -> Vec<u64> {
        (0..app.queue.len())
            .filter_map(|row| app.queue_at(row))
            .filter_map(|i| app.queue.get(i).map(|t| t.id))
            .collect()
    }

    /// The ids of a queue of `n` tracks in the order they were listed in.
    fn listing(n: u64) -> Vec<u64> {
        (1..=n).collect()
    }

    /// A queue of `n` tracks playing the first of them with the shuffle on:
    /// the two keys a listener presses, in the order they press them.
    fn playing_shuffled(n: u64) -> Rig {
        let mut r = rig();
        r.app.favorites = (1..=n).map(|i| track(i, "T", "A")).collect();
        r.app.on_key(code(KeyCode::Enter));
        r.app.on_key(key('s'));
        // The resolve for the first track has landed, so nothing is in flight.
        r.app.current_target = None;
        let _ = requests(&r);
        r
    }

    /// The same, but with the shuffle already on when a row further down the
    /// listing is chosen - so the head of the order is not queue entry zero and
    /// a row of the order cannot be mistaken for an index into the listing.
    fn playing_shuffled_from(n: u64, chosen: usize) -> Rig {
        let mut r = rig();
        r.app.favorites = (1..=n).map(|i| track(i, "T", "A")).collect();
        r.app.shuffle = true;
        r.app.selected = chosen;
        r.app.on_key(code(KeyCode::Enter));
        r.app.current_target = None;
        let _ = requests(&r);
        r
    }

    /// Take the next entry of the order, as an advance does. `false` where the
    /// order has nothing left.
    fn play_on(r: &mut Rig) -> bool {
        let Some(p) = r.app.next_pos(Repeat::Off) else {
            return false;
        };
        r.app.load_fresh(p);
        r.app.current_target = None;
        let _ = requests(r);
        true
    }

    #[test]
    fn the_shuffle_lays_an_order_beside_the_queue_rather_than_reordering_it() {
        // Goal: the permutation is the whole design. The queue stays the set of
        // tracks the listener chose in the order they chose it - which is what
        // there is to go back to when the shuffle is turned off - and a second
        // structure says what follows what.
        let r = playing_shuffled(12);
        assert_eq!(
            r.app.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
            listing(12),
            "the queue itself was not touched"
        );

        let order = play_order(&r.app);
        let mut covered = order.clone();
        covered.sort_unstable();
        assert_eq!(covered, listing(12), "every track exactly once: {order:?}");
        assert_eq!(order[0], 1, "the track playing stays at the head of it");
        assert_ne!(order, listing(12), "and the rest is not the listing");
    }

    #[test]
    fn no_track_repeats_until_every_other_one_has_played() {
        // Goal: the fresh draw at every advance was random *with replacement*,
        // so a ten-track queue would usually repeat several tracks before
        // covering them all. Walking an order cannot: it ends when it is spent.
        let mut r = playing_shuffled(8);
        let mut played = vec![r.app.queue[r.app.queue_pos].id];
        for _ in 0..8 {
            if !play_on(&mut r) {
                break;
            }
            played.push(r.app.queue[r.app.queue_pos].id);
            assert_eq!(
                r.app.queue_indicator(),
                Some(format!("{}/8", played.len())),
                "the counter counts rows of the order, not of the listing"
            );
        }
        let mut covered = played.clone();
        covered.sort_unstable();
        assert_eq!(covered, listing(8), "each one once, none twice: {played:?}");
    }

    #[test]
    fn the_order_stands_still_between_advances_so_the_panel_can_show_it() {
        // Goal: what the queue panel draws below the current track has to be
        // what will play. A pick drawn afresh at every advance would make the
        // row under the current one a different track each time it was asked.
        let r = playing_shuffled(10);
        let next = r
            .app
            .next_pos(Repeat::Off)
            .expect("nine rows still to play");
        for _ in 0..25 {
            assert_eq!(
                r.app.next_pos(Repeat::Off),
                Some(next),
                "asking again must not draw again"
            );
        }
        assert_eq!(
            r.app.queue_at(r.app.playing_row() + 1),
            Some(next),
            "and it is the row the panel puts under the current one"
        );
    }

    #[test]
    fn the_preload_asks_for_the_track_the_order_will_actually_play() {
        // Goal: the preload used to be a guess. The shuffle picked afresh at the
        // advance, so the track fetched during the last minute of the current
        // one was usually not the track that turned up - a whole track
        // downloaded for nothing, and a gap where the gapless handover should
        // have been. With an order the two are the same question.
        let mut r = playing_shuffled(10);
        for round in 0..3 {
            let expected = r
                .app
                .queue_at(r.app.playing_row() + 1)
                .and_then(|p| r.app.queue.get(p).map(|t| t.id));
            r.app.status.playing = true;
            r.app.status.ended = false;
            r.app.refresh_for_test();
            let asked = preloaded(&r);
            assert_eq!(asked, expected.into_iter().collect::<Vec<_>>(), "{round}");

            // The track ends before mpv took the preload, so the fallback loads
            // the next one from scratch: the same one, or it was a guess.
            r.app.status.ended = true;
            r.app.status.playing = false;
            r.app.refresh_for_test();
            r.app.current_target = None;
            let _ = requests(&r);
            assert_eq!(
                r.app.now_playing.as_ref().map(|t| t.id),
                asked.first().copied(),
                "what was preloaded is what plays, round {round}"
            );
        }
    }

    #[test]
    fn turning_the_shuffle_off_goes_back_to_the_listing_still_playing() {
        // Goal: the one thing shuffling the queue in place cannot do. The
        // listing order is still there because it was never overwritten, so `s`
        // returns to it without the music stopping or changing track.
        let mut r = playing_shuffled(10);
        play_on(&mut r);
        play_on(&mut r);
        assert_ne!(play_order(&r.app), listing(10), "shuffled while it is on");
        let playing = r.app.now_playing.as_ref().map(|t| t.id);

        r.app.on_key(key('s'));
        assert!(!r.app.shuffle);
        assert_eq!(
            r.app.now_playing.as_ref().map(|t| t.id),
            playing,
            "the same track is still in the speakers"
        );
        assert_eq!(play_order(&r.app), listing(10), "back to the listing");
        assert_eq!(
            r.app.playing_row(),
            r.app.queue_pos,
            "and the panel finds it where the listing puts it"
        );
    }

    #[test]
    fn re_shuffling_deals_again_only_what_has_not_played_yet() {
        // Goal: what is behind the listener stays behind them. Re-dealing the
        // whole order would move tracks that have already played to the front
        // of it, where the panel says they are still to come. Asked for through
        // the session bus, because the desktop's `Shuffle` has to reach the very
        // same method the key does.
        let mut r = playing_shuffled(12);
        play_on(&mut r);
        play_on(&mut r);
        let mut before = play_order(&r.app);
        let behind = before[..=r.app.playing_row()].to_vec();

        // Five deals rather than one: a single deal that wrongly moved the
        // played rows could put them back where they were by luck.
        for deal in 0..5 {
            r.app.apply(BusCommand::Shuffle(true));
            let after = play_order(&r.app);
            assert_eq!(
                &after[..behind.len()],
                &behind[..],
                "what has played, and what is playing, do not move (deal {deal}): {after:?}"
            );
            assert_eq!(
                r.app.queue_selected,
                r.app.playing_row(),
                "and the cursor follows the track to its new row"
            );
            let mut covered = after.clone();
            covered.sort_unstable();
            assert_eq!(covered, listing(12), "still every track exactly once");
            assert_ne!(after, before, "and the rest was dealt again");
            before = after;
        }
    }

    #[test]
    fn a_row_chosen_with_the_shuffle_on_is_where_the_order_starts() {
        // Goal: Enter names a track, and the shuffle does not get to overrule
        // that. It plays, at the head of a freshly dealt order - dealt into the
        // middle of one instead, the rows above it would read as history that
        // never played. Five choices, because one landing at the head by luck
        // would prove nothing.
        let mut r = rig();
        r.app.favorites = (1..=10).map(|i| track(i, "T", "A")).collect();
        r.app.shuffle = true;
        for chosen in [0usize, 3, 9, 5, 7] {
            r.app.selected = chosen;
            r.app.on_key(code(KeyCode::Enter));
            let order = play_order(&r.app);
            assert_eq!(
                r.app.now_playing.as_ref().map(|t| t.id),
                Some(chosen as u64 + 1),
                "the row that was pointed at plays"
            );
            assert_eq!(r.app.playing_row(), 0, "at the head of the order");
            assert_eq!(order[0], chosen as u64 + 1);
            let mut covered = order.clone();
            covered.sort_unstable();
            assert_eq!(covered, listing(10), "with the rest behind it, once each");
            assert_ne!(order, listing(10), "and the rest of it dealt");
        }
    }

    #[test]
    fn a_spent_order_ends_the_queue_exactly_as_the_listing_does() {
        // Goal: the shuffle's answer is what the order is, and the repeat's is
        // whether it ends - so a spent order asks the repeat the same question
        // the bottom of the listing asks, and repeat-all is answered from the
        // head of the order rather than from the head of the queue.
        let mut r = playing_shuffled_from(6, 3);
        for _ in 0..6 {
            if !play_on(&mut r) {
                break;
            }
        }
        assert_eq!(r.app.playing_row(), 5, "the order ran to its end");
        assert_eq!(r.app.next_pos(Repeat::Off), None, "and stops where it ends");
        assert_eq!(
            r.app.next_pos(Repeat::All),
            Some(3),
            "repeat-all starts that order again: its head, not the listing's"
        );
    }

    #[test]
    fn an_order_that_does_not_fit_the_queue_is_not_half_used() {
        // Goal: the fallback is all or nothing. Taking the rows from an order
        // built for a different queue but the current position from the listing
        // - or the other way about - puts the cursor and the music on different
        // tracks, which is the one thing the panel must never do.
        let mut r = rig();
        r.app.queue = (1..=5).map(|i| track(i, "T", "A")).collect();
        r.app.order = vec![2, 0, 1]; // built for a queue of three
        r.app.queue_pos = 1;

        assert_eq!(r.app.queue_at(0), Some(0), "the listing, row for row");
        assert_eq!(r.app.playing_row(), 1, "and where the listing puts it");
        assert_eq!(
            r.app.queue_at(r.app.playing_row()),
            Some(r.app.queue_pos),
            "the row and the track always agree"
        );
    }

    #[test]
    fn an_empty_queue_and_a_queue_of_one_have_a_defined_order() {
        // Goal: the two degenerate queues. Neither has anywhere to go, and both
        // used to be a modulo away from a panic or from standing still.
        let mut r = rig();
        r.app.shuffle = true;
        assert_eq!(r.app.queue_at(0), None, "no rows to draw");
        assert_eq!(r.app.playing_row(), 0);
        assert_eq!(r.app.next_pos(Repeat::Off), None);
        assert_eq!(r.app.next_pos(Repeat::All), None, "nothing to start again");

        let mut r = playing_shuffled(1);
        assert_eq!(play_order(&r.app), vec![1], "an order of one");
        assert_eq!(r.app.next_pos(Repeat::Off), None, "which is spent at once");
        assert_eq!(r.app.next_pos(Repeat::All), Some(0), "and starts again");
        r.app.on_key(key('p'));
        assert_eq!(r.app.queue_pos, 0, "with nowhere behind it either");
    }

    #[test]
    fn previous_walks_back_up_the_order_the_panel_shows() {
        // Goal: going back is going up the panel. The shuffle used to answer
        // `H` with another random pick, so the track above the current row was
        // not the track that had just played and the history read as a lie.
        let mut r = playing_shuffled(10);
        let mut played = vec![r.app.queue[r.app.queue_pos].id];
        for _ in 0..3 {
            play_on(&mut r);
            played.push(r.app.queue[r.app.queue_pos].id);
        }
        while played.len() > 1 {
            played.pop();
            r.app.on_key(key('H'));
            assert_eq!(
                r.app.now_playing.as_ref().map(|t| t.id),
                played.last().copied(),
                "back through what actually played: {played:?}"
            );
        }
    }

    #[test]
    fn a_queue_that_grows_keeps_the_order_already_laid_out() {
        // Goal: the radio extends the queue while it plays, and the rows the
        // listener is looking at must not rearrange under them when the page
        // lands. The new entries go on the end of the order, dealt among
        // themselves.
        let mut r = rig();
        r.app.favorites = (1..=8).map(|i| track_with_radio(i, "0016d")).collect();
        r.app.on_key(code(KeyCode::Enter));
        r.app.on_key(key('s'));
        r.app.current_target = None;
        r.app.continue_radio = true;
        for _ in 0..8 {
            if !play_on(&mut r) {
                break;
            }
        }
        let before = play_order(&r.app);
        assert_ne!(before, listing(8), "shuffled before the queue grew");

        r.app.status.playing = true;
        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
        r.to_app
            .send(radio_page("0016d", 20..28))
            .expect("the app holds the other end");
        r.app.drain_worker();

        let after = play_order(&r.app);
        assert_eq!(&after[..before.len()], &before[..], "nothing already laid");
        assert_eq!(after.len(), r.app.queue.len(), "and every new row is in it");
        let mut covered = after.clone();
        covered.sort_unstable();
        covered.dedup();
        assert_eq!(
            covered.len(),
            after.len(),
            "each queue entry once: {after:?}"
        );
        assert_ne!(
            after[before.len()..],
            (20..28).collect::<Vec<_>>()[..],
            "and the new rows were dealt among themselves"
        );
    }

    #[test]
    fn a_queue_that_grew_without_an_order_is_given_one() {
        // Goal: the order is honoured only while it fits the queue, so one that
        // has fallen behind is laid again rather than extended - extending it
        // would build a permutation of a queue that no longer exists, and the
        // rows would name tracks other than the ones under them.
        let mut r = rig();
        r.app.queue = (1..=8).map(|i| track_with_radio(i, "0016d")).collect();
        r.app.queue_pos = 7;
        r.app.now_playing = r.app.queue.last().cloned();
        r.app.expected_id = 8;
        r.app.status.playing = true;
        r.app.shuffle = true;
        r.app.continue_radio = true;
        let _ = requests(&r);

        r.app.refresh_for_test();
        assert_eq!(radios_asked(&r), vec!["0016d".to_string()]);
        r.to_app
            .send(radio_page("0016d", 20..28))
            .expect("the app holds the other end");
        r.app.drain_worker();

        let after = play_order(&r.app);
        assert_eq!(after.len(), r.app.queue.len(), "one row per entry");
        assert_eq!(
            &after[..8],
            &listing(8)[..],
            "what was there keeps its place"
        );
        assert_ne!(
            after[8..],
            (20..28).collect::<Vec<_>>()[..],
            "and what arrived was dealt"
        );
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
        assert_eq!(
            r.app.click_at(50, 9),
            Click::Nothing,
            "a zero-length track has no position to seek to"
        );

        r.app.status.duration = 200.0;
        assert_eq!(r.app.click_at(50, 9), Click::Seek(100.0));
        assert_eq!(r.app.click_at(0, 9), Click::Seek(0.0));
        assert_eq!(
            r.app.click_at(99, 9),
            Click::Seek(198.0),
            "the last cell of the bar is the end of the track, not past it"
        );
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
    fn a_click_on_the_reference_runs_the_key_it_landed_on_and_closes() {
        // Goal: the reference is priel's menu, so a click on one of its keys must
        // run that key rather than merely dismiss - that is the mouse's route to
        // everything the bottom row has no width for. A click on nothing still
        // just dismisses. That the keys under the pointer belong to the overlay
        // and never to the header behind it is the renderer's half of this, and
        // is asserted in `ui`.
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
        assert_eq!(r.app.mode, Mode::Normal, "the overlay closes either way");
        assert!(r.app.shuffle, "and the key it landed on ran");

        r.app.on_key(key('?'));
        r.app.on_mouse(click(40, 20));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(r.app.shuffle, "a click on nothing changes nothing else");
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
    fn a_wobbling_buffer_count_is_not_in_the_redraw_signature() {
        // Goal: mpv's demuxer-cache-duration wobbles across whole-second
        // boundaries several times a second. In the signature, each wobble
        // repaints the whole window - a GPU composite for a number nobody needs
        // to the fraction of a second. The buffered readout rides the
        // once-a-second position update instead; the count itself repaints
        // nothing.
        let mut r = rig();
        r.app.status.loaded = true;
        r.app.status.playing = true;
        let before = r.app.render_sig();
        r.app.status.cache_secs = 119.0;
        r.app.status.bitrate = 950_000;
        assert_eq!(
            before,
            r.app.render_sig(),
            "a changed buffer count or bitrate must not change the signature"
        );
        r.app.status.cache_secs = 121.0;
        assert_eq!(before, r.app.render_sig());
        // But moving on to a new whole second still does.
        r.app.status.position = f64::from(u32::try_from(before.position).unwrap_or(0)) + 2.0;
        assert_ne!(before, r.app.render_sig(), "the position still repaints");
    }

    #[test]
    fn mpvs_growing_duration_estimate_is_not_in_the_redraw_signature() {
        // Goal: a segment stream advertises no length, so mpv estimates the
        // duration and the estimate grows about once a second. The progress bar
        // shows the listing's fixed figure (`App::duration`), not mpv's, so the
        // signature must watch that too - or the whole window repaints every
        // time mpv's guess ticks up, which is the constant GPU load this fixes.
        let mut r = rig();
        r.app.status.loaded = true;
        r.app.status.playing = true;
        r.app.now_playing = Some(track(1, "A", "X"));
        r.app.now_playing.as_mut().expect("just set").duration_secs = 200;
        let before = r.app.render_sig();
        r.app.status.duration = 140.0; // mpv's estimate
        assert_eq!(
            before,
            r.app.render_sig(),
            "mpv's growing estimate must not move the signature"
        );
        r.app.status.duration = 160.0; // it grew again
        assert_eq!(before, r.app.render_sig());
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

        // A page is measured from the list rect the renderer published.
        r.app.list_inner = Rect {
            x: 0,
            y: 1,
            width: 40,
            height: 2,
        };

        fire(&mut r.app, Hit::MoveDown);
        assert_eq!(r.app.selected, 1);
        fire(&mut r.app, Hit::Bottom);
        assert_eq!(r.app.selected, 4);
        fire(&mut r.app, Hit::MoveUp);
        assert_eq!(r.app.selected, 3);
        fire(&mut r.app, Hit::Top);
        assert_eq!(r.app.selected, 0);

        fire(&mut r.app, Hit::PageDown);
        assert_eq!(r.app.selected, 2, "a full page is the list's height");
        fire(&mut r.app, Hit::HalfPageDown);
        assert_eq!(r.app.selected, 3, "and half of it is rounded up to one");
        fire(&mut r.app, Hit::HalfPageUp);
        assert_eq!(r.app.selected, 2);
        fire(&mut r.app, Hit::PageUp);
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
        fire(&mut r.app, Hit::Devices);
        assert_eq!(r.app.mode, Mode::Devices);
        fire(&mut r.app, Hit::Themes);
        assert_eq!(r.app.mode, Mode::Themes);
        fire(&mut r.app, Hit::Graph);
        assert_eq!(r.app.mode, Mode::Graph);
        fire(&mut r.app, Hit::Log);
        assert_eq!(r.app.mode, Mode::Log);
        fire(&mut r.app, Hit::EditSearch);
        assert_eq!(r.app.view, View::Search);
        assert_eq!(r.app.mode, Mode::Search);
        r.app.view = View::Favorites;
        r.app.selected = 2;
        fire(&mut r.app, Hit::Enter);
        assert_eq!(
            r.app.now_playing.as_ref().map(|t| t.id),
            Some(3),
            "Enter plays the row the selection is on"
        );

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

        fire(&mut r.app, Hit::SignIn);
        assert_eq!(
            r.app.mode,
            Mode::Normal,
            "no client identity, so nowhere to sign in to - but it is wired"
        );

        fire(&mut r.app, Hit::Quit);
        assert!(r.app.should_quit);
    }

    #[test]
    fn every_control_on_the_two_modal_screens_dispatches_to_a_real_action() {
        // Goal: the consent and sign-in screens took no mouse input at all, so
        // their controls are the newest and the least exercised. A rigged app has
        // no client identity and no flow in progress, so what this asserts is
        // that each is wired to a method rather than to nothing - and that the
        // two that leave a screen do leave it.
        let mut r = rig();
        let press = |app: &mut App, mode: Mode, h: Hit| {
            app.set_mode_for_test(mode);
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

        press(&mut r.app, Mode::Credentials, Hit::FetchCredentials);
        assert!(r.app.credential_status().is_none(), "nowhere to save it to");
        press(&mut r.app, Mode::Credentials, Hit::DeclineCredentials);
        assert_eq!(r.app.mode, Mode::Normal, "not now continues without it");

        for h in [Hit::SubmitLogin, Hit::ReopenBrowser, Hit::ClearPaste] {
            press(&mut r.app, Mode::Login, h);
            assert_eq!(r.app.mode, Mode::Login, "these all stay on the screen");
        }
        press(&mut r.app, Mode::Login, Hit::CancelLogin);
        assert_eq!(r.app.mode, Mode::Normal, "cancelling leaves it");
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
        // usable app - the failure arrives later as a notice. It is also the
        // one place `--theme` reaches the renderer, so assert the flag lands.
        // The bus is `None` because this test builds a real app, and nothing a
        // test constructs may reach the machine - which is exactly why `main`
        // opens the bus rather than this constructor.
        let app = App::new(
            PlayerConfig {
                audio_device: Some("null".into()),
                ..PlayerConfig::default()
            },
            "/nonexistent/priel.json".into(),
            crate::logging::Recent::default(),
            ThemeName::OneLight,
            None,
        )
        .expect("an app should be constructible without a valid token");
        assert_eq!(app.view, View::Favorites);
        assert!(app.loading, "it starts out loading");
        assert_eq!(
            app.theme(),
            Theme::of(ThemeName::OneLight),
            "--theme reaches the renderer"
        );
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
    fn only_a_click_on_one_of_the_consent_screens_choices_answers_it() {
        // Goal: every other overlay is dismissed by a click. This one is not:
        // a stray click must not be read as consent to download a credential,
        // nor as declining. Only a click that lands on one of its own controls
        // answers, and it runs the same shared method the key does.
        let mut r = rig();
        r.app.set_mode_for_test(Mode::Credentials);
        r.app.on_mouse(click(1, 1));
        assert_eq!(r.app.mode, Mode::Credentials, "a click is not consent");

        r.app.hits = vec![(
            Rect {
                x: 0,
                y: 1,
                width: 3,
                height: 1,
            },
            Hit::DeclineCredentials,
        )];
        r.app.on_mouse(click(1, 1));
        assert_eq!(r.app.mode, Mode::Normal, "but pointing at `not now` does");
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

    // ---- editing playlists ----

    /// An app sitting on the playlists list with two of them loaded.
    fn with_playlists() -> Rig {
        let mut r = rig();
        r.app.playlists = vec![playlist("a1", "Morning"), playlist("b2", "Evening")];
        r.app.playlists_paging.total = 2;
        r.app.view = View::Playlists;
        let _ = requests(&r);
        r
    }

    fn type_in(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(c));
        }
    }

    #[test]
    fn a_new_playlist_is_named_before_anything_is_asked_for() {
        // Goal: `N` must not create anything on its own. A playlist is made once
        // and cannot be un-made, so the request goes out only after a name has
        // been typed and accepted.
        let mut r = with_playlists();
        r.app.on_key(key('N'));
        assert_eq!(r.app.mode, Mode::Prompt);
        type_in(&mut r.app, "  Late night  ");
        assert!(
            requests(&r).is_empty(),
            "nothing may go out while it is being typed"
        );

        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.mode, Mode::Normal);
        let sent = requests(&r);
        assert_eq!(sent.len(), 1);
        assert!(
            matches!(&sent[0], ToWorker::CreatePlaylist { title } if title == "Late night"),
            "the name is trimmed before it is sent"
        );
    }

    #[test]
    fn a_playlist_with_no_name_is_not_created() {
        // Goal: the service accepts an empty title and the listener ends up with
        // a row they cannot tell from any other. The prompt stays up so the name
        // can be typed rather than the attempt being lost.
        let mut r = with_playlists();
        r.app.on_key(key('N'));
        type_in(&mut r.app, "   ");
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.mode, Mode::Prompt, "still asking");
        assert!(requests(&r).is_empty());
    }

    #[test]
    fn a_created_playlist_appears_without_reloading_the_list() {
        // Goal: the uuid is the service's to choose, so this reply is the only
        // moment the new row can exist. Reloading instead would throw away a
        // filter the listener had typed and their place in the list.
        let mut r = with_playlists();
        r.to_app
            .send(FromWorker::PlaylistCreated(playlist("c3", "Late night")))
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(
            r.app.playlists[0].uuid, "c3",
            "newest first, as the listing is"
        );
        assert_eq!(r.app.playlists.len(), 3);
        assert_eq!(
            r.app.playlists_paging.total, 3,
            "the listing is one longer, or paging asks for the last row twice"
        );
    }

    #[test]
    fn renaming_shows_the_new_title_at_once_and_puts_the_old_one_back_if_refused() {
        // Goal: a rename is one field and cheap to undo, so it is optimistic
        // like a favorite. That is only safe if the refusal really does restore
        // the exact title that was there - hence the round trip both ways.
        let mut r = with_playlists();
        r.app.selected = 1;
        r.app.on_key(key('R'));
        assert_eq!(r.app.prompt_text, "Evening", "primed with what is there");
        type_in(&mut r.app, "!");
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(
            r.app.playlists[1].title, "Evening!",
            "shown before it is agreed"
        );

        let sent = requests(&r);
        assert!(matches!(
            &sent[0],
            ToWorker::RenamePlaylist { uuid, title, was }
                if uuid == "b2" && title == "Evening!" && was == "Evening"
        ));

        r.to_app
            .send(FromWorker::Failed {
                task: Task::RenamePlaylist {
                    uuid: "b2".into(),
                    was: "Evening".into(),
                },
                fault: Fault::Refused,
                detail: "no".into(),
            })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(r.app.playlists[1].title, "Evening", "put back exactly");
    }

    #[test]
    fn a_rename_acts_on_the_highlighted_playlist_not_the_row_beneath_it() {
        // Goal: selection indexes the *filtered* rows. Reading the backing vec
        // with it renames whichever playlist happens to sit at that position in
        // the unfiltered list, which is a different playlist entirely.
        let mut r = with_playlists();
        r.app.filter = "even".into();
        r.app.selected = 0;
        r.app.on_key(key('R'));
        assert_eq!(
            r.app.prompt_text, "Evening",
            "the one row the filter left, not the first of the two"
        );
    }

    #[test]
    fn deleting_a_playlist_asks_first_and_sends_nothing_until_it_is_answered() {
        // Goal: the one action in priel that cannot be undone. `X` must open a
        // question naming the playlist, and the question must be answerable
        // only on purpose.
        let mut r = with_playlists();
        r.app.selected = 0;
        r.app.on_key(key('X'));
        assert_eq!(r.app.mode, Mode::Confirm);
        let asked = r.app.confirm_question().expect("a question");
        assert!(
            asked[0].contains("Morning"),
            "it names the playlist: {asked:?}"
        );
        assert!(
            asked.iter().any(|l| l.contains("no way to bring it back")),
            "and says what it means: {asked:?}"
        );
        assert!(requests(&r).is_empty(), "asking is not doing");

        r.app.on_key(key('y'));
        let sent = requests(&r);
        assert!(matches!(
            &sent[0],
            ToWorker::DeletePlaylist { uuid, .. } if uuid == "a1"
        ));
    }

    #[test]
    fn enter_does_not_answer_a_confirmation() {
        // Goal: every other modal here accepts on Enter, and that is exactly the
        // danger. Enter opens a playlist; pressing it twice out of rhythm must
        // not be the difference between reading a question and deleting.
        let mut r = with_playlists();
        r.app.on_key(key('X'));
        for k in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('d')] {
            r.app.on_key(code(k));
            assert_eq!(r.app.mode, Mode::Confirm, "{k:?} must not answer it");
        }
        assert!(requests(&r).is_empty());
    }

    #[test]
    fn a_click_that_misses_the_confirmations_controls_does_nothing_at_all() {
        // Goal: a stray click is not consent, and it is not anything else
        // either. The list is still underneath - the renderer clears the header
        // and hint hit boxes but `list_inner` is a rect, not a hit box - so
        // without the modal guard a click on a row would move the selection and
        // a double-click would start playing, all while a delete is being asked
        // about. Scrolling is swallowed for the same reason.
        let mut r = with_playlists();
        r.app.list_inner = Rect {
            x: 0,
            y: 4,
            width: 40,
            height: 10,
        };
        r.app.on_key(key('X'));
        r.app.hits = vec![(
            Rect {
                x: 10,
                y: 20,
                width: 3,
                height: 1,
            },
            Hit::ConfirmYes,
        )];

        r.app.on_mouse(click(5, 5));
        r.app.on_mouse(click(5, 5));
        r.app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(r.app.mode, Mode::Confirm, "still asking");
        assert_eq!(r.app.selected, 0, "the list behind is out of reach");
        assert!(r.app.now_playing.is_none(), "and cannot be played through");
        assert!(requests(&r).is_empty(), "nothing was deleted");

        r.app.on_mouse(click(11, 20));
        assert_eq!(r.app.mode, Mode::Normal);
        assert_eq!(requests(&r).len(), 1, "the control itself does answer");
    }

    #[test]
    fn backing_out_of_a_confirmation_forgets_what_it_was_asking_about() {
        // Goal: a question left pending would be answered by the *next* one, so
        // a `y` meant for a harmless prompt could delete whatever `X` had been
        // pointed at minutes earlier.
        let mut r = with_playlists();
        r.app.on_key(key('X'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(r.app.confirm_question().is_none(), "nothing is pending");

        r.app.set_mode_for_test(Mode::Confirm);
        r.app.on_key(key('y'));
        assert!(requests(&r).is_empty(), "a stale yes deletes nothing");
    }

    #[test]
    fn the_row_stays_until_the_service_says_the_playlist_is_gone() {
        // Goal: the deliberate difference from a favorite. A favorite is one bit
        // and the same key puts it back; a deleted playlist is not, so the row
        // must not vanish on hope and quietly reappear when the answer is no.
        let mut r = with_playlists();
        r.app.on_key(key('X'));
        r.app.on_key(key('y'));
        assert_eq!(
            r.app.playlists.len(),
            2,
            "still there while it is in flight"
        );

        r.to_app
            .send(FromWorker::PlaylistDeleted { uuid: "a1".into() })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(r.app.playlists.len(), 1);
        assert_eq!(r.app.playlists[0].uuid, "b2");
        assert_eq!(r.app.playlists_paging.total, 1);
    }

    #[test]
    fn a_refused_delete_leaves_the_playlist_where_it_was() {
        // Goal: the other half of waiting. Nothing to put back, and the reason
        // has to reach the notice line rather than being swallowed.
        let mut r = with_playlists();
        r.app.on_key(key('X'));
        r.app.on_key(key('y'));
        r.to_app
            .send(FromWorker::Failed {
                task: Task::DeletePlaylist { uuid: "a1".into() },
                fault: Fault::Refused,
                detail: "deleting the playlist: not yours".into(),
            })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(r.app.playlists.len(), 2, "nothing was taken away");
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("not yours"), "{notice}");
    }

    #[test]
    fn deleting_the_playlist_that_is_open_takes_the_listener_back_out_of_it() {
        // Goal: the view would otherwise go on showing the tracks of a playlist
        // that no longer exists, and reloading it would 404.
        let mut r = with_playlists();
        r.app.open_playlist = Some(("a1".into(), "Morning".into()));
        r.app.playlist_tracks = vec![track(1, "T", "A")];
        r.app.view = View::PlaylistTracks;
        r.to_app
            .send(FromWorker::PlaylistDeleted { uuid: "a1".into() })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(r.app.view, View::Playlists);
        assert!(r.app.open_playlist.is_none());
        assert!(r.app.playlist_tracks.is_empty());
    }

    #[test]
    fn removing_a_track_asks_first_and_waits_before_the_row_goes() {
        // Goal: `X` means "take away what is highlighted", and inside a playlist
        // that is the track. It is confirmed for the same reason the delete is,
        // and it waits for the same reason: the row leaving is the only report
        // the listener gets that it worked.
        let mut r = with_playlists();
        r.app.open_playlist = Some(("a1".into(), "Morning".into()));
        r.app.playlist_tracks = vec![track(7, "So What", "Miles"), track(8, "Blue", "Miles")];
        r.app.playlist_tracks_paging.total = 2;
        r.app.view = View::PlaylistTracks;
        r.app.selected = 0;

        r.app.on_key(key('X'));
        let asked = r.app.confirm_question().expect("a question");
        assert!(
            asked[0].contains("So What"),
            "it names the track: {asked:?}"
        );
        assert!(asked[0].contains("Morning"), "and the playlist: {asked:?}");

        r.app.on_key(key('y'));
        assert_eq!(
            r.app.playlist_tracks.len(),
            2,
            "still there while in flight"
        );
        let sent = requests(&r);
        assert!(matches!(
            &sent[0],
            ToWorker::RemoveFromPlaylist { uuid, track_id } if uuid == "a1" && *track_id == 7
        ));

        r.to_app
            .send(FromWorker::PlaylistTrackRemoved {
                uuid: "a1".into(),
                track_id: 7,
            })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(r.app.playlist_tracks.len(), 1);
        assert_eq!(r.app.playlist_tracks[0].id, 8);
        assert_eq!(r.app.playlist_tracks_paging.total, 1);
    }

    #[test]
    fn a_removal_that_lands_after_the_listener_has_moved_on_changes_nothing() {
        // Goal: the reply names the playlist it belongs to. Without that guard a
        // removal answered late would take a row out of whichever playlist is
        // open now, which is a track the listener never asked to lose.
        let mut r = with_playlists();
        r.app.open_playlist = Some(("b2".into(), "Evening".into()));
        r.app.playlist_tracks = vec![track(7, "So What", "Miles")];
        r.to_app
            .send(FromWorker::PlaylistTrackRemoved {
                uuid: "a1".into(),
                track_id: 7,
            })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        assert_eq!(
            r.app.playlist_tracks.len(),
            1,
            "a different playlist is open"
        );
    }

    #[test]
    fn there_is_nothing_to_remove_outside_a_playlist() {
        // Goal: `X` in the favorites has no honest meaning - taking a track off
        // the favorites is what `f` does - so it says so rather than guessing.
        let mut r = rig();
        r.app.favorites = vec![track(1, "T", "A")];
        r.app.view = View::Favorites;
        let _ = requests(&r);
        r.app.on_key(key('X'));
        assert_eq!(r.app.mode, Mode::Normal, "no question was opened");
        assert!(requests(&r).is_empty());
        assert!(r.app.notice.clone().unwrap_or_default().contains("removed"));
    }

    #[test]
    fn adding_a_track_asks_which_playlist_and_sends_the_one_chosen() {
        // Goal: the playlist being added to is usually not the one on screen, so
        // there is no target the interface could guess. The picker is the whole
        // point, and the track it carries is the highlighted one.
        let mut r = with_playlists();
        r.app.view = View::Favorites;
        r.app.favorites = vec![track(1, "One", "A"), track(2, "Two", "A")];
        r.app.selected = 1;

        r.app.on_key(key('a'));
        assert_eq!(r.app.mode, Mode::AddTo);
        assert!(requests(&r).is_empty(), "opening it asks for nothing");

        r.app.on_key(key('j'));
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.mode, Mode::Normal);
        let sent = requests(&r);
        assert!(
            matches!(
                &sent[0],
                ToWorker::AddToPlaylist { uuid, title, track_id }
                    if uuid == "b2" && title == "Evening" && *track_id == 2
            ),
            "the second playlist and the highlighted track"
        );
    }

    #[test]
    fn the_picker_adds_to_the_playlist_that_was_clicked() {
        // Goal: a key press and a click run the same method, so the row under
        // the pointer is the row that is chosen.
        let mut r = with_playlists();
        r.app.view = View::Favorites;
        r.app.favorites = vec![track(1, "One", "A")];
        r.app.on_key(key('a'));
        r.app.add_rows = vec![(
            Rect {
                x: 0,
                y: 3,
                width: 20,
                height: 1,
            },
            1,
        )];
        r.app.on_mouse(click(4, 3));
        let sent = requests(&r);
        assert!(matches!(
            &sent[0],
            ToWorker::AddToPlaylist { uuid, .. } if uuid == "b2"
        ));
    }

    #[test]
    fn backing_out_of_the_picker_adds_nothing() {
        // Goal: Esc cancels, as it does in the other two pickers, and the track
        // it was holding is let go so a later Enter cannot send it.
        let mut r = with_playlists();
        r.app.view = View::Favorites;
        r.app.favorites = vec![track(1, "One", "A")];
        r.app.on_key(key('a'));
        r.app.on_key(code(KeyCode::Esc));
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(requests(&r).is_empty());

        r.app.set_mode_for_test(Mode::AddTo);
        r.app.on_key(code(KeyCode::Enter));
        assert!(requests(&r).is_empty(), "there is no track pending");
    }

    #[test]
    fn the_picker_fetches_the_playlists_if_it_has_none_to_show() {
        // Goal: somebody who has never opened the playlists tab has none loaded.
        // An empty picker would say they have no playlists, which is not true.
        let mut r = rig();
        r.app.favorites = vec![track(1, "One", "A")];
        r.app.view = View::Favorites;
        let _ = requests(&r);
        r.app.on_key(key('a'));
        assert!(
            requests(&r)
                .iter()
                .any(|q| matches!(q, ToWorker::LoadPlaylists { offset: 0, .. })),
            "it asks rather than showing an empty list as the truth"
        );
    }

    #[test]
    fn adding_a_track_is_reported_because_nothing_on_screen_would_show_it() {
        // Goal: the one success that is announced. A favorite fills a heart the
        // listener is looking at; this changes a playlist they are not, so the
        // notice is the only confirmation there is.
        let mut r = with_playlists();
        r.to_app
            .send(FromWorker::PlaylistTrackAdded {
                title: "Evening".into(),
            })
            .expect("the rigged channel is open");
        r.app.drain_worker();
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(notice.contains("Evening"), "{notice}");
    }

    #[test]
    fn every_playlist_control_is_wired_to_the_same_method_its_key_runs() {
        // Goal: `Hit` is the contract between the renderer and the input layer,
        // and a variant wired to nothing compiles into a dead button. Each of
        // these is reached from the reference overlay, which is the only place
        // the mouse can get at them.
        let mut r = with_playlists();
        let fire = |app: &mut App, mode: Mode, h: Hit| {
            app.set_mode_for_test(mode);
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

        fire(&mut r.app, Mode::Normal, Hit::NewPlaylist);
        assert_eq!(r.app.mode, Mode::Prompt);
        fire(&mut r.app, Mode::Prompt, Hit::CancelPrompt);
        assert_eq!(r.app.mode, Mode::Normal);

        fire(&mut r.app, Mode::Normal, Hit::RenamePlaylist);
        assert_eq!(r.app.mode, Mode::Prompt);
        assert_eq!(r.app.prompt_text, "Morning");
        fire(&mut r.app, Mode::Prompt, Hit::SubmitPrompt);
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(requests(&r).iter().any(|q| matches!(
            q,
            ToWorker::RenamePlaylist { uuid, .. } if uuid == "a1"
        )));

        fire(&mut r.app, Mode::Normal, Hit::RemoveSelected);
        assert_eq!(r.app.mode, Mode::Confirm);
        fire(&mut r.app, Mode::Confirm, Hit::ConfirmNo);
        assert_eq!(r.app.mode, Mode::Normal);
        assert!(requests(&r).is_empty(), "no is no");

        fire(&mut r.app, Mode::Normal, Hit::RemoveSelected);
        fire(&mut r.app, Mode::Confirm, Hit::ConfirmYes);
        assert!(requests(&r).iter().any(|q| matches!(
            q,
            ToWorker::DeletePlaylist { uuid, .. } if uuid == "a1"
        )));

        r.app.view = View::Favorites;
        r.app.favorites = vec![track(1, "One", "A")];
        r.app.selected = 0;
        fire(&mut r.app, Mode::Normal, Hit::AddToPlaylist);
        assert_eq!(r.app.mode, Mode::AddTo);
    }

    #[test]
    fn a_playlist_name_can_be_pasted_as_well_as_typed() {
        // Goal: the name box is a text field, and the only other one that takes
        // a paste is the sign-in box. A field that took keystrokes but not a
        // paste would be an inconsistency nobody could guess at, and the bound
        // has to hold on this path too.
        let mut r = with_playlists();
        r.app.on_key(key('N'));
        r.app.on_paste("  Late night  ");
        assert_eq!(r.app.prompt_text, "Late night");

        r.app.on_paste(&"x".repeat(PLAYLIST_NAME_MAX * 2));
        assert_eq!(r.app.prompt_text.chars().count(), PLAYLIST_NAME_MAX);
    }

    #[test]
    fn a_typed_name_stops_growing_at_the_bound() {
        // Goal: the box grows from keystrokes and nothing else would stop it.
        let mut r = with_playlists();
        r.app.on_key(key('N'));
        for _ in 0..(PLAYLIST_NAME_MAX + 50) {
            r.app.on_key(key('x'));
        }
        assert_eq!(r.app.prompt_text.chars().count(), PLAYLIST_NAME_MAX);
    }

    // ---- the queue in its column: a second focusable region ----

    /// The keyboard's way between the two lists. Vim's own window prefix, and
    /// with exactly two regions the prefix *is* the move.
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Put a queue region on screen without rendering one.
    ///
    /// `focus` answers `Queue` only where the renderer published a region to
    /// focus, which is what makes "there is no column below 120 columns" a fact
    /// about geometry rather than a second breakpoint in the key handler. A
    /// test that is not rendering says so by hand.
    fn with_column(app: &mut App) {
        app.queue_inner = Rect {
            x: 84,
            y: 10,
            width: 32,
            height: 6,
        };
    }

    fn queued(r: &mut Rig, n: u64) {
        let tracks: Vec<Track> = (1..=n)
            .map(|i| track(i, &format!("T{i}"), "Artist"))
            .collect();
        r.app.set_queue(tracks);
    }

    #[test]
    fn the_focus_key_moves_between_the_two_lists_and_back() {
        // Goal: one key changes which list the keyboard drives, and it toggles
        // rather than latching - with two regions the window prefix is the
        // whole move. Method: press it twice and read the focus after each.
        let mut r = rig();
        with_column(&mut r.app);
        assert_eq!(r.app.focus(), Focus::List, "the list starts with the keys");
        r.app.on_key(ctrl('w'));
        assert_eq!(r.app.focus(), Focus::Queue);
        r.app.on_key(ctrl('w'));
        assert_eq!(r.app.focus(), Focus::List);
    }

    #[test]
    fn without_the_column_there_is_nothing_to_focus_and_the_key_says_so() {
        // Goal: below 120 columns the column is not on screen, so there is no
        // second region - and the key must leave the list driving rather than
        // moving the keyboard somewhere invisible. Method: press it with no
        // region published and check both the focus and what the user is told.
        let mut r = rig();
        queued(&mut r, 4);
        r.app.on_key(ctrl('w'));
        assert_eq!(r.app.focus(), Focus::List);
        let said = r.app.notice.clone().unwrap_or_default();
        assert!(
            said.contains("120"),
            "the key says nothing about the width that brings the queue back: {said:?}"
        );
    }

    #[test]
    fn a_terminal_narrowed_under_the_column_hands_the_keyboard_back() {
        // Goal: the region can go away under the listener's fingers - a resize
        // takes the column with it - and j must not then move something nobody
        // can see. Method: focus the queue, take the region away as a narrow
        // frame does, and check the browse list moves again.
        let mut r = rig();
        with_column(&mut r.app);
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "X")];
        queued(&mut r, 4);
        r.app.on_key(ctrl('w'));
        r.app.queue_inner = Rect::default();
        assert_eq!(r.app.focus(), Focus::List);
        r.app.on_key(key('j'));
        assert_eq!(r.app.selected, 1, "j went to the region that is on screen");
    }

    #[test]
    fn j_and_k_move_whichever_list_holds_the_keyboard() {
        // Goal: the same keys drive both regions, so there is one movement
        // idiom rather than two. Method: move with the list focused, focus the
        // queue, move again, and check each cursor moved only on its own turn.
        let mut r = rig();
        with_column(&mut r.app);
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "X"), track(3, "C", "X")];
        queued(&mut r, 5);
        r.app.on_key(key('j'));
        assert_eq!(r.app.selected, 1);
        assert_eq!(r.app.queue_selected, 0);
        r.app.on_key(ctrl('w'));
        r.app.on_key(key('j'));
        r.app.on_key(key('j'));
        assert_eq!(r.app.queue_selected, 2);
        assert_eq!(r.app.selected, 1, "the browse list stayed where it was");
        r.app.on_key(key('k'));
        assert_eq!(r.app.queue_selected, 1);
    }

    #[test]
    fn the_ends_belong_to_the_focused_region_too() {
        // Goal: g and G are the same two keys in both regions - a second
        // scrolling idiom would be its own bug. Method: send them with the
        // queue focused and check the queue's cursor, not the list's, moved.
        let mut r = rig();
        with_column(&mut r.app);
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "X")];
        queued(&mut r, 5);
        r.app.on_key(ctrl('w'));
        r.app.on_key(key('G'));
        assert_eq!(r.app.queue_selected, 4, "the last entry in the queue");
        assert_eq!(r.app.selected, 0);
        r.app.on_key(key('g'));
        assert_eq!(r.app.queue_selected, 0);
    }

    #[test]
    fn enter_on_a_queue_entry_plays_it() {
        // Goal: the queue is navigable in both directions, and the way back
        // through what has played is Enter on the row itself. Method: focus the
        // queue, walk to the third entry, press Enter, and check the queue
        // moved there and that entry was asked for.
        let mut r = rig();
        with_column(&mut r.app);
        queued(&mut r, 5);
        r.app.on_key(ctrl('w'));
        r.app.on_key(key('j'));
        r.app.on_key(key('j'));
        let _ = requests(&r);
        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.queue_pos, 2);
        assert_eq!(
            r.app.now_playing.as_ref().map(|t| t.id),
            Some(3),
            "the entry under the cursor is the one that plays"
        );
        assert_eq!(resolved_ids(&requests(&r)), vec![3]);
    }

    #[test]
    fn the_queue_cursor_follows_the_music_until_the_listener_takes_it() {
        // Goal: the column is a readout before it is a list, so while nobody is
        // driving it the cursor stays on what is playing - and the moment it is
        // driven, the music must stop dragging it about under the listener's
        // fingers. Method: advance the queue with each focus in turn.
        let mut r = rig();
        with_column(&mut r.app);
        queued(&mut r, 6);
        r.app.load_fresh(3);
        assert_eq!(r.app.queue_selected, 3, "the cursor came along");
        r.app.on_key(ctrl('w'));
        r.app.on_key(key('g'));
        r.app.load_fresh(4);
        assert_eq!(
            r.app.queue_selected, 0,
            "the music moved the cursor out from under the listener"
        );
    }

    #[test]
    fn the_queue_tells_a_suggestion_from_a_choice_entry_by_entry() {
        // Goal: what the radio added is a suggestion and what the listener
        // queued is not, and a view of the queue must not blur the two. The
        // rule is the positional one `playing_from_radio` already answers with,
        // asked of any entry rather than only of the one playing. Method: mark
        // where the radio took over and read the answer either side of it.
        let mut r = rig();
        queued(&mut r, 6);
        r.app.set_radio_from_for_test(Some(4));
        assert!(!r.app.suggested(3), "the listener queued this one");
        assert!(r.app.suggested(4), "the radio added this one");
        assert!(r.app.suggested(5));
        r.app.set_radio_from_for_test(None);
        assert!(!r.app.suggested(5), "nothing here was suggested");
    }

    #[test]
    fn the_playing_track_answers_the_same_rule_the_rows_do() {
        // Goal: one rule with two callers rather than two rules that can come
        // to disagree - the badge beside the playing track and the mark on the
        // queue row are the same question about the same index. Method: walk
        // the queue position across the join and check the two agree at every
        // step.
        let mut r = rig();
        queued(&mut r, 6);
        r.app.set_radio_from_for_test(Some(3));
        for pos in 0..6 {
            r.app.queue_pos = pos;
            assert_eq!(
                r.app.playing_from_radio(),
                r.app.suggested(pos),
                "the two disagree at {pos}"
            );
        }
    }

    #[test]
    fn favouriting_the_playing_track_works_from_either_focus() {
        // Goal: F is not a list action and must not become one - the heart
        // belongs to what is in the speakers whatever has the keyboard.
        // Method: press it with the queue focused.
        let mut r = rig();
        with_column(&mut r.app);
        queued(&mut r, 3);
        r.app.load_fresh(1);
        r.app.on_key(ctrl('w'));
        assert_eq!(r.app.focus(), Focus::Queue);
        r.app.on_key(key('F'));
        assert!(r.app.is_favorite(2), "the playing track was not kept");
    }

    #[test]
    fn a_click_in_a_region_gives_it_the_keyboard() {
        // Goal: the mouse path for focus is the natural gesture - clicking into
        // a region is asking for it - so the key and the pointer cannot leave
        // focus in two different places. Method: click a queue entry, then a
        // browse row, and read the focus after each.
        let mut r = rig();
        r.app.list_inner = Rect {
            x: 1,
            y: 2,
            width: 40,
            height: 6,
        };
        with_column(&mut r.app);
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "X")];
        queued(&mut r, 4);
        r.app.on_mouse(click(86, 12));
        assert_eq!(r.app.focus(), Focus::Queue);
        assert_eq!(r.app.queue_selected, 1, "the entry that was clicked");
        r.app.on_mouse(click(3, 3));
        assert_eq!(r.app.focus(), Focus::List);
        assert_eq!(r.app.selected, 1);
    }

    #[test]
    fn a_queue_entry_is_read_off_the_rect_the_renderer_published() {
        // Goal: a click on the queue is answered by the same pure seam a click
        // on the list is, so there is one place where a cell becomes an intent -
        // and an entry is two rows now, its title and the artist beneath it, so
        // a click on either belongs to the one entry. Method: ask what the title
        // cell, the artist cell, and a cell past the entries each mean.
        let mut r = rig();
        with_column(&mut r.app);
        queued(&mut r, 2);
        assert_eq!(r.app.click_at(86, 10), Click::QueueRow(0), "the title row");
        assert_eq!(
            r.app.click_at(86, 11),
            Click::QueueRow(0),
            "the artist beneath it is the same entry"
        );
        assert_eq!(r.app.click_at(86, 12), Click::QueueRow(1));
        assert_eq!(
            r.app.click_at(86, 13),
            Click::QueueRow(1),
            "and its artist too"
        );
        assert_eq!(
            r.app.click_at(86, 14),
            Click::Nothing,
            "past the last entry is not an entry"
        );
    }

    #[test]
    fn a_second_click_on_a_queue_entry_plays_it() {
        // Goal: the queue answers the pointer the way the list does - one click
        // to put the cursor there, a second to play - so there is one gesture
        // to learn. Method: click the same entry twice.
        let mut r = rig();
        with_column(&mut r.app);
        queued(&mut r, 4);
        let _ = requests(&r);
        // Row 14 is the title row of the third entry, its two-row entries being
        // rows 10-11, 12-13, 14-15 down the column.
        r.app.on_mouse(click(86, 14));
        assert!(
            r.app.now_playing.is_none(),
            "one click only moves the cursor"
        );
        r.app.on_mouse(click(86, 14));
        assert_eq!(r.app.now_playing.as_ref().map(|t| t.id), Some(3));
    }

    #[test]
    fn two_regions_do_not_share_one_double_click() {
        // Goal: the two lists sit side by side, so the same screen row exists
        // in both - and a double click was once a row number and a clock, which
        // would read a click in each region as a double click on the second.
        // Method: click a browse row, then the queue entry on the same row.
        let mut r = rig();
        r.app.list_inner = Rect {
            x: 1,
            y: 10,
            width: 40,
            height: 6,
        };
        with_column(&mut r.app);
        r.app.favorites = vec![track(9, "A", "X"), track(8, "B", "X")];
        queued(&mut r, 4);
        r.app.on_mouse(click(3, 10));
        r.app.on_mouse(click(86, 10));
        assert!(
            r.app.now_playing.is_none(),
            "a click in each region played something"
        );
    }

    #[test]
    fn the_wheel_moves_the_list_under_the_pointer() {
        // Goal: the wheel is a gesture aimed at a region, so it moves what is
        // under the pointer rather than whatever the keyboard happens to hold.
        // Method: scroll over the queue, then over the browse list.
        let mut r = rig();
        r.app.list_inner = Rect {
            x: 1,
            y: 10,
            width: 40,
            height: 6,
        };
        with_column(&mut r.app);
        r.app.favorites = vec![track(1, "A", "X"), track(2, "B", "X")];
        queued(&mut r, 5);
        r.app.on_mouse(wheel_down(86, 11));
        assert_eq!(r.app.queue_selected, 1);
        assert_eq!(r.app.selected, 0);
        r.app.on_mouse(wheel_down(3, 11));
        assert_eq!(r.app.selected, 1);
        assert_eq!(r.app.queue_selected, 1, "the queue stayed where it was");
    }

    #[test]
    fn an_empty_queue_says_it_is_empty_rather_than_blaming_the_width() {
        // Goal: there are three reasons there is no queue to focus - the
        // terminal is too narrow, the column is folded away, or nothing has been
        // queued yet - and a key that named the width on a two-hundred-column
        // terminal would be telling the listener to fix something that is not
        // wrong. Method: ask for the queue with nothing in it.
        let mut r = rig();
        r.app.on_key(ctrl('w'));
        let said = r.app.notice.clone().unwrap_or_default();
        assert!(
            said.to_lowercase().contains("queue"),
            "an empty queue is not named: {said:?}"
        );
        assert!(
            !said.contains("120"),
            "an empty queue was blamed on the width: {said:?}"
        );
    }

    #[test]
    fn a_folded_column_is_named_as_such_rather_than_blamed_on_the_width() {
        // Goal: the third reason. A listener who folded the column away is on a
        // terminal that is already wide enough, so telling them to widen it
        // sends them to fix what is not broken - the answer is the key that
        // brings it back. Method: hide it with a queue in hand, then ask for
        // the queue.
        let mut r = rig();
        queued(&mut r, 4);
        r.app.on_key(key('W'));
        r.app.on_key(ctrl('w'));
        let said = r.app.notice.clone().unwrap_or_default();
        assert!(
            said.contains('W'),
            "the key that brings it back is not named: {said:?}"
        );
        assert!(
            !said.contains("120"),
            "a folded column was blamed on the width: {said:?}"
        );
    }

    #[test]
    fn an_empty_queue_is_said_before_anything_about_the_column() {
        // Goal: the two reasons can hold at once, and only one of them is worth
        // acting on - bringing an empty column back shows nothing. Method: fold
        // the column away with nothing queued and read which answer came out.
        let mut r = rig();
        r.app.on_key(key('W'));
        r.app.on_key(ctrl('w'));
        let said = r.app.notice.clone().unwrap_or_default();
        assert!(
            said.contains("Enter"),
            "the empty queue was not the answer: {said:?}"
        );
    }

    /// Load `have` favorites out of a listing of `total`, ready to play, with a
    /// list rect so the paging trigger has geometry to reason about.
    fn favorites_partly_loaded(r: &mut Rig, have: u64, total: u32) {
        r.app.start();
        r.to_app
            .send(favorites_page(0, 1..have + 1, total))
            .unwrap();
        r.app.drain_worker();
        r.app.list_inner = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        let _ = requests(r); // clear the view's own paging requests
    }

    /// The `FillQueue` requests among what the app has sent.
    fn fills(reqs: &[ToWorker]) -> Vec<(QueueSource, u32)> {
        reqs.iter()
            .filter_map(|c| match c {
                ToWorker::FillQueue { source, offset, .. } => Some((source.clone(), *offset)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn playing_a_partly_loaded_listing_fills_the_queue_from_where_it_ends() {
        // Goal: the fix. Pressing Enter on favorites that are only partly paged
        // in must build the queue from the loaded rows *and* start pulling the
        // rest, so shuffle ends up over the whole listing rather than the subset
        // that happened to be on screen. Method: three of ten loaded, play, and
        // read what was asked.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 10);

        r.app.on_key(code(KeyCode::Enter));
        assert_eq!(r.app.queue.len(), 3, "the queue starts as the loaded rows");
        assert_eq!(
            fills(&requests(&r)),
            vec![(QueueSource::Favorites, 3)],
            "and the fill continues from where they end"
        );
    }

    #[test]
    fn a_filled_page_grows_the_queue_and_chains_the_next() {
        // Goal: a fill page lands, the queue grows by it, its order grows with
        // it, and the next page is asked for - up until the listing is in.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 8);
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 3,
                page: track_page(4..7, 8),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.queue.len(), 6, "the queue grew by the page");
        assert_eq!(r.app.order.len(), 6, "and the play order grew with it");
        assert_eq!(
            fills(&requests(&r)),
            vec![(QueueSource::Favorites, 6)],
            "and it asked for the next page"
        );

        // The last page brings the listing in; the fill then stops.
        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 6,
                page: track_page(7..9, 8),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(r.app.queue.len(), 8, "the whole listing is in the queue");
        assert!(
            fills(&requests(&r)).is_empty(),
            "and nothing more is asked for once it is complete"
        );
    }

    #[test]
    fn a_fill_page_does_not_re_preload_when_a_next_is_already_queued() {
        // Goal: #53. A track plays with the next already preloaded in mpv. A
        // queue fill page landing must not schedule that next again - the
        // duplicate preload that piled up as [A, B, B, ...] and looped the
        // second track, because each B -> B is a same-id transition the app
        // cannot see.
        let mut r = rig();
        r.app.queue = (1..=5).map(|i| track(i, "T", "A")).collect();
        r.app.order = (0..5).collect();
        r.app.queue_pos = 0;
        r.app.now_playing = Some(track(1, "T", "A"));
        r.app.status.playing = true;
        r.app.status.has_next = true; // mpv holds the preloaded next
        r.app.next_intended = Some(2); // and the app queued it
        r.app.current_target = None; // the current track is settled
        r.app.queue_fill = Some(QueueFill {
            source: QueueSource::Favorites,
            next: 5,
            total: 10,
            inflight: true,
        });
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 5,
                page: track_page(6..8, 10),
            })
            .unwrap();
        r.app.drain_worker();

        assert_eq!(r.app.queue.len(), 7, "the fill still grows the queue");
        assert!(
            !requests(&r)
                .iter()
                .any(|c| matches!(c, ToWorker::Resolve(_))),
            "but it must not re-issue a preload when a next is already queued"
        );
    }

    #[test]
    fn a_fill_page_preloads_when_nothing_is_queued_behind_the_current_track() {
        // Goal: the flip side, and the reason on_queue_filled preloads at all. A
        // fill that lands while the last loaded track plays with nothing behind
        // it is what makes the newly-available next there to load.
        let mut r = rig();
        r.app.queue = (1..=3).map(|i| track(i, "T", "A")).collect();
        r.app.order = (0..3).collect();
        r.app.queue_pos = 0;
        r.app.now_playing = Some(track(1, "T", "A"));
        r.app.status.playing = true;
        r.app.status.has_next = false; // nothing preloaded yet
        r.app.next_intended = None;
        r.app.current_target = None; // the current track is settled
        r.app.queue_fill = Some(QueueFill {
            source: QueueSource::Favorites,
            next: 3,
            total: 10,
            inflight: true,
        });
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 3,
                page: track_page(4..6, 10),
            })
            .unwrap();
        r.app.drain_worker();

        assert!(
            requests(&r)
                .iter()
                .any(|c| matches!(c, ToWorker::Resolve(_))),
            "with nothing queued behind, the fill preloads the now-available next"
        );
    }

    #[test]
    fn a_confirmed_total_is_not_re_inflated_by_a_later_page() {
        // Goal: once the true end is known - here as the queue fill would set it -
        // a later view page still carrying the service's over-count must not
        // raise the total back up, or scrolling after the correction would undo
        // it. The view has three loaded and the fill found the listing is seven.
        let mut p = Paging::default();
        p.confirm(7, 3);
        assert_eq!(p.total, 7, "the fill's count, not the service's");

        // The view scrolls and loads its next page; the service still says 8.
        let mut rows: Vec<u32> = (0..3).collect();
        p.absorb(
            &mut rows,
            3,
            priel_core::Page {
                items: vec![3, 4, 5],
                total: 8,
            },
        );
        assert_eq!(rows.len(), 6, "the rows still grow by the page");
        assert_eq!(p.total, 7, "but the confirmed count holds against the 8");
    }

    #[test]
    fn a_fill_reaching_the_end_corrects_an_over_counted_heading() {
        // Goal: #36. The service advertises one more favourite than it delivers
        // (8 claimed, 7 real). The lazy view heading trusts the 8, but the
        // background queue fill pages the whole listing and hits the empty page
        // at offset 7 - so the heading's total is corrected to 7 without the
        // listener ever scrolling to the end.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 8);
        assert_eq!(
            r.app.favorites_paging.total, 8,
            "the service's advertised count to begin with"
        );
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        for (offset, ids) in [(3u32, 4..7u64), (6, 7..8)] {
            r.to_app
                .send(FromWorker::QueueFilled {
                    source: QueueSource::Favorites,
                    offset,
                    page: track_page(ids, 8),
                })
                .unwrap();
            r.app.drain_worker();
            let _ = requests(&r);
        }
        assert_eq!(r.app.queue.len(), 7, "the fill has the real seven rows");

        // At offset 7 the service returns nothing: the listing's true end.
        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 7,
                page: track_page(7..7, 8),
            })
            .unwrap();
        r.app.drain_worker();

        assert_eq!(
            r.app.favorites_paging.total, 7,
            "the heading's total is corrected from the service's 8 to the real 7"
        );
    }

    #[test]
    fn a_filter_keeps_the_queue_a_snapshot_and_does_not_fill() {
        // Goal: a filtered listing is a deliberate subset, so the queue is the
        // filtered rows and paging the unfiltered listing in behind them would
        // fill it with rows the filter hides. Method: filter, play, and check
        // nothing is asked for.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 10);
        r.app.filter = "t".into(); // matches the test tracks' title

        r.app.on_key(code(KeyCode::Enter));
        assert!(
            fills(&requests(&r)).is_empty(),
            "a filtered queue must not fill from the unfiltered listing"
        );
    }

    #[test]
    fn a_fill_page_for_a_replaced_queue_is_dropped() {
        // Goal: correlation by source, not arrival. A page still on its way for
        // the queue that was here must not be appended to whatever queue is now
        // current. Method: play favorites, replace the queue directly, then hand
        // back a favorites fill page.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 10);
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        // A different queue is now current, from something else.
        r.app.set_queue(vec![track(99, "Elsewhere", "X")]);
        let before = r.app.queue.len();

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 3,
                page: track_page(4..7, 10),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            r.app.queue.len(),
            before,
            "a page for the old queue must not grow the new one"
        );
    }

    #[test]
    fn a_fill_page_naming_a_different_source_is_dropped() {
        // Goal: the source-match, not just the presence of a fill. When one
        // filling queue has replaced another, a page still on its way for the
        // old source can land while a fill for the *new* source is waiting at
        // the same offset - and only the source tells them apart. Method: a
        // favorites fill is in progress; deliver a page naming a search instead.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 10);
        r.app.on_key(code(KeyCode::Enter)); // a favorites fill is now waiting at offset 3
        let before = r.app.queue.len();

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Search("x".into()),
                offset: 3,
                page: track_page(4..7, 10),
            })
            .unwrap();
        r.app.drain_worker();
        assert_eq!(
            r.app.queue.len(),
            before,
            "a page whose source is not the queue's must not grow it"
        );
    }

    #[test]
    fn a_fully_loaded_listing_needs_no_fill() {
        // Goal: the ordinary small case - a playlist wholly on screen - asks for
        // nothing. Method: all three of three loaded, play, and check.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 3);
        r.app.on_key(code(KeyCode::Enter));
        assert!(
            fills(&requests(&r)).is_empty(),
            "a queue that is already the whole listing has nothing to fill"
        );
    }

    #[test]
    fn the_fill_deals_into_the_shuffle_so_it_covers_the_whole_listing() {
        // Goal: the point of the whole change - shuffle over the listing, not
        // the loaded subset. Method: shuffle on, play a partly-loaded listing,
        // let a fill page land, and check the order is a permutation of the
        // whole grown queue with every entry once.
        let mut r = rig();
        favorites_partly_loaded(&mut r, 3, 6);
        r.app.shuffle = true;
        r.app.on_key(code(KeyCode::Enter));
        let _ = requests(&r);

        r.to_app
            .send(FromWorker::QueueFilled {
                source: QueueSource::Favorites,
                offset: 3,
                page: track_page(4..7, 6),
            })
            .unwrap();
        r.app.drain_worker();

        assert_eq!(r.app.queue.len(), 6);
        let mut seen: Vec<usize> = r.app.order.clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..6).collect::<Vec<_>>(),
            "every entry of the grown queue is in the order exactly once"
        );
    }

    #[test]
    fn a_newer_release_lands_on_the_notice_line_with_how_to_update() {
        // Goal: the update check is worth a word, not an interruption, and the
        // word names the one command that acts on it. Method: hand the app an
        // UpdateAvailable reply the way the worker would, and read the notice.
        let mut r = rig();
        r.app.check_for_updates();
        assert!(
            matches!(requests(&r)[..], [ToWorker::CheckUpdate]),
            "the check is one request and no more"
        );

        r.to_app
            .send(FromWorker::UpdateAvailable("v0.2.0".into()))
            .unwrap();
        r.app.drain_worker();
        let notice = r.app.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("v0.2.0"),
            "the notice names the version: {notice}"
        );
        assert!(
            notice.contains("priel --update"),
            "and how to get it: {notice}"
        );
    }

    #[test]
    fn folding_the_column_away_and_back_is_one_key() {
        // Goal: one binding does both halves, so there is no state a listener
        // can get into where the key they pressed will not undo itself.
        // Method: press it twice and read the intent after each.
        let mut r = rig();
        assert!(r.app.queue_shown, "the column starts shown");
        r.app.on_key(key('W'));
        assert!(!r.app.queue_shown);
        r.app.on_key(key('W'));
        assert!(r.app.queue_shown);
    }

    /// A track that names an album cover.
    fn track_with_cover(id: u64, cover: &str) -> Track {
        Track {
            cover: cover.into(),
            ..track(id, "T", "A")
        }
    }

    #[test]
    fn the_cover_is_asked_for_once_per_playing_track() {
        // Goal: the fetch is triggered by a poll, so it must fire exactly once
        // when a track starts and not again every tick, or the worker drowns in
        // duplicate requests. Method: put a track in the speakers, refresh
        // twice, and count the fetches.
        let mut r = rig();
        r.app.now_playing = Some(track_with_cover(1, "aaaa-bbbb"));
        r.app.refresh();
        r.app.refresh();

        let fetches: Vec<_> = requests(&r)
            .into_iter()
            .filter_map(|c| match c {
                ToWorker::FetchCover { track_id, cover_id } => Some((track_id, cover_id)),
                _ => None,
            })
            .collect();
        assert_eq!(
            fetches,
            vec![(1, "aaaa-bbbb".to_string())],
            "one fetch, carrying the track and its cover id"
        );
    }

    #[test]
    fn a_track_with_no_cover_is_never_asked() {
        // Goal: an empty cover id is a track whose listing named none, so there
        // is nothing to fetch and a request would only 404. Method: a track
        // with no cover, refreshed.
        let mut r = rig();
        r.app.now_playing = Some(track(1, "T", "A"));
        r.app.refresh();
        assert!(
            !requests(&r)
                .iter()
                .any(|c| matches!(c, ToWorker::FetchCover { .. })),
            "a coverless track was asked for anyway"
        );
    }

    #[test]
    fn a_cover_that_arrives_for_the_playing_track_is_kept() {
        // Goal: the reply lands and is stored against its track, so the renderer
        // can find it. Method: play a track, hand back a decoded cover for it,
        // and read it back through the accessor the renderer uses.
        let mut r = rig();
        r.app.now_playing = Some(track_with_cover(1, "aaaa"));
        let image = crate::art::Image {
            width: 1,
            height: 1,
            rgb: vec![1, 2, 3],
        };
        r.to_app
            .send(FromWorker::Cover {
                track_id: 1,
                image: image.clone(),
            })
            .expect("send");
        r.app.drain_worker();
        assert_eq!(
            r.app.cover_for_now_playing(),
            Some(&image),
            "the cover for the playing track is available to draw"
        );
    }

    #[test]
    fn a_cover_that_arrives_after_its_track_stopped_is_dropped() {
        // Goal: a decode that finished just after the track changed must not be
        // drawn over the one now playing. Method: hand back a cover for track 1
        // while track 2 is in the speakers.
        let mut r = rig();
        r.app.now_playing = Some(track_with_cover(2, "bbbb"));
        r.to_app
            .send(FromWorker::Cover {
                track_id: 1,
                image: crate::art::Image {
                    width: 1,
                    height: 1,
                    rgb: vec![9, 9, 9],
                },
            })
            .expect("send");
        r.app.drain_worker();
        assert!(
            r.app.cover_for_now_playing().is_none(),
            "a stale cover was kept for the wrong track"
        );
    }
}
