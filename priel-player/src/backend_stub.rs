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

//! No-libmpv fallback: keeps the API working for UI-only builds
//! (`--no-default-features`). Playback commands are no-ops.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{Cmd, PlaybackStatus, PlayerConfig};

/// Start the (no-op) player thread.
///
/// # Errors
///
/// If the OS will not give us a thread. Matches the libmpv backend, which names
/// its thread for the log and so cannot spawn infallibly.
pub fn spawn(
    _config: PlayerConfig,
    rx: Receiver<Cmd>,
    _status: Arc<Mutex<PlaybackStatus>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("player".into()).spawn(move || {
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                _ => {} // ignore everything else; built without libmpv
            }
        }
    })
}
