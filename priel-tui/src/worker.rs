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
use priel_core::{Client, Fault, Page, Playlist, Quality, ResolvedStream, Track};
use priel_player::graph::{self, AudioGraph, GraphError};

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

/// Rows one search request asks for. The smallest of the four: a searcher looks
/// at the head of the results, and a wrong query should cost the least.
pub const SEARCH_PAGE: u32 = 50;

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
    Search {
        query: String,
        offset: u32,
        limit: u32,
    },
    Resolve(u64),
    /// Read the chain to the output device. Runs `pw-dump` and waits for it,
    /// which is why it is here and not on the UI thread.
    ReadAudioGraph,
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
    Favorites { offset: u32 },
    Playlists { offset: u32 },
    PlaylistTracks { uuid: String, offset: u32 },
    Search { query: String, offset: u32 },
    Resolve,
}

impl std::fmt::Display for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Startup => "startup",
            Self::Favorites { .. } => "favorites",
            Self::Playlists { .. } => "playlists",
            Self::PlaylistTracks { .. } => "playlist tracks",
            Self::Search { .. } => "search",
            Self::Resolve => "resolve",
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
    SearchResults {
        query: String,
        offset: u32,
        page: Page<Track>,
    },
    Resolved(u64, ResolvedStream),
    /// The chain to the output device, or the reason there is none to show.
    ///
    /// The failure travels as `GraphError` rather than `Failed`: nothing about
    /// it is a request that went wrong, and the overlay has its own sentence
    /// for each case.
    AudioGraph(Result<AudioGraph, GraphError>),
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
pub fn spawn(token_path: String, credentials_path: String) -> Worker {
    spawn_with(move || {
        // With credentials configured the client renews its own session, which
        // is what stops the access token expiring mid-listen. Without them it
        // still works from the stored token, until that token runs out.
        let mut client = match Credentials::load(&credentials_path) {
            Ok(creds) => Client::with_auth(&token_path, creds.into_config())?,
            Err(_) => Client::from_token_file(&token_path)?,
        };
        client.connect()?;
        Ok(client)
    })
}

/// Spawn a worker around a caller-supplied client factory.
///
/// The factory runs *on the worker thread* so a failure to authenticate becomes
/// a `FromWorker::Error` on screen rather than a panic before the UI exists.
/// Tests use this to point the worker at a stub origin with no token file.
pub fn spawn_with<F>(build: F) -> Worker
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
            let msg = match cmd {
                ToWorker::LoadFavorites { offset, limit } => {
                    match client.favorite_tracks(offset, limit) {
                        Ok(page) => FromWorker::Favorites { offset, page },
                        Err(e) => failed(Task::Favorites { offset }, &e),
                    }
                }
                ToWorker::LoadPlaylists { offset, limit } => {
                    match client.user_playlists(offset, limit) {
                        Ok(page) => FromWorker::Playlists { offset, page },
                        Err(e) => failed(Task::Playlists { offset }, &e),
                    }
                }
                ToWorker::LoadPlaylistTracks {
                    uuid,
                    offset,
                    limit,
                } => match client.playlist_tracks(&uuid, offset, limit) {
                    Ok(page) => FromWorker::PlaylistTracks { uuid, offset, page },
                    Err(e) => failed(Task::PlaylistTracks { uuid, offset }, &e),
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
    use super::{Fault, FromWorker, Task, ToWorker, Worker, spawn_with};
    use priel_core::Client;
    use std::io::{BufRead, BufReader, Write};
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
                loop {
                    let mut h = String::new();
                    match reader.read_line(&mut h) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if h == "\r\n" => break,
                        Ok(_) => {}
                    }
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

        w.tx.send(ToWorker::Resolve(7)).unwrap();
        match next(&w) {
            FromWorker::Resolved(id, _) => assert_eq!(id, 7, "resolves are matched by id"),
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

    fn variant(m: &FromWorker) -> &'static str {
        match m {
            FromWorker::Favorites { .. } => "Favorites",
            FromWorker::Playlists { .. } => "Playlists",
            FromWorker::PlaylistTracks { .. } => "PlaylistTracks",
            FromWorker::SearchResults { .. } => "SearchResults",
            FromWorker::Resolved(..) => "Resolved",
            FromWorker::AudioGraph(_) => "AudioGraph",
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
}
