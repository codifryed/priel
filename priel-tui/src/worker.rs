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

//! Background worker: owns the blocking `priel_core::Client` so network calls
//! never touch the UI thread. Talks to the app over two channels.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use priel_core::auth::Credentials;
use priel_core::{Client, Fault, Mix, Page, Playlist, Quality, ResolvedStream, Track};
use priel_player::graph::{self, AudioGraph, ClockRates, GraphError};
use priel_player::setup;

/// Rows one favorites request asks for.
///
/// Every listing call takes an explicit limit and this is the favorites one:
/// large enough that a screenful never waits on a round trip, small enough that
/// the first page is on screen quickly.
pub const FAVORITES_PAGE: u32 = 100;

/// Rows one playlists request asks for. A playlist row is cheap and a user has
/// far fewer playlists than favorites, so this is usually the whole listing.
pub const PLAYLISTS_PAGE: u32 = 100;

/// Rows one playlist-tracks request asks for. Larger than the others because a
/// playlist is normally read from the top and played straight through.
pub const PLAYLIST_TRACKS_PAGE: u32 = 200;

/// Rows one search request asks for. The smallest of these: a searcher looks
/// at the head of the results, and a wrong query should cost the least.
pub const SEARCH_PAGE: u32 = 50;

/// Rows one mixes request asks for. Sized like the playlists for the same
/// reason - a listener has a handful of mixes, not a library of them - and this
/// listing is asked for again on every visit, so the request wants to be cheap.
pub const MIXES_PAGE: u32 = 100;

/// Rows one request for a mix's tracks asks for. Sized like a playlist's: a mix
/// is read from the top and played straight through.
pub const MIX_TRACKS_PAGE: u32 = 200;

/// Rows one radio request adds to the queue.
///
/// Much the smallest of these, and deliberately: this is the one listing nobody
/// asked for by name, so it takes what a listener might reasonably sit through
/// before deciding, rather than a library's worth of it. Running out asks again
/// from whatever was playing by then, which is a better mix than one seeded an
/// hour earlier.
pub const RADIO_PAGE: u32 = 25;

/// Which listing a queue was filled from, so the fill can page it on regardless
/// of what the on-screen view has since become.
///
/// A queue built by pressing Enter on a listing keeps growing in the background
/// until it holds the whole listing, and that fill is correlated by this rather
/// than by the view - the listener can navigate away, and a page still landing
/// for the queue's source is still the queue's, where a page for the view is the
/// view's. The one that cannot be filled is not here: a filtered listing is a
/// deliberate subset and stays the snapshot it was.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum QueueSource {
    Favorites,
    Playlist(String),
    Mix(String),
    Search(String),
}

/// A request for one page of a listing.
///
/// Every one carries an explicit offset and limit, and the reply carries the
/// same identity back: a page for a listing the view has since thrown away can
/// then be recognised and dropped instead of appended in the wrong place.
pub enum ToWorker {
    LoadFavorites {
        offset: u32,
        limit: u32,
    },
    LoadPlaylists {
        offset: u32,
        limit: u32,
    },
    LoadPlaylistTracks {
        uuid: String,
        offset: u32,
        limit: u32,
    },
    LoadMixes {
        offset: u32,
        limit: u32,
    },
    LoadMixTracks {
        mix_id: String,
        offset: u32,
        limit: u32,
    },
    Search {
        query: String,
        offset: u32,
        limit: u32,
    },
    /// The first page of a track's own radio mix, to carry a queue on past its
    /// end.
    ///
    /// Deliberately not a [`ToWorker::LoadMixTracks`] with a different caller:
    /// the two go to the same endpoint and mean different things, one filling a
    /// listing on screen and the other extending the queue behind it. They can
    /// be in flight for the same mix at once, and a reply that stood for either
    /// would land in whichever the app guessed. No offset, because there is
    /// only ever a first page of one - what follows it is the radio for
    /// whatever is playing by then.
    LoadRadio {
        mix_id: String,
        limit: u32,
    },
    Resolve(u64),
    /// Put a track in the user's favorites, or take it back off them.
    ///
    /// The one request here that *changes* remote state. It carries the state
    /// the interface has already shown rather than "toggle", because a toggle
    /// would be interpreted against whatever the service held by the time it
    /// arrived, which is not what the user saw when they pressed the key.
    SetFavorite {
        track_id: u64,
        favorite: bool,
    },
    /// Make a new, empty playlist.
    ///
    /// The only request here whose *success* the interface has to hear about:
    /// the uuid is the service's to choose, so until the reply arrives there is
    /// no row to show and nothing to open.
    CreatePlaylist {
        title: String,
    },
    /// Give a playlist a new title.
    ///
    /// The interface has already shown the new one, so `was` travels with it:
    /// putting a refused rename back needs the exact title that was there, and
    /// by the time the refusal arrives the interface no longer holds it.
    RenamePlaylist {
        uuid: String,
        title: String,
        was: String,
    },
    /// Delete a playlist. Carries the title so a refusal can name what survived.
    DeletePlaylist {
        uuid: String,
        title: String,
    },
    /// Put a track at the end of a playlist.
    ///
    /// Carries the playlist's title because the notice that reports it is the
    /// only sign the user gets: the playlist being added to is usually not the
    /// one on screen, so nothing visible changes on its own.
    AddToPlaylist {
        uuid: String,
        title: String,
        track_id: u64,
    },
    /// Take a track out of a playlist.
    RemoveFromPlaylist {
        uuid: String,
        track_id: u64,
    },
    /// Read the chain to the output device. Runs `pw-dump` and waits for it,
    /// which is why it is here and not on the UI thread.
    ReadAudioGraph,
    /// Read just the sound server's graph clock, at track start, so the verdict
    /// can tell a resample the server hid on an output with no ALSA readout. The
    /// reply is [`FromWorker::OutputClock`]. Runs `pw-dump`, so it is here.
    ReadClock,
    /// Write priel's own `pipewire.conf.d` drop-in so the sound server may clock
    /// at the rates in `allowed_hz`. Touches the filesystem, which is why it is
    /// here. The reply is [`FromWorker::AudioSetUp`].
    SetUpAudio {
        allowed_hz: Vec<u32>,
    },
    /// Restart the user's sound server so a freshly written drop-in takes effect.
    /// Waits on a subprocess. The reply is [`FromWorker::AudioRestarted`].
    RestartAudio,
    /// Switch a Bluetooth device to the A2DP profile that carries a better codec.
    /// Waits on `wpctl`, so it is here. The reply is [`FromWorker::BtCodecSwitched`].
    SwitchBtCodec {
        device_id: u32,
        profile_index: u32,
    },
    /// One page of the listing a queue was filled from, to grow the queue up to
    /// the whole listing in the background.
    ///
    /// Distinct from the `Load*` requests, which page the *view*: this pages the
    /// *queue's* source, correlated by [`QueueSource`] so it keeps arriving for
    /// the right queue after the view has moved on. The reply is
    /// [`FromWorker::QueueFilled`].
    FillQueue {
        source: QueueSource,
        offset: u32,
        limit: u32,
    },
    /// Ask the forge whether a newer release than this build exists.
    ///
    /// One unauthenticated read of a public version number, on the worker like
    /// every other network call. It never downloads anything - the reply only
    /// says whether there is something to tell the listener about.
    CheckUpdate,
    /// Fetch and decode a track's album cover.
    ///
    /// Both the fetch and the decode happen here, off the render thread: the
    /// UI is handed pixels, never bytes, because decoding a JPEG allocates and
    /// takes real time and the render loop may do neither. Carries the track id
    /// so the reply can be dropped if the track has changed by the time it
    /// lands - replies are correlated by id, never by order.
    FetchCover {
        track_id: u64,
        cover_id: String,
    },
}

/// Which request a reply belongs to.
///
/// Successes have always carried their identity - a playlist reply names its
/// uuid, a resolve names its track - and a failure has to as well. A view
/// waiting on a page that died cannot otherwise tell that the death was its
/// own, so it waits for a reply that is never coming.
///
/// Each identity is everything that makes the request unique, which for a paged
/// listing is the page *and* the listing: a failure for the playlist the user
/// has just left would otherwise stall the one they have just opened.
///
/// The display name lives here too, so the words on screen and the identity the
/// interface branches on cannot drift apart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Task {
    Startup,
    Favorites {
        offset: u32,
    },
    Playlists {
        offset: u32,
    },
    PlaylistTracks {
        uuid: String,
        offset: u32,
    },
    Mixes {
        offset: u32,
    },
    MixTracks {
        mix_id: String,
        offset: u32,
    },
    /// A radio that could not be fetched. Named by the mix so a second one for
    /// a different track is a different failure.
    Radio {
        mix_id: String,
    },
    Search {
        query: String,
        offset: u32,
    },
    Resolve,
    /// A favorite that could not be changed. The track names it, and `wanted`
    /// is the state that was asked for: "add track 7" and "remove track 7" are
    /// two different requests, and the interface has to know which of them it
    /// is putting back.
    SetFavorite {
        track_id: u64,
        wanted: bool,
    },
    /// A playlist edit that did not happen. Named by the playlist it was aimed
    /// at, so a failure for one cannot stop a view showing another - and by the
    /// track as well where there is one, since two edits to the same playlist
    /// are two requests.
    CreatePlaylist,
    RenamePlaylist {
        uuid: String,
        was: String,
    },
    DeletePlaylist {
        uuid: String,
    },
    AddToPlaylist {
        uuid: String,
        track_id: u64,
    },
    RemoveFromPlaylist {
        uuid: String,
        track_id: u64,
    },
}

impl std::fmt::Display for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Startup => "startup",
            Self::Favorites { .. } => "favorites",
            Self::Playlists { .. } => "playlists",
            Self::PlaylistTracks { .. } => "playlist tracks",
            Self::Mixes { .. } => "mixes",
            Self::MixTracks { .. } => "mix tracks",
            Self::Radio { .. } => "the radio",
            Self::Search { .. } => "search",
            Self::Resolve => "resolve",
            // Named by what did not happen rather than by the endpoint: this is
            // the only failure here the user asked for directly, so the notice
            // should read back the action they took.
            Self::SetFavorite { wanted: true, .. } => "adding to favorites",
            Self::SetFavorite { wanted: false, .. } => "removing from favorites",
            // Named the same way: what the user asked for, not the endpoint.
            // These are the sentences a refusal is read back with, and "the
            // playlist was not deleted" is the one that has to be unmistakable.
            Self::CreatePlaylist => "creating the playlist",
            Self::RenamePlaylist { .. } => "renaming the playlist",
            Self::DeletePlaylist { .. } => "deleting the playlist",
            Self::AddToPlaylist { .. } => "adding to the playlist",
            Self::RemoveFromPlaylist { .. } => "removing from the playlist",
        })
    }
}

/// A page of a listing, named by the request it answers.
pub enum FromWorker {
    Favorites {
        offset: u32,
        page: Page<Track>,
    },
    Playlists {
        offset: u32,
        page: Page<Playlist>,
    },
    PlaylistTracks {
        uuid: String,
        offset: u32,
        page: Page<Track>,
    },
    Mixes {
        offset: u32,
        page: Page<Mix>,
    },
    MixTracks {
        mix_id: String,
        offset: u32,
        page: Page<Track>,
    },
    /// A track's radio mix, to be appended to the play queue.
    ///
    /// Names the mix it answers for the reason every listing reply here does:
    /// a radio and an open mix listing can be in flight for the same id at the
    /// same time, and only the request that asked knows which of them this is.
    Radio {
        mix_id: String,
        page: Page<Track>,
    },
    SearchResults {
        query: String,
        offset: u32,
        page: Page<Track>,
    },
    Resolved(u64, ResolvedStream),
    /// A playlist now exists that did not before.
    ///
    /// The one success in this enum that is not a listing, and it is here
    /// because the interface could not have guessed it: the uuid came from the
    /// service, and without it the new row cannot be opened or edited.
    PlaylistCreated(Playlist),
    /// A playlist is gone. The interface waited for this rather than assuming
    /// it - see `App::delete_playlist` for why a delete does not assume.
    PlaylistDeleted {
        uuid: String,
    },
    /// A track is out of a playlist, and the row may now be dropped.
    PlaylistTrackRemoved {
        uuid: String,
        track_id: u64,
    },
    /// A track went into a playlist.
    ///
    /// Reported although nothing failed, which no other write here does. The
    /// reason is that the playlist added to is usually not the one on screen,
    /// so there is nothing the user can look at to see that it worked; the
    /// notice is the only confirmation there is.
    PlaylistTrackAdded {
        title: String,
    },
    /// The chain to the output device, or the reason there is none to show.
    ///
    /// The failure travels as `GraphError` rather than `Failed`: nothing about
    /// it is a request that went wrong, and the overlay has its own sentence
    /// for each case.
    AudioGraph(Result<AudioGraph, GraphError>),
    /// The sound server's graph clock at track start, or `None` when it could
    /// not be read - a background read, so a failure is silent, not a banner.
    /// The app hands it to the player, where the verdict falls back to it.
    OutputClock(Option<ClockRates>),
    /// priel's rates drop-in was written, or the reason it was not. `Ok` carries
    /// the path written, for the sentence that offers the restart; `Err` carries
    /// a reason already fit to show.
    AudioSetUp(Result<String, String>),
    /// The sound server was restarted, or the reason it was not. `Err` carries a
    /// reason already fit to show, so the interface can say the file landed but
    /// the restart did not.
    AudioRestarted(Result<(), String>),
    /// The Bluetooth codec was switched, or the reason it was not. `Err` carries
    /// a reason already fit to show.
    BtCodecSwitched(Result<(), String>),
    /// One page of the listing a queue is being filled from.
    ///
    /// Carries the source it answers for, so a page for a queue the listener has
    /// since replaced is recognised and dropped rather than appended to whatever
    /// queue is now current. A failed fetch sends nothing: a background fill that
    /// cannot reach a page simply stops, and the queue keeps what it has.
    QueueFilled {
        source: QueueSource,
        offset: u32,
        page: Page<Track>,
    },
    /// A newer release exists on the forge; the string is its tag.
    ///
    /// Only ever sent when there *is* one - like [`FromWorker::Cover`], a check
    /// that finds nothing sends nothing, because the interface has nothing to
    /// say when priel is already current or the forge could not be reached.
    UpdateAvailable(String),
    /// A decoded album cover, and the track it belongs to.
    ///
    /// Only ever sent on success: a cover that would not fetch or decode is
    /// absent, the same as one that was never asked for, and the box already
    /// knows how to draw nothing. The id is what lets the app drop a cover that
    /// arrived after its track stopped playing.
    Cover {
        track_id: u64,
        image: crate::art::Image,
    },
    /// A request failed. `task` says which one, so the view that was waiting can
    /// stop; `fault` is what the interface branches on; `detail` is the sentence
    /// it shows. Nothing may match on `detail` - that is the whole point of
    /// `fault` existing.
    Failed {
        task: Task,
        fault: Fault,
        detail: String,
    },
}

pub struct Worker {
    pub tx: Sender<ToWorker>,
    pub rx: Receiver<FromWorker>,
}

impl Worker {
    /// A worker with no thread behind it: the caller drives both directions.
    ///
    /// Returns the worker plus the far ends of its channels, so a test can post
    /// `FromWorker` messages and assert on the `ToWorker` requests the app makes.
    #[cfg(test)]
    pub fn rigged() -> (Self, Sender<FromWorker>, Receiver<ToWorker>) {
        let (tx, cmd_rx) = mpsc::channel::<ToWorker>();
        let (evt_tx, rx) = mpsc::channel::<FromWorker>();
        (Self { tx, rx }, evt_tx, cmd_rx)
    }
}

/// Turn a failed request into the reply the app branches on.
///
/// The classification comes from the core, the only layer that can tell a
/// refused session from a dropped connection. `detail` is one line for the
/// screen; the full chain goes to the log, which has room for it.
fn failed(task: Task, e: &anyhow::Error) -> FromWorker {
    log::warn!("{task}: {e:#}");
    let detail = format!("{task}: {e}");
    FromWorker::Failed {
        task,
        fault: Fault::of(e),
        detail,
    }
}

/// Start a worker.
///
/// Both paths are passed in rather than resolved here so the caller decides,
/// and so a test is not at the mercy of whatever happens to be configured on
/// the machine running it.
pub fn spawn(token_path: String, credentials_path: String, cache_cap: u64) -> Worker {
    spawn_with_cap(
        move || {
            // With credentials configured the client renews its own session, which
            // is what stops the access token expiring mid-listen. Without them it
            // still works from the stored token, until that token runs out.
            let mut client = match Credentials::load(&credentials_path) {
                Ok(creds) => Client::with_auth(&token_path, creds.into_config())?,
                Err(_) => Client::from_token_file(&token_path)?,
            };
            client.connect()?;
            Ok(client)
        },
        cache_cap,
    )
}

/// Spawn a worker around a caller-supplied client factory.
///
/// The factory runs *on the worker thread* so a failure to authenticate becomes
/// a `FromWorker::Error` on screen rather than a panic before the UI exists.
/// Tests use this to point the worker at a stub origin with no token file.
/// Make a playlist.
///
/// The one edit that answers with something. The uuid is the service's to
/// choose, so until this comes back the interface has no row it could show.
fn created(client: &mut Client, title: &str) -> FromWorker {
    match client.create_playlist(title) {
        Ok(made) => {
            log::info!("created playlist {} ({})", made.title, made.uuid);
            FromWorker::PlaylistCreated(made)
        }
        Err(e) => failed(Task::CreatePlaylist, &e),
    }
}

/// Retitle a playlist.
///
/// Silence is success here, as it is for a favorite: the new title is already
/// on screen, and the reply worth the channel is the one saying it should not
/// be. `was` travels only so a refusal can put back the exact title.
fn renamed(client: &mut Client, uuid: String, title: &str, was: String) -> Option<FromWorker> {
    match client.rename_playlist(&uuid, title) {
        Ok(()) => {
            log::info!("renamed playlist {uuid} to {title}");
            None
        }
        Err(e) => Some(failed(Task::RenamePlaylist { uuid, was }, &e)),
    }
}

/// Delete a playlist.
///
/// **Not silence.** The interface still shows the row and is waiting to be told
/// it may stop: this is the one edit that cannot be undone, so it is reported
/// rather than assumed.
fn deleted(client: &mut Client, uuid: String, title: &str) -> FromWorker {
    match client.delete_playlist(&uuid) {
        Ok(()) => {
            log::info!("deleted playlist {title} ({uuid})");
            FromWorker::PlaylistDeleted { uuid }
        }
        Err(e) => failed(Task::DeletePlaylist { uuid }, &e),
    }
}

/// Put a track into a playlist.
///
/// Reported on success although nothing failed, which no other write does. The
/// playlist added to is usually not the one on screen, so the notice this
/// becomes is the only confirmation the listener gets.
fn added(client: &mut Client, uuid: &str, title: String, track_id: u64) -> FromWorker {
    match client.add_track_to_playlist(uuid, track_id) {
        Ok(()) => {
            log::info!("added track {track_id} to playlist {title} ({uuid})");
            FromWorker::PlaylistTrackAdded { title }
        }
        Err(e) => failed(
            Task::AddToPlaylist {
                uuid: uuid.to_string(),
                track_id,
            },
            &e,
        ),
    }
}

/// Take a track out of a playlist. Waits, for the reason a delete does.
fn removed(client: &mut Client, uuid: String, track_id: u64) -> FromWorker {
    match client.remove_track_from_playlist(&uuid, track_id) {
        Ok(()) => {
            log::info!("removed track {track_id} from playlist {uuid}");
            FromWorker::PlaylistTrackRemoved { uuid, track_id }
        }
        Err(e) => failed(Task::RemoveFromPlaylist { uuid, track_id }, &e),
    }
}

/// Carry out one request and say what to send back.
///
/// Split out of the loop so the thread's body stays readable as requests are
/// added, and so the answer to "what does this request reply with" is one
/// `match` rather than a hundred lines of thread setup around it. `None` is a
/// success the interface does not need to hear about.
/// Write priel's rates drop-in - and only that file - into the user's config.
///
/// Always answers, so the overlay is never left waiting: an `Err` carries a
/// reason already fit to show.
fn set_up_audio(allowed_hz: &[u32]) -> FromWorker {
    let done = match setup::conf_dir() {
        Some(dir) => setup::write_rates(&dir, allowed_hz)
            .map(|path| path.display().to_string())
            .map_err(|e| e.to_string()),
        None => Err("priel could not find your config directory".to_string()),
    };
    if let Err(e) = &done {
        log::info!("set up audio: {e}");
    }
    FromWorker::AudioSetUp(done)
}

/// Switch a Bluetooth device's A2DP profile. Waits on `wpctl`, so it is here and
/// not on the render thread.
fn switch_bt(device_id: u32, profile_index: u32) -> FromWorker {
    FromWorker::BtCodecSwitched(setup::switch_bt_codec(device_id, profile_index))
}

#[allow(
    clippy::too_many_lines,
    reason = "one dispatch match over every request; an arm per feature, splitting it would scatter the dispatch"
)]
fn serve(client: &mut Client, cmd: ToWorker, cache_cap: u64) -> Option<FromWorker> {
    Some(match cmd {
        ToWorker::LoadFavorites { offset, limit } => match client.favorite_tracks(offset, limit) {
            Ok(page) => FromWorker::Favorites { offset, page },
            Err(e) => failed(Task::Favorites { offset }, &e),
        },
        ToWorker::LoadPlaylists { offset, limit } => match client.user_playlists(offset, limit) {
            Ok(page) => FromWorker::Playlists { offset, page },
            Err(e) => failed(Task::Playlists { offset }, &e),
        },
        ToWorker::LoadPlaylistTracks {
            uuid,
            offset,
            limit,
        } => match client.playlist_tracks(&uuid, offset, limit) {
            Ok(page) => FromWorker::PlaylistTracks { uuid, offset, page },
            Err(e) => failed(Task::PlaylistTracks { uuid, offset }, &e),
        },
        ToWorker::LoadMixes { offset, limit } => match client.user_mixes(offset, limit) {
            Ok(page) => FromWorker::Mixes { offset, page },
            Err(e) => failed(Task::Mixes { offset }, &e),
        },
        ToWorker::LoadMixTracks {
            mix_id,
            offset,
            limit,
        } => match client.mix_tracks(&mix_id, offset, limit) {
            Ok(page) => FromWorker::MixTracks {
                mix_id,
                offset,
                page,
            },
            Err(e) => failed(Task::MixTracks { mix_id, offset }, &e),
        },
        // Always the first page: there is only ever one of a radio, and what
        // follows it is the radio for whatever is playing by then.
        ToWorker::LoadRadio { mix_id, limit } => match client.mix_tracks(&mix_id, 0, limit) {
            Ok(page) => FromWorker::Radio { mix_id, page },
            Err(e) => failed(Task::Radio { mix_id }, &e),
        },
        ToWorker::Search {
            query,
            offset,
            limit,
        } => match client.search_tracks(&query, offset, limit) {
            Ok(page) => FromWorker::SearchResults {
                query,
                offset,
                page,
            },
            Err(e) => failed(Task::Search { query, offset }, &e),
        },
        ToWorker::Resolve(id) => match client.resolve_stream(id, Quality::HiRes) {
            Ok(r) => FromWorker::Resolved(id, r),
            Err(e) => failed(Task::Resolve, &e),
        },
        ToWorker::SetFavorite { track_id, favorite } => {
            match client.set_favorite_track(track_id, favorite) {
                // Silence is success, and it is the only reply in this
                // loop that is. The interface changed the heart before
                // this went out, so a confirmation would tell it only
                // what it already believes; the reply worth the channel
                // is the one that says it was wrong. Recorded in the log
                // all the same, because a library that changed under the
                // user is worth a line.
                Ok(()) => {
                    log::info!("favorite {track_id} -> {favorite}");
                    return None;
                }
                Err(e) => failed(
                    Task::SetFavorite {
                        track_id,
                        wanted: favorite,
                    },
                    &e,
                ),
            }
        }
        ToWorker::CreatePlaylist { title } => created(client, &title),
        ToWorker::RenamePlaylist { uuid, title, was } => renamed(client, uuid, &title, was)?,
        ToWorker::DeletePlaylist { uuid, title } => deleted(client, uuid, &title),
        ToWorker::AddToPlaylist {
            uuid,
            title,
            track_id,
        } => added(client, &uuid, title, track_id),
        ToWorker::RemoveFromPlaylist { uuid, track_id } => removed(client, uuid, track_id),
        // The only request here that touches no network. It is still on
        // this thread because it waits on a subprocess, and the render
        // loop may not wait on anything.
        ToWorker::ReadAudioGraph => {
            let read = graph::probe();
            if let Err(e) = &read {
                log::info!("audio graph: {e}");
            }
            FromWorker::AudioGraph(read)
        }
        // Just the clock, read at track start. A background read: on failure the
        // reply is `None` and the verdict keeps its old fallback, no banner.
        // Just the clock, not the whole graph: the graph needs priel's stream to
        // be present, which on the first track it is not yet, and the verdict's
        // resample fallback needs the clock from the very first track. See
        // `graph::probe_clock`.
        ToWorker::ReadClock => FromWorker::OutputClock(graph::probe_clock()),
        // Write priel's own drop-in - never another file - into the user's
        // config directory. Filesystem work, so it is here and not on the render
        // thread, and it always answers so the overlay can move on.
        ToWorker::SetUpAudio { allowed_hz } => set_up_audio(&allowed_hz),
        // Restart the user's sound server. Waits on `systemctl`, so it too is
        // here and not on the UI thread.
        ToWorker::RestartAudio => FromWorker::AudioRestarted(setup::restart_pipewire()),
        ToWorker::SwitchBtCodec {
            device_id,
            profile_index,
        } => switch_bt(device_id, profile_index),
        // Page the queue's own source. Distinct from the view's `Load*` so a
        // fill keeps arriving after the listener has navigated elsewhere. A
        // failure sends nothing - the fill stops and the queue keeps what it
        // has - so this returns `None` on error rather than a `Failed`, which
        // would put a banner over a background job the listener never started.
        ToWorker::FillQueue {
            source,
            offset,
            limit,
        } => fill_queue(client, source, offset, limit)?,
        // A newer release than this build, if the forge has one. Silent
        // otherwise, for the reason the cover is: nothing to say when priel is
        // current or the check did not land.
        ToWorker::CheckUpdate => check_update(client)?,
        // Fetch the bytes and decode them here, on the worker: a cover is tens
        // of kilobytes, and the cap keeps a broken or hostile response off the
        // heap. Silence on every failure - no URL, a fetch that did not land, a
        // body that was not a picture - because an absent cover is a state the
        // box already draws, and an error banner over the now-playing row would
        // be louder than a missing picture is worth.
        ToWorker::FetchCover { track_id, cover_id } => {
            let bytes = cover_bytes(client, &cover_id, cache_cap)?;
            let image = crate::art::decode_jpeg(&bytes)?;
            FromWorker::Cover { track_id, image }
        }
    })
}

/// Ask the forge whether there is a newer release than this build.
///
/// `None` when there is nothing to tell the listener - already current, or the
/// check did not land - so the caller sends no reply, the same as the cover.
fn check_update(client: &Client) -> Option<FromWorker> {
    let tag = priel_core::update::newer_release(
        client,
        priel_core::update::LATEST_RELEASE_URL,
        env!("CARGO_PKG_VERSION"),
    )?;
    Some(FromWorker::UpdateAvailable(tag))
}

/// Page a queue's source for the background fill.
///
/// Split out of [`serve`] so that arm stays short. A failure returns `None` -
/// the fill stops and the queue keeps what it has - rather than a `Failed`,
/// which would put a banner over a job the listener never started.
fn fill_queue(
    client: &mut Client,
    source: QueueSource,
    offset: u32,
    limit: u32,
) -> Option<FromWorker> {
    let read = match &source {
        QueueSource::Favorites => client.favorite_tracks(offset, limit),
        QueueSource::Playlist(uuid) => client.playlist_tracks(uuid, offset, limit),
        QueueSource::Mix(mix_id) => client.mix_tracks(mix_id, offset, limit),
        QueueSource::Search(query) => client.search_tracks(query, offset, limit),
    };
    match read {
        Ok(page) => Some(FromWorker::QueueFilled {
            source,
            offset,
            page,
        }),
        Err(e) => {
            log::info!("queue fill at offset {offset} stopped: {e}");
            None
        }
    }
}

/// The square size the cover is fetched at, in pixels.
///
/// Close to what it is drawn at - `COVER_ROWS` rows is a block of a few dozen
/// cells - so the downscale averages a handful of source pixels rather than
/// resampling a large image. A little larger than the largest block, so the art
/// is sharp rather than upscaled.
const COVER_FETCH_PX: u32 = 160;

/// The most a cover fetch will read into memory.
///
/// A 160px JPEG is tens of kilobytes; this is set well above that and well
/// below anything that would matter, so a redirect to something enormous cannot
/// fill the heap. See [`Client::fetch_bytes`].
const COVER_FETCH_CAP: usize = 4 * 1024 * 1024;

/// A cover's bytes: from the on-disk cache when it is there, otherwise the
/// network - caching what it fetches, bounded by `cache_cap`.
///
/// Art is immutable per URL, so a cache hit is always the right image and never
/// needs revalidating. The cache is best-effort throughout: a machine with no
/// cache directory, or a write that fails, just falls back to fetching, exactly
/// as before the cache existed. `None` (the whole thing is `?`-chained) is the
/// same silence the caller already treats as "no cover to draw".
fn cover_bytes(client: &mut Client, cover_id: &str, cache_cap: u64) -> Option<Vec<u8>> {
    let dir = crate::cache::cover_dir();
    cached_or_fetch(dir.as_deref(), cover_id, cache_cap, || {
        let url = priel_core::cover_url(cover_id, COVER_FETCH_PX)?;
        client.fetch_bytes(&url, COVER_FETCH_CAP).ok()
    })
}

/// The cache-around-a-fetch itself, over an injected directory and fetch, so it
/// is testable without the network or a real cache directory.
///
/// A hit serves the cached bytes and never runs `fetch`; a miss runs it and
/// caches what it returns. A `None` directory (no cache on this machine) skips
/// both, fetching exactly as before the cache existed.
fn cached_or_fetch(
    dir: Option<&std::path::Path>,
    key: &str,
    cache_cap: u64,
    fetch: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    if let Some(dir) = dir
        && let Some(hit) = crate::cache::get(dir, key)
    {
        return Some(hit);
    }
    let bytes = fetch()?;
    if let Some(dir) = dir {
        let _ = crate::cache::put(dir, key, &bytes, cache_cap);
    }
    Some(bytes)
}

/// Spawn a worker with the default cache ceiling.
///
/// The one the tests use: they exercise requests, not the cover cache, so the
/// cap is left at its default rather than plumbed through every call site. Only
/// the tests need it - production goes through [`spawn`], which sets the cap -
/// so it is compiled only for them.
#[cfg(test)]
pub fn spawn_with<F>(build: F) -> Worker
where
    F: FnOnce() -> anyhow::Result<Client> + Send + 'static,
{
    spawn_with_cap(build, crate::cache::DEFAULT_CAP_BYTES)
}

/// Spawn a worker around a caller-supplied client factory, with the cover
/// cache bounded to `cache_cap` bytes.
///
/// The factory runs *on the worker thread* so a failure to authenticate becomes
/// a `FromWorker::Error` on screen rather than a panic before the UI exists.
pub fn spawn_with_cap<F>(build: F, cache_cap: u64) -> Worker
where
    F: FnOnce() -> anyhow::Result<Client> + Send + 'static,
{
    let (tx, cmd_rx) = mpsc::channel::<ToWorker>();
    let (evt_tx, rx) = mpsc::channel::<FromWorker>();

    // Named so the log can say which thread a line came from. If the OS will
    // not give us one, both channel ends drop with the closure and the app's own
    // disconnect check reports it - which is a better failure than a panic.
    let started = thread::Builder::new().name("worker".into()).spawn(move || {
        let mut client = match build() {
            Ok(c) => c,
            Err(e) => {
                log::error!("the worker could not start: {e:#}");
                let _ = evt_tx.send(failed(Task::Startup, &e));
                return;
            }
        };

        for cmd in cmd_rx {
            // A request that changed something and succeeded answers with
            // nothing: the interface already shows what it asked for, so there
            // is no reply worth the channel. Only `None` means that.
            let Some(msg) = serve(&mut client, cmd, cache_cap) else {
                continue;
            };
            // Recorded here rather than at each call site: one place covers
            // every request kind, and the app only ever sees the flattened
            // string.
            if let FromWorker::Failed { detail, .. } = &msg {
                log::warn!("{detail}");
            }
            let _ = evt_tx.send(msg);
        }
        // The app dropped its end. Normal on quit, and the only trace of a
        // worker that stopped for any other reason.
        log::info!("the worker thread is stopping");
    });
    if let Err(e) = started {
        log::error!("no thread for the worker: {e}");
    }

    Worker { tx, rx }
}

#[cfg(test)]
mod tests {
    use super::{
        Fault, FromWorker, QueueSource, Task, ToWorker, Worker, cached_or_fetch, spawn_with,
    };
    use priel_core::Client;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    /// A long-lived stub origin that answers by path, so one server can serve a
    /// whole worker session.
    fn origin() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut sock) = stream else { continue };
                let mut reader = BufReader::new(sock.try_clone().expect("clone"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                // The body is drained rather than ignored: closing a socket that
                // still holds unread bytes resets the connection, and a write
                // would lose the answer it is in the middle of reading.
                let mut length = 0usize;
                loop {
                    let mut h = String::new();
                    match reader.read_line(&mut h) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if h == "\r\n" => break,
                        Ok(_) => {
                            let lower = h.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                }
                if length > 0 {
                    let mut sent = vec![0u8; length];
                    let _ = reader.read_exact(&mut sent);
                }
                let body = if line.contains("/v1/sessions") {
                    r#"{"userId":1,"countryCode":"DE"}"#.to_string()
                } else if line.contains("/favorites/tracks") {
                    // Varied by offset so a paging test can tell one page from
                    // the next; a stub that answered the same thing twice would
                    // pass whether or not the offset reached the URL.
                    let id = if line.contains("offset=0") { 1 } else { 2 };
                    format!(r#"{{"totalNumberOfItems":2,"items":[{{"item":{{"id":{id}}}}}]}}"#)
                } else if line.contains("playbackinfo") {
                    let inner = r#"{"codecs":"flac","urls":["http://127.0.0.1:1/a"]}"#;
                    let b64 = {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD.encode(inner)
                    };
                    format!(
                        r#"{{"manifestMimeType":"application/vnd.tidal.bts","manifest":"{b64}"}}"#
                    )
                } else if line.contains("my_collection_my_mixes") {
                    // The mixes arrive inside a screen rather than a listing,
                    // and varied by offset like the rest so the paging
                    // arguments are proved to reach the URL.
                    let n = if line.contains("offset=0") { 1 } else { 2 };
                    format!(
                        r#"{{"rows":[{{"modules":[{{"type":"MIX_LIST","pagedList":{{
                          "totalNumberOfItems":2,"items":[{{"id":"m{n}","title":"Mix {n}",
                          "subTitle":"Miles Davis"}}]}}}}]}}]}}"#
                    )
                } else if line.contains("/v1/pages/mix") {
                    // A header module with no list on it comes first, exactly as
                    // the service sends it: a stub that led with the tracks
                    // would pass whether or not the header is stepped over.
                    let id = if line.contains("offset=0") { 1 } else { 2 };
                    format!(
                        r#"{{"rows":[{{"modules":[{{"type":"MIX_HEADER","mix":{{"id":"m1"}}}}]}},
                          {{"modules":[{{"type":"TRACK_LIST","pagedList":{{
                            "totalNumberOfItems":2,
                            "items":[{{"type":"track","item":{{"id":{id}}}}}]}}}}]}}]}}"#
                    )
                } else if line.contains("create-playlist") {
                    // The newer API wraps what it made, and the uuid is the one
                    // fact the interface cannot do without.
                    r#"{"data":{"uuid":"new-1","title":"Late night"}}"#.to_string()
                } else if line.contains("/v1/search") {
                    // Varied by offset for the same reason the favorites are.
                    let id = if line.contains("offset=0") { 1 } else { 2 };
                    format!(r#"{{"tracks":{{"totalNumberOfItems":2,"items":[{{"id":{id}}}]}}}}"#)
                } else if line.contains("/tracks") || line.contains("/playlists") {
                    let id = if line.contains("offset=0") { 1 } else { 2 };
                    format!(r#"{{"totalNumberOfItems":2,"items":[{{"id":{id},"uuid":"p{id}"}}]}}"#)
                } else {
                    r#"{"items":[]}"#.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 S\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        base
    }

    fn worker_on(base: String) -> Worker {
        spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c)
        })
    }

    fn next(w: &Worker) -> FromWorker {
        w.rx.recv_timeout(Duration::from_secs(5))
            .expect("the worker should answer")
    }

    #[test]
    fn a_favorites_reply_names_the_page_it_is_for() {
        // Goal: two pages of the same listing are told apart by the offset they
        // were asked for, never by the order they come back in. Without that
        // identity a slow first page would be appended after the second.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadFavorites {
            offset: 0,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Favorites { offset, page } => {
                assert_eq!(offset, 0, "the reply names the page it answers");
                assert_eq!(page.items[0].id, 1);
                assert_eq!(page.total, 2, "and carries the length of the listing");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::LoadFavorites {
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Favorites { offset, page } => {
                assert_eq!(offset, 1);
                assert_eq!(page.items[0].id, 2, "the offset has to reach the URL");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn a_queue_fill_pages_its_source_and_names_it_back() {
        // Goal: the fill goes to the queue's source, not the view, and the reply
        // carries the source and offset so the app can match it to the right
        // queue. Method: ask to fill favorites, then search, and read both back.
        let w = worker_on(origin());

        w.tx.send(ToWorker::FillQueue {
            source: QueueSource::Favorites,
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::QueueFilled {
                source,
                offset,
                page,
            } => {
                assert_eq!(source, QueueSource::Favorites);
                assert_eq!(offset, 1, "the reply names the page it answers");
                assert_eq!(page.items[0].id, 2, "the offset reached the URL");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::FillQueue {
            source: QueueSource::Search("miles".into()),
            offset: 0,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::QueueFilled { source, .. } => {
                assert_eq!(source, QueueSource::Search("miles".into()));
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn a_queue_fill_that_fails_sends_nothing_rather_than_a_banner() {
        // Goal: a background fill is not something the listener started, so a
        // failure must not put a `Failed` on the notice line. It sends nothing
        // and the queue keeps what it has. Method: point the fill at a dead port
        // and confirm the next thing the channel carries is not this fill.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });
        w.tx.send(ToWorker::FillQueue {
            source: QueueSource::Favorites,
            offset: 0,
            limit: 10,
        })
        .unwrap();
        // A following request that does answer proves the fill sent nothing: if
        // it had, this would receive the fill's reply first.
        w.tx.send(ToWorker::FillQueue {
            source: QueueSource::Favorites,
            offset: 0,
            limit: 10,
        })
        .unwrap();
        assert!(
            w.rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "a failed fill must send neither a reply nor an error"
        );
    }

    #[test]
    fn a_failed_page_says_which_page_it_was() {
        // Goal: a view waiting on a page has to recognise its own failure, or it
        // waits forever for a reply that is never coming.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });
        w.tx.send(ToWorker::LoadFavorites {
            offset: 40,
            limit: 20,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Failed { task, .. } => {
                assert_eq!(task, Task::Favorites { offset: 40 });
            }
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }

    #[test]
    fn every_request_kind_comes_back_as_its_own_reply() {
        // Goal: the worker is the only thing between the UI and the network, and
        // it must keep replies typed. A reply of the wrong variant would silently
        // populate the wrong view.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadFavorites {
            offset: 0,
            limit: 10,
        })
        .unwrap();
        assert!(matches!(next(&w), FromWorker::Favorites { .. }));

        w.tx.send(ToWorker::LoadPlaylists {
            offset: 0,
            limit: 10,
        })
        .unwrap();
        assert!(matches!(next(&w), FromWorker::Playlists { .. }));

        w.tx.send(ToWorker::LoadPlaylistTracks {
            uuid: "u".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap();
        match next(&w) {
            FromWorker::PlaylistTracks { uuid, .. } => {
                assert_eq!(uuid, "u", "the reply must name which playlist it is for");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::Search {
            query: "q".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap();
        assert!(matches!(next(&w), FromWorker::SearchResults { .. }));

        w.tx.send(ToWorker::LoadMixes {
            offset: 0,
            limit: 10,
        })
        .unwrap();
        assert!(matches!(next(&w), FromWorker::Mixes { .. }));

        w.tx.send(ToWorker::LoadMixTracks {
            mix_id: "m1".into(),
            offset: 0,
            limit: 10,
        })
        .unwrap();
        match next(&w) {
            FromWorker::MixTracks { mix_id, .. } => {
                assert_eq!(mix_id, "m1", "the reply must name which mix it is for");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::LoadRadio {
            mix_id: "m1".into(),
            limit: 10,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Radio { mix_id, .. } => {
                assert_eq!(mix_id, "m1", "the reply must name which mix it is for");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::Resolve(7)).unwrap();
        match next(&w) {
            FromWorker::Resolved(id, _) => assert_eq!(id, 7, "resolves are matched by id"),
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn a_radio_is_a_request_of_its_own_and_not_a_page_of_the_open_mix() {
        // Goal: the two go to the same endpoint and mean different things. One
        // fills the listing on screen; the other extends the play queue behind
        // it, and they can be in flight for the same mix at the same time - so
        // a reply that could stand for either would put a mix's tracks into the
        // view nobody opened, or into the queue nobody asked to extend.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadRadio {
            mix_id: "m1".into(),
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Radio { mix_id, page } => {
                assert_eq!(mix_id, "m1");
                assert_eq!(page.items[0].id, 1, "past the header module");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn a_mix_reply_names_the_page_and_the_mix_it_answers() {
        // Goal: both mix calls are served by a screen endpoint that has to be
        // unwrapped, so this is where the unwrapping is proved end to end - and
        // where the paging arguments are proved to survive the trip through it.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadMixes {
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Mixes { offset, page } => {
                assert_eq!(offset, 1, "the reply names the page it answers");
                assert_eq!(page.items[0].id, "m2", "the offset has to reach the URL");
                assert_eq!(page.items[0].subtitle, "Miles Davis");
                assert_eq!(page.total, 2, "and carries the length of the listing");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::LoadMixTracks {
            mix_id: "m1".into(),
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::MixTracks {
                mix_id,
                offset,
                page,
            } => {
                assert_eq!((mix_id.as_str(), offset), ("m1", 1));
                assert_eq!(page.items[0].id, 2, "past the header module and paged");
                assert_eq!(page.total, 2);
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn every_listing_reply_names_the_page_and_the_listing_it_answers() {
        // Goal: the offset alone is not an identity where more than one listing
        // can be open. A page of the playlist the user has just left, or of the
        // query they have just replaced, has to be recognisable as such - and
        // every listing has to report how long it is, or nothing can page.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadPlaylists {
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Playlists { offset, page } => {
                assert_eq!(offset, 1, "the reply names the page it answers");
                assert_eq!(page.items[0].uuid, "p2", "the offset has to reach the URL");
                assert_eq!(page.total, 2, "and carries the length of the listing");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::LoadPlaylistTracks {
            uuid: "abc".into(),
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::PlaylistTracks { uuid, offset, page } => {
                assert_eq!((uuid.as_str(), offset), ("abc", 1));
                assert_eq!(page.items[0].id, 2, "the offset has to reach the URL");
                assert_eq!(page.total, 2);
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::Search {
            query: "blue".into(),
            offset: 1,
            limit: 1,
        })
        .unwrap();
        match next(&w) {
            FromWorker::SearchResults {
                query,
                offset,
                page,
            } => {
                assert_eq!((query.as_str(), offset), ("blue", 1));
                assert_eq!(page.items[0].id, 2, "the offset has to reach the URL");
                assert_eq!(page.total, 2);
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    #[test]
    fn a_failed_listing_page_names_the_page_and_the_listing_too() {
        // Goal: a failure has to carry the same identity a success does, or one
        // view's dead request latches another view's paging.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });

        let expected = [
            (
                ToWorker::LoadPlaylists {
                    offset: 7,
                    limit: 1,
                },
                Task::Playlists { offset: 7 },
            ),
            (
                ToWorker::LoadPlaylistTracks {
                    uuid: "abc".into(),
                    offset: 8,
                    limit: 1,
                },
                Task::PlaylistTracks {
                    uuid: "abc".into(),
                    offset: 8,
                },
            ),
            (
                ToWorker::Search {
                    query: "blue".into(),
                    offset: 9,
                    limit: 1,
                },
                Task::Search {
                    query: "blue".into(),
                    offset: 9,
                },
            ),
        ];
        for (cmd, want) in expected {
            w.tx.send(cmd).unwrap();
            match next(&w) {
                FromWorker::Failed { task, .. } => assert_eq!(task, want),
                other => panic!("expected an error, got {}", variant(&other)),
            }
        }
    }

    #[test]
    fn a_favorite_that_was_accepted_is_answered_with_silence() {
        // Goal: the one request here that changes remote state, and the one that
        // replies only when it goes wrong. Asserted against the *next* reply
        // rather than against a timeout: the worker handles commands in order,
        // so a page asked for afterwards arriving first proves nothing was sent
        // for the write.
        let w = worker_on(origin());

        w.tx.send(ToWorker::SetFavorite {
            track_id: 7,
            favorite: true,
        })
        .unwrap();
        w.tx.send(ToWorker::LoadFavorites {
            offset: 0,
            limit: 1,
        })
        .unwrap();

        match next(&w) {
            FromWorker::Favorites { .. } => {}
            other => panic!(
                "a successful favorite should say nothing, got {}",
                variant(&other)
            ),
        }
    }

    #[test]
    fn a_favorite_that_failed_names_the_track_and_what_was_attempted() {
        // Goal: the interface filled the heart before this went out and has to
        // put exactly that one back. Correlating by identity rather than by
        // arrival order is what lets it: two tracks toggled in quick succession
        // must not undo each other.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });

        for wanted in [true, false] {
            w.tx.send(ToWorker::SetFavorite {
                track_id: 7,
                favorite: wanted,
            })
            .unwrap();
            match next(&w) {
                FromWorker::Failed { task, detail, .. } => {
                    assert_eq!(
                        task,
                        Task::SetFavorite {
                            track_id: 7,
                            wanted
                        }
                    );
                    assert!(detail.contains("favorites"), "{detail}");
                }
                other => panic!("expected an error, got {}", variant(&other)),
            }
        }
    }

    #[test]
    fn a_failed_favorite_says_which_direction_it_was_going() {
        // Goal: the notice is one line and it is the only thing the user is
        // told. "adding to favorites" and "removing from favorites" fail for
        // the same reasons and mean opposite things.
        assert_eq!(
            Task::SetFavorite {
                track_id: 1,
                wanted: true
            }
            .to_string(),
            "adding to favorites"
        );
        assert_eq!(
            Task::SetFavorite {
                track_id: 1,
                wanted: false
            }
            .to_string(),
            "removing from favorites"
        );
    }

    fn variant(m: &FromWorker) -> &'static str {
        match m {
            FromWorker::Favorites { .. } => "Favorites",
            FromWorker::Playlists { .. } => "Playlists",
            FromWorker::PlaylistTracks { .. } => "PlaylistTracks",
            FromWorker::Mixes { .. } => "Mixes",
            FromWorker::MixTracks { .. } => "MixTracks",
            FromWorker::Radio { .. } => "Radio",
            FromWorker::SearchResults { .. } => "SearchResults",
            FromWorker::Resolved(..) => "Resolved",
            FromWorker::AudioGraph(_) => "AudioGraph",
            FromWorker::OutputClock(_) => "OutputClock",
            FromWorker::AudioSetUp(_) => "AudioSetUp",
            FromWorker::AudioRestarted(_) => "AudioRestarted",
            FromWorker::BtCodecSwitched(_) => "BtCodecSwitched",
            FromWorker::PlaylistCreated(_) => "PlaylistCreated",
            FromWorker::PlaylistDeleted { .. } => "PlaylistDeleted",
            FromWorker::PlaylistTrackRemoved { .. } => "PlaylistTrackRemoved",
            FromWorker::PlaylistTrackAdded { .. } => "PlaylistTrackAdded",
            FromWorker::Cover { .. } => "Cover",
            FromWorker::UpdateAvailable(_) => "UpdateAvailable",
            FromWorker::QueueFilled { .. } => "QueueFilled",
            FromWorker::Failed { .. } => "Failed",
        }
    }

    #[test]
    fn a_failed_handshake_is_reported_instead_of_killing_the_app() {
        // Goal: an expired token must reach the screen. The worker runs the
        // build on its own thread precisely so this is a message, not a panic
        // before the UI exists.
        // Port 1 on loopback refuses at once, so this never touches the network.
        let w = worker_on("http://127.0.0.1:1".into());
        match next(&w) {
            FromWorker::Failed { detail, .. } => assert!(detail.contains("startup"), "{detail}"),
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }

    #[test]
    fn a_request_that_fails_reports_which_one_it_was() {
        // Goal: the notice line is one line, so the message has to identify the
        // failed call by itself.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            // Repoint at a dead port so the handshake succeeds and the request
            // that follows does not.
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });
        w.tx.send(ToWorker::LoadFavorites {
            offset: 0,
            limit: 10,
        })
        .unwrap();
        match next(&w) {
            FromWorker::Failed { detail, .. } => assert!(detail.contains("favorites"), "{detail}"),
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }

    #[test]
    fn dropping_the_app_side_ends_the_worker_thread() {
        // Goal: the command channel closing is the worker's shutdown signal; a
        // worker that outlived the app would hold the process open.
        let w = worker_on(origin());
        let rx = w.rx;
        drop(w.tx);
        // The loop ends, dropping the event sender, which disconnects this end.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                _ if std::time::Instant::now() > deadline => {
                    panic!("the worker thread did not exit")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn every_request_kind_reports_its_own_failure() {
        // Goal: each arm formats its own message. One arm reporting another's
        // name would send the user looking in the wrong place.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())?.with_base_url(base);
            c.connect()?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });

        for (cmd, expected) in [
            (
                ToWorker::LoadPlaylists {
                    offset: 0,
                    limit: 1,
                },
                "playlists",
            ),
            (
                ToWorker::LoadPlaylistTracks {
                    uuid: "u".into(),
                    offset: 0,
                    limit: 1,
                },
                "playlist tracks",
            ),
            (
                ToWorker::Search {
                    query: "q".into(),
                    offset: 0,
                    limit: 1,
                },
                "search",
            ),
            (ToWorker::Resolve(1), "resolve"),
        ] {
            w.tx.send(cmd).unwrap();
            match next(&w) {
                FromWorker::Failed { detail, .. } => {
                    assert!(
                        detail.contains(expected),
                        "expected {expected:?} in {detail:?}"
                    );
                }
                other => panic!("expected an error, got {}", variant(&other)),
            }
        }
    }

    #[test]
    fn a_missing_token_file_is_reported_by_name() {
        // Goal: the standard startup failure. `spawn` builds the client from a
        // path, and a user who has not logged in must be told that, not shown an
        // empty library.
        let w = super::spawn(
            "/nonexistent/priel/token.json".into(),
            "/nonexistent/priel/credentials.json".into(),
            crate::cache::DEFAULT_CAP_BYTES,
        );
        match next(&w) {
            FromWorker::Failed {
                task,
                fault,
                detail,
            } => {
                assert_eq!(task, Task::Startup, "the startup handshake is its own task");
                assert_eq!(
                    fault,
                    Fault::SignedOut,
                    "a first run and an expired session lead to the same screen"
                );
                assert!(detail.contains("not signed in"), "and say so: {detail}");
            }
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }

    #[test]
    fn a_created_playlist_comes_back_through_the_channel() {
        // Goal: the whole chain for the one edit that answers with something -
        // the app's request, the core's call, and the reply the interface needs
        // because it cannot guess the uuid. Nothing here touches an account:
        // the origin is a loopback stub.
        let w = worker_on(origin());
        w.tx.send(ToWorker::CreatePlaylist {
            title: "Late night".into(),
        })
        .expect("the worker is listening");
        match next(&w) {
            FromWorker::PlaylistCreated(made) => {
                assert_eq!(made.uuid, "new-1");
                assert_eq!(made.title, "Late night");
            }
            other => panic!("expected the new playlist, got {}", variant(&other)),
        }
    }

    #[test]
    fn a_delete_answers_and_a_rename_does_not() {
        // Goal: the two halves of the silence rule, in one place because the
        // difference between them is the point. A rename is already on screen,
        // so success says nothing; a delete is not, so it must be reported or
        // the row never leaves. The rename is proved silent by the reply that
        // arrives being the *next* request's.
        let w = worker_on(origin());
        w.tx.send(ToWorker::RenamePlaylist {
            uuid: "p1".into(),
            title: "New".into(),
            was: "Old".into(),
        })
        .expect("the worker is listening");
        w.tx.send(ToWorker::DeletePlaylist {
            uuid: "p1".into(),
            title: "Old".into(),
        })
        .expect("the worker is listening");
        match next(&w) {
            FromWorker::PlaylistDeleted { uuid } => assert_eq!(uuid, "p1"),
            other => panic!(
                "the rename should have said nothing, but {} arrived",
                variant(&other)
            ),
        }
    }

    #[test]
    fn a_track_added_to_a_playlist_is_reported_by_the_name_of_the_playlist() {
        // Goal: the notice this becomes is the only confirmation the listener
        // gets, so the title has to survive the round trip rather than being
        // looked up again from a list that may not hold it.
        let w = worker_on(origin());
        w.tx.send(ToWorker::AddToPlaylist {
            uuid: "p1".into(),
            title: "Evening".into(),
            track_id: 7,
        })
        .expect("the worker is listening");
        match next(&w) {
            FromWorker::PlaylistTrackAdded { title } => assert_eq!(title, "Evening"),
            other => panic!("expected the add to be reported, got {}", variant(&other)),
        }
    }

    #[test]
    fn a_refused_playlist_edit_names_the_action_the_user_took() {
        // Goal: the notice line reads back what was asked for, not the endpoint
        // that refused it. "deleting the playlist" is a sentence somebody can
        // act on; "playlists -> HTTP 500" is not.
        for (task, wanted) in [
            (Task::CreatePlaylist, "creating the playlist"),
            (
                Task::DeletePlaylist { uuid: "p1".into() },
                "deleting the playlist",
            ),
            (
                Task::AddToPlaylist {
                    uuid: "p1".into(),
                    track_id: 7,
                },
                "adding to the playlist",
            ),
            (
                Task::RemoveFromPlaylist {
                    uuid: "p1".into(),
                    track_id: 7,
                },
                "removing from the playlist",
            ),
            (
                Task::RenamePlaylist {
                    uuid: "p1".into(),
                    was: "Old".into(),
                },
                "renaming the playlist",
            ),
        ] {
            assert_eq!(task.to_string(), wanted);
        }
    }

    /// A fresh empty directory under the OS temp dir, unique to this process -
    /// never a real cache. Removed first so a rerun starts clean.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("priel-worker-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_cached_cover_is_served_without_fetching() {
        // Goal: the point of the cache - a cover already on disk is returned and
        // the network is never touched.
        let dir = scratch("hit");
        crate::cache::put(
            &dir,
            "cov1",
            b"cached-bytes",
            crate::cache::DEFAULT_CAP_BYTES,
        )
        .expect("seed the cache");
        let mut fetched = false;
        let out = cached_or_fetch(Some(&dir), "cov1", crate::cache::DEFAULT_CAP_BYTES, || {
            fetched = true;
            Some(b"from-the-network".to_vec())
        });
        assert_eq!(
            out.as_deref(),
            Some(&b"cached-bytes"[..]),
            "served the cache"
        );
        assert!(!fetched, "a hit must not fetch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_cover_is_fetched_and_then_cached() {
        // Goal: a miss fetches, and what it fetched is cached so the next look is
        // a hit.
        let dir = scratch("miss");
        let out = cached_or_fetch(Some(&dir), "cov2", crate::cache::DEFAULT_CAP_BYTES, || {
            Some(b"from-the-network".to_vec())
        });
        assert_eq!(out.as_deref(), Some(&b"from-the-network"[..]));
        assert_eq!(
            crate::cache::get(&dir, "cov2").as_deref(),
            Some(&b"from-the-network"[..]),
            "the fetched bytes are now cached"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_no_cache_directory_it_simply_fetches() {
        // Goal: a machine with no cache dir degrades to fetching, exactly as
        // before the cache existed - never a panic, never a skipped cover.
        let mut fetched = false;
        let out = cached_or_fetch(None, "cov3", crate::cache::DEFAULT_CAP_BYTES, || {
            fetched = true;
            Some(b"bytes".to_vec())
        });
        assert!(fetched, "with no cache there is nothing to do but fetch");
        assert_eq!(out.as_deref(), Some(&b"bytes"[..]));
    }
}
