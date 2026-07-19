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

pub fn spawn(token_path: String) -> Worker {
    let (tx, cmd_rx) = mpsc::channel::<ToWorker>();
    let (evt_tx, rx) = mpsc::channel::<FromWorker>();

    thread::spawn(move || {
        let mut client = match Client::from_token_file(&token_path) {
            Ok(c) => c,
            Err(e) => {
                let _ = evt_tx.send(FromWorker::Error(format!("token: {e}")));
                return;
            }
        };
        if let Err(e) = client.connect() {
            let _ = evt_tx.send(FromWorker::Error(format!("connect: {e}")));
            return;
        }

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
            let _ = evt_tx.send(msg);
        }
    });

    Worker { tx, rx }
}
