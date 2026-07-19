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

//! priel-player — a small, thread-owned player over libmpv.
//!
//! The libmpv handle + custom `prielseg://` protocol live entirely on one
//! background thread (avoids self-referential borrows and keeps the handle off
//! the UI thread). The `Player` you hold is just a command sender plus a shared
//! status snapshot the thread refreshes ~10x/s. Callers issue non-blocking
//! commands and read `status()`; nothing here blocks the UI.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use anyhow::Result;
use priel_core::PlayableSource;

#[cfg(feature = "libmpv")]
mod backend_mpv;
#[cfg(feature = "libmpv")]
use backend_mpv as backend;

#[cfg(not(feature = "libmpv"))]
mod backend_stub;
#[cfg(not(feature = "libmpv"))]
use backend_stub as backend;

/// Snapshot of playback state, refreshed by the player thread.
#[derive(Clone, Debug, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flag snapshot of mpv's state, not a state machine"
)]
pub struct PlaybackStatus {
    pub loaded: bool,
    pub playing: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    /// DAC / output sample rate (Hz) from `audio-out-params/samplerate`.
    pub sample_rate: u32,
    /// DAC / output sample format from `audio-out-params/format` (e.g. "s32").
    pub out_format: String,
    /// Decoded codec name (e.g. "flac").
    pub codec: String,
    /// Decoded bitrate in bits/s (0 if unknown).
    pub bitrate: i64,
    pub ended: bool,
    pub volume: f64,
    /// Track id of the currently-playing playlist entry (0 if none). Changes on
    /// a gapless transition — the app watches this to update the UI + preload.
    pub current_id: u64,
    /// A preloaded next entry exists in the mpv playlist (queued, NOT necessarily
    /// buffered — the app derives "buffering" from whether audio is flowing).
    pub has_next: bool,
    /// Seconds of decoded audio buffered ahead (`demuxer-cache-duration`).
    pub cache_secs: f64,
}

/// Commands sent to the player thread.
#[allow(dead_code)] // fields go unread in the stub (no-libmpv) backend
pub(crate) enum Cmd {
    /// Start fresh: replace the playlist with this track (id + source).
    Load(u64, PlayableSource),
    /// Preload the next track by appending it to the playlist (for gapless).
    Append(u64, PlayableSource),
    /// Skip to the preloaded next entry.
    Next,
    TogglePause,
    Seek(f64),         // absolute seconds
    SeekRelative(f64), // +/- seconds
    SetVolume(f64),
    Stop,
    Quit,
}

pub struct Player {
    tx: Sender<Cmd>,
    status: Arc<Mutex<PlaybackStatus>>,
    handle: Option<JoinHandle<()>>,
}

impl Player {
    /// Create a player. `audio_device` is an mpv device string
    /// (e.g. `pipewire/alsa_output.usb-SMSL...pro-output-0`), or `None` for the
    /// default sink, or `Some("null")` for a silent test.
    ///
    /// # Errors
    /// Reserved. Both backends start their thread infallibly and report an mpv
    /// init failure on that thread, so this does not fail today; the signature
    /// stays fallible so propagating that failure later is not a breaking change.
    pub fn new(audio_device: Option<String>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let handle = backend::spawn(audio_device, rx, status.clone());
        Ok(Self {
            tx,
            status,
            handle: Some(handle),
        })
    }

    /// Start playing a track now (replaces the playlist).
    pub fn play_now(&self, id: u64, source: PlayableSource) {
        let _ = self.tx.send(Cmd::Load(id, source));
    }
    /// Preload the next track (appended, for gapless transition).
    pub fn append_next(&self, id: u64, source: PlayableSource) {
        let _ = self.tx.send(Cmd::Append(id, source));
    }
    /// Skip to the preloaded next entry.
    pub fn skip_next(&self) {
        let _ = self.tx.send(Cmd::Next);
    }
    pub fn toggle_pause(&self) {
        let _ = self.tx.send(Cmd::TogglePause);
    }
    pub fn seek(&self, seconds: f64) {
        let _ = self.tx.send(Cmd::Seek(seconds));
    }
    pub fn seek_relative(&self, delta: f64) {
        let _ = self.tx.send(Cmd::SeekRelative(delta));
    }
    pub fn set_volume(&self, vol: f64) {
        let _ = self.tx.send(Cmd::SetVolume(vol.clamp(0.0, 130.0)));
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn status(&self) -> PlaybackStatus {
        // Tolerate poisoning: this is a plain snapshot with no cross-field
        // invariant, so a producer that panicked mid-update leaves nothing to
        // corrupt, and the UI must keep rendering rather than panic in turn.
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
