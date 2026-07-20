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
use priel_core::{Client, Playlist, Quality, ResolvedStream, SearchResults, Track};

pub enum ToWorker {
    LoadFavorites,
    LoadPlaylists,
    LoadPlaylistTracks(String), // uuid
    Search(String),
    Resolve(u64),
}

pub enum FromWorker {
    Favorites(Vec<Track>),
    Playlists(Vec<Playlist>),
    PlaylistTracks(String, Vec<Track>), // uuid, tracks
    SearchResults(SearchResults),
    Resolved(u64, ResolvedStream),
    Error(String),
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
            Ok(creds) => Client::with_auth(&token_path, creds.into_config())
                .map_err(|e| format!("token: {e}"))?,
            Err(_) => Client::from_token_file(&token_path).map_err(|e| format!("token: {e}"))?,
        };
        client.connect().map_err(|e| format!("connect: {e}"))?;
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
    F: FnOnce() -> Result<Client, String> + Send + 'static,
{
    let (tx, cmd_rx) = mpsc::channel::<ToWorker>();
    let (evt_tx, rx) = mpsc::channel::<FromWorker>();

    thread::spawn(move || {
        let mut client = match build() {
            Ok(c) => c,
            Err(e) => {
                log::error!("the worker could not start: {e}");
                let _ = evt_tx.send(FromWorker::Error(e));
                return;
            }
        };

        for cmd in cmd_rx {
            let msg = match cmd {
                ToWorker::LoadFavorites => match client.favorite_tracks(0, 100) {
                    Ok(t) => FromWorker::Favorites(t),
                    Err(e) => FromWorker::Error(format!("favorites: {e}")),
                },
                ToWorker::LoadPlaylists => match client.user_playlists(0, 100) {
                    Ok(p) => FromWorker::Playlists(p),
                    Err(e) => FromWorker::Error(format!("playlists: {e}")),
                },
                ToWorker::LoadPlaylistTracks(uuid) => match client.playlist_tracks(&uuid, 0, 200) {
                    Ok(t) => FromWorker::PlaylistTracks(uuid, t),
                    Err(e) => FromWorker::Error(format!("playlist tracks: {e}")),
                },
                ToWorker::Search(q) => match client.search(&q, 50) {
                    Ok(r) => FromWorker::SearchResults(r),
                    Err(e) => FromWorker::Error(format!("search: {e}")),
                },
                ToWorker::Resolve(id) => match client.resolve_stream(id, Quality::HiRes) {
                    Ok(r) => FromWorker::Resolved(id, r),
                    Err(e) => FromWorker::Error(format!("resolve: {e}")),
                },
            };
            // Recorded here rather than at each call site: one place covers
            // every request kind, and the app only ever sees the flattened
            // string.
            if let FromWorker::Error(e) = &msg {
                log::warn!("{e}");
            }
            let _ = evt_tx.send(msg);
        }
        // The app dropped its end. Normal on quit, and the only trace of a
        // worker that stopped for any other reason.
        log::info!("the worker thread is stopping");
    });

    Worker { tx, rx }
}

#[cfg(test)]
mod tests {
    use super::{FromWorker, ToWorker, Worker, spawn_with};
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
                    r#"{"tracks":{"items":[]},"playlists":{"items":[]}}"#.to_string()
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
            let mut c = Client::new("tok".into())
                .map_err(|e| e.to_string())?
                .with_base_url(base);
            c.connect().map_err(|e| format!("connect: {e}"))?;
            Ok(c)
        })
    }

    fn next(w: &Worker) -> FromWorker {
        w.rx.recv_timeout(Duration::from_secs(5))
            .expect("the worker should answer")
    }

    #[test]
    fn every_request_kind_comes_back_as_its_own_reply() {
        // Goal: the worker is the only thing between the UI and the network, and
        // it must keep replies typed. A reply of the wrong variant would silently
        // populate the wrong view.
        let w = worker_on(origin());

        w.tx.send(ToWorker::LoadFavorites).unwrap();
        assert!(matches!(next(&w), FromWorker::Favorites(_)));

        w.tx.send(ToWorker::LoadPlaylists).unwrap();
        assert!(matches!(next(&w), FromWorker::Playlists(_)));

        w.tx.send(ToWorker::LoadPlaylistTracks("u".into())).unwrap();
        match next(&w) {
            FromWorker::PlaylistTracks(uuid, _) => {
                assert_eq!(uuid, "u", "the reply must name which playlist it is for");
            }
            other => panic!("wrong reply variant: {}", variant(&other)),
        }

        w.tx.send(ToWorker::Search("q".into())).unwrap();
        assert!(matches!(next(&w), FromWorker::SearchResults(_)));

        w.tx.send(ToWorker::Resolve(7)).unwrap();
        match next(&w) {
            FromWorker::Resolved(id, _) => assert_eq!(id, 7, "resolves are matched by id"),
            other => panic!("wrong reply variant: {}", variant(&other)),
        }
    }

    fn variant(m: &FromWorker) -> &'static str {
        match m {
            FromWorker::Favorites(_) => "Favorites",
            FromWorker::Playlists(_) => "Playlists",
            FromWorker::PlaylistTracks(..) => "PlaylistTracks",
            FromWorker::SearchResults(_) => "SearchResults",
            FromWorker::Resolved(..) => "Resolved",
            FromWorker::Error(_) => "Error",
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
            FromWorker::Error(e) => assert!(e.contains("connect"), "{e}"),
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }

    #[test]
    fn a_request_that_fails_reports_which_one_it_was() {
        // Goal: the notice line is one line, so the message has to identify the
        // failed call by itself.
        let base = origin();
        let w = spawn_with(move || {
            let mut c = Client::new("tok".into())
                .map_err(|e| e.to_string())?
                .with_base_url(base);
            c.connect().map_err(|e| format!("connect: {e}"))?;
            // Repoint at a dead port so the handshake succeeds and the request
            // that follows does not.
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });
        w.tx.send(ToWorker::LoadFavorites).unwrap();
        match next(&w) {
            FromWorker::Error(e) => assert!(e.contains("favorites"), "{e}"),
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
            let mut c = Client::new("tok".into())
                .map_err(|e| e.to_string())?
                .with_base_url(base);
            c.connect().map_err(|e| format!("connect: {e}"))?;
            Ok(c.with_base_url("http://127.0.0.1:1"))
        });

        for (cmd, expected) in [
            (ToWorker::LoadPlaylists, "playlists"),
            (ToWorker::LoadPlaylistTracks("u".into()), "playlist tracks"),
            (ToWorker::Search("q".into()), "search"),
            (ToWorker::Resolve(1), "resolve"),
        ] {
            w.tx.send(cmd).unwrap();
            match next(&w) {
                FromWorker::Error(e) => {
                    assert!(e.contains(expected), "expected {expected:?} in {e:?}");
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
            FromWorker::Error(e) => {
                assert!(e.starts_with("token:"), "should name the stage: {e}");
                assert!(e.contains("not signed in"), "should say how to fix it: {e}");
            }
            other => panic!("expected an error, got {}", variant(&other)),
        }
    }
}
