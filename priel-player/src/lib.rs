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

use anyhow::{Context, Result};
use priel_core::PlayableSource;

use crate::hw::HwParams;

pub mod graph;
pub mod hw;
mod run;

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
    /// Decoded sample rate (Hz) from `audio-params/samplerate`, before the
    /// output stage. Differing from `sample_rate` means something resampled.
    pub in_sample_rate: u32,
    /// Decoded sample format from `audio-params/format`. This is a *container*
    /// width, not the source's real bit depth: ffmpeg hands 24-bit FLAC back as
    /// `s32`, so it cannot be compared against the output width on its own.
    pub in_format: String,
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
    /// The audio server's own volume for our stream (`ao-volume`), when the
    /// output driver exposes it. `None` means the driver has no such concept.
    ///
    /// This is separate from [`Self::volume`], which is mpv's internal software
    /// volume. `PipeWire` attenuates in software too, so a stream turned down in
    /// the system mixer is no more bit-perfect than one turned down in priel.
    pub ao_volume: Option<f64>,
    /// The output device mpv is pointed at (`auto` unless one was chosen). This
    /// is an identifier from [`AudioDevice::name`], so the picker can mark the
    /// row that is in use by comparing the two.
    pub audio_device: String,
    /// Why the last device change did not take, if it did not.
    ///
    /// Display only, like the `detail` of a worker failure: there is nothing
    /// here for a caller to branch on, and both causes - a device that is no
    /// longer there and one that refused to open - leave the previous output in
    /// use. Cleared by the next change that is accepted.
    pub device_error: Option<String>,
    /// Live parameters of the ALSA device, when one could be read.
    ///
    /// This is the only unmediated view of the hardware. When present it decides
    /// the verdict; when absent the judgement falls back to what the audio
    /// server reported, which can hide a resample it performed itself.
    pub hw: Option<HwParams>,
    /// How the output device is being held **now**, which is not the same thing
    /// as what was asked for. See [`OutputAccess`].
    pub access: OutputAccess,
}

/// How priel is holding the output device.
///
/// Asking for a device exclusively and having it are two different facts, and
/// only the second may ever be shown: a player that quietly fell back to the
/// mixer while still claiming a direct connection would be worse than not
/// offering the path at all. So this describes what the player *achieved* -
/// what the listener asked for lives in the interface, as the thing they can
/// still change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputAccess {
    /// The ordinary path: the device is shared with everything else on the
    /// machine. Also what an idle player reports, because it is holding nothing.
    #[default]
    Shared,
    /// The device is priel's alone.
    Exclusive,
    /// Exclusive access was asked for and refused, so priel is sharing the
    /// device instead - which is the only reason this is distinct from
    /// [`Self::Shared`]. The listener asked for something they did not get and
    /// has to be told.
    Refused,
}

/// How faithfully the decoded samples are reaching the output device.
///
/// This describes **what priel hands to the audio API**, which is as far as mpv
/// can see. A server further down the graph can still resample or mix; detecting
/// that needs to inspect the `PipeWire` graph itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fidelity {
    /// Nothing is playing, or the parameters are not known yet.
    Unknown,
    /// Every sample reaches the device unaltered.
    BitPerfect,
    /// Only the level is changed: no resampling, no truncation, the sample
    /// stream otherwise intact.
    ///
    /// Digital attenuation costs roughly one bit per 6 dB, which is a trade most
    /// listeners make happily for a volume key. It is a different kind of thing
    /// from rebuilding the stream, and flattening the two into one warning makes
    /// the indicator useless for the people who care most.
    NearBitPerfect(Alteration),
    /// The sample stream itself is being rebuilt.
    Altered(Alteration),
}

/// Why playback is not bit-perfect, most damaging first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alteration {
    /// The output rate differs from the decoded rate, so samples are being
    /// interpolated. Usually a sink locked to one rate, or `allowed-rates` not
    /// listing the track's rate.
    Resampled,
    /// The output format is narrower than the source, so low bits are being
    /// discarded. A 24-bit track played out as `s16` loses eight bits.
    Truncated,
    /// priel's own software volume is scaling every sample. Only exact at 100%,
    /// and the one cause the listener can clear with a single keypress.
    VolumeScaled,
    /// The audio server is attenuating our stream. Usually software, so it costs
    /// resolution the same way - unless the sink maps volume onto a hardware
    /// mixer control on the DAC itself, which priel cannot tell from here.
    ServerVolumeScaled,
}

/// Bits of resolution an mpv sample format can carry.
///
/// `float` is 32-bit with a 24-bit mantissa, which represents any integer source
/// up to 24 bits exactly - so it counts as 24, not 32.
#[allow(
    clippy::match_same_arms,
    reason = "s24 and float coincide numerically for unrelated reasons; merging them would hide why"
)]
fn format_bits(fmt: &str) -> Option<u32> {
    // Two vocabularies reach this: mpv's (`s32`, `floatp`) and ALSA's from
    // /proc/asound (`S32_LE`, `S24_3LE`). Normalise both.
    let lower = fmt.to_ascii_lowercase();
    // `S24_3LE` is 24 bits packed into 3 bytes and has no underscore before the
    // endianness, so it has to be stripped before the plain `_le` case - which
    // is precisely the device format a packed-24 DAC reports.
    let f = lower
        .trim_end_matches("_3le")
        .trim_end_matches("_3be")
        .trim_end_matches("_le")
        .trim_end_matches("_be")
        .trim_end_matches('p');
    match f {
        "u8" | "s8" => Some(8),
        "s16" => Some(16),
        "s24" => Some(24),
        "s32" => Some(32),
        "float" => Some(24),
        "float64" | "double" => Some(53),
        _ => None,
    }
}

impl PlaybackStatus {
    /// Judge the output chain, given the source's true bit depth.
    ///
    /// `source_bits` must come from the stream metadata, not from `in_format`:
    /// the decoder reports a container width (24-bit FLAC arrives as `s32`), so
    /// comparing container widths would call a lossless `s32 -> s24` output a
    /// truncation. Pass 0 when the depth is unknown and the container width is
    /// used as an upper bound instead.
    /// The output rate and format actually in force.
    ///
    /// Prefers the ALSA readout over what the audio server told mpv, because a
    /// server that resampled on our behalf still reports the rate we asked for.
    #[must_use]
    pub fn effective_output(&self) -> (u32, &str) {
        match &self.hw {
            Some(h) => (h.rate, h.format.as_str()),
            None => (self.sample_rate, self.out_format.as_str()),
        }
    }

    /// Is the output going straight to the hardware, with no sound server in
    /// the chain at all?
    ///
    /// True of a direct ALSA card device, which is what the exclusive path
    /// uses. The distinction matters to anything that reports on the sound
    /// server's graph: there is not merely no stream of priel's in it, there is
    /// no graph between priel and the DAC to have an opinion about - which is
    /// the bit-perfect ideal rather than a failure, and reads as the opposite
    /// if the two are confused.
    #[must_use]
    pub fn bypasses_sound_server(&self) -> bool {
        hw::is_direct_card_device(&self.audio_device)
    }

    /// True when the verdict is based on the hardware rather than on what the
    /// audio server claimed.
    #[must_use]
    pub fn verdict_is_from_hardware(&self) -> bool {
        self.hw.is_some()
    }

    #[must_use]
    pub fn fidelity(&self, source_bits: u32) -> Fidelity {
        if !self.loaded || self.in_sample_rate == 0 {
            return Fidelity::Unknown;
        }
        let (out_rate, out_format) = self.effective_output();
        if out_rate == 0 {
            return Fidelity::Unknown;
        }
        let Some(out_bits) = format_bits(out_format) else {
            return Fidelity::Unknown;
        };
        let source_bits = if source_bits > 0 {
            source_bits
        } else {
            match format_bits(&self.in_format) {
                Some(b) => b,
                None => return Fidelity::Unknown,
            }
        };

        if out_rate != self.in_sample_rate {
            return Fidelity::Altered(Alteration::Resampled);
        }
        if out_bits < source_bits {
            return Fidelity::Altered(Alteration::Truncated);
        }
        // mpv's software volume multiplies every sample; only unity is exact.
        if (self.volume - 100.0).abs() > f64::EPSILON {
            return Fidelity::NearBitPerfect(Alteration::VolumeScaled);
        }
        // Then the server's. Reported second because clearing priel's own volume
        // is a keypress, while this one lives in the system mixer.
        if self
            .ao_volume
            .is_some_and(|v| (v - 100.0).abs() > f64::EPSILON)
        {
            return Fidelity::NearBitPerfect(Alteration::ServerVolumeScaled);
        }
        Fidelity::BitPerfect
    }
}

/// An audio output device the player can be pointed at.
///
/// `name` is mpv's own identifier and is exactly what `--device` accepts, so a
/// choice made in the interface can be made permanent by copying it onto the
/// command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDevice {
    /// mpv's identifier, e.g. `pipewire/alsa_output.usb-...pro-output-0`. It
    /// carries the driver before the slash; `auto` means "let mpv choose".
    pub name: String,
    /// What the audio system calls the device, for a human to recognise.
    pub description: String,
}

/// The audio devices the player can see, without starting one.
///
/// For callers that only want the list - `--list-devices` runs before there is
/// a player at all. Enumeration reads what the audio system advertises and
/// opens no output. Empty in a build without libmpv, which has nothing to ask.
#[must_use]
pub fn audio_devices() -> Vec<AudioDevice> {
    backend::audio_devices()
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
    /// Re-read the audio devices and publish them. Asked for when the picker
    /// opens, so a device plugged in mid-session shows up.
    RefreshDevices,
    /// Move the output to this device, reopening it. The queue and the position
    /// are untouched; the cost is the same short gap a sample-rate change makes.
    SetDevice(String),
    /// Ask for, or give up, exclusive use of the output device.
    SetExclusive(bool),
    Quit,
}

/// What the player thread publishes back to its owner.
///
/// Two snapshots rather than one: the status changes ten times a second and is
/// read on every render, while the device list changes only when hardware does
/// and is read only while the picker is open. Putting the devices in the status
/// would clone them on every tick for a reader that almost never wants them.
#[allow(
    dead_code,
    reason = "the stub (no-libmpv) backend has nothing to publish into either slot"
)]
pub(crate) struct Published {
    pub status: Arc<Mutex<PlaybackStatus>>,
    pub devices: Arc<Mutex<Vec<AudioDevice>>>,
}

/// How to start the player.
///
/// A struct rather than a growing argument list: everything here is optional and
/// independent, and a frontend that wants none of it can use [`Player::new`].
#[derive(Clone, Debug, Default)]
pub struct PlayerConfig {
    /// An mpv device string (e.g. `pipewire/alsa_output.usb-SMSL...pro-output-0`),
    /// `None` for the default sink, or `Some("null")` for a silent test.
    pub audio_device: Option<String>,
    /// Ask for the output device to be priel's alone, taking it out of the
    /// sound server's graph entirely.
    ///
    /// Never set by priel itself under any circumstance: exclusive access
    /// silences every other application on the machine, so it is always the
    /// listener's choice. Orthogonal to [`Self::audio_device`] - choosing a
    /// hardware device does not imply this, and a device can be opened either
    /// way. A device that will not open exclusively is reported through
    /// [`PlaybackStatus::access`] and playback continues on the shared path.
    pub exclusive: bool,
    /// How much of mpv's own log to ask for, as an mpv level name (`error`,
    /// `warn`, `info`, `v`, `debug`, `trace`), or `None` to ask for none.
    ///
    /// The messages arrive as events and are recorded through the `log` facade
    /// like everything else, so they interleave with the caller's own lines
    /// rather than living in a second file with its own clock. mpv formats every
    /// message it is asked for, so asking for more than will be recorded is
    /// wasted work.
    pub mpv_log_level: Option<String>,
}

pub struct Player {
    tx: Sender<Cmd>,
    status: Arc<Mutex<PlaybackStatus>>,
    devices: Arc<Mutex<Vec<AudioDevice>>>,
    handle: Option<JoinHandle<()>>,
}

impl Player {
    /// Create a player. `audio_device` is an mpv device string
    /// (e.g. `pipewire/alsa_output.usb-SMSL...pro-output-0`), or `None` for the
    /// default sink, or `Some("null")` for a silent test.
    ///
    /// # Errors
    /// If the player thread cannot be started. An mpv init failure is *not*
    /// reported here: it happens on that thread and is recorded in the log.
    pub fn new(audio_device: Option<String>) -> Result<Self> {
        Self::with_config(PlayerConfig {
            audio_device,
            ..PlayerConfig::default()
        })
    }

    /// Create a player from a full configuration.
    ///
    /// # Errors
    /// If the player thread cannot be started.
    pub fn with_config(config: PlayerConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(PlaybackStatus::default()));
        let devices = Arc::new(Mutex::new(Vec::new()));
        let published = Published {
            status: status.clone(),
            devices: devices.clone(),
        };
        let handle = backend::spawn(config, rx, published).context("starting the player thread")?;
        Ok(Self {
            tx,
            status,
            devices,
            handle: Some(handle),
        })
    }

    /// Start playing a track now (replaces the playlist).
    pub fn play_now(&self, id: u64, source: PlayableSource) {
        self.send(Cmd::Load(id, source));
    }
    /// Preload the next track (appended, for gapless transition).
    pub fn append_next(&self, id: u64, source: PlayableSource) {
        self.send(Cmd::Append(id, source));
    }
    /// Skip to the preloaded next entry.
    pub fn skip_next(&self) {
        self.send(Cmd::Next);
    }
    pub fn toggle_pause(&self) {
        self.send(Cmd::TogglePause);
    }
    pub fn seek(&self, seconds: f64) {
        self.send(Cmd::Seek(seconds));
    }
    pub fn seek_relative(&self, delta: f64) {
        self.send(Cmd::SeekRelative(delta));
    }
    pub fn set_volume(&self, vol: f64) {
        self.send(Cmd::SetVolume(vol.clamp(0.0, 130.0)));
    }
    pub fn stop(&self) {
        self.send(Cmd::Stop);
    }
    /// Ask the player thread to re-read the audio devices.
    ///
    /// Fire-and-forget like every other command: the answer appears in
    /// [`Self::devices`] a tick later rather than being waited for here, because
    /// enumerating asks the audio system and the caller may be a UI thread.
    pub fn refresh_devices(&self) {
        self.send(Cmd::RefreshDevices);
    }

    /// Move the output to `device`, named as [`AudioDevice::name`] spells it.
    ///
    /// The output is reopened, which costs the same short gap a sample-rate
    /// change already does; nothing else about the queue changes. A device that
    /// is gone or will not open leaves the previous one in use and reports
    /// itself through [`PlaybackStatus::device_error`] - and the track it cost
    /// is loaded again, from the start, because the player abandons a file
    /// whose output fails to open.
    ///
    /// The choice lasts for this session. priel reads no configuration file, so
    /// `--device` is what makes one permanent.
    pub fn set_device(&self, device: &str) {
        self.send(Cmd::SetDevice(device.to_string()));
    }

    /// Ask for the output device to be priel's alone, or give it back.
    ///
    /// The output is reopened either way, which costs the same short gap a
    /// device change does. A device that will not open exclusively is not left
    /// silent: priel goes back to sharing it, records why, and reports
    /// [`OutputAccess::Refused`] rather than claiming a connection it does not
    /// have.
    ///
    /// Like the device, the choice lasts for this session; `--exclusive` is
    /// what makes it permanent.
    pub fn set_exclusive(&self, exclusive: bool) {
        self.send(Cmd::SetExclusive(exclusive));
    }

    /// The audio devices the player last reported.
    ///
    /// Empty until the first [`Self::refresh_devices`] has been answered, and in
    /// a build without libmpv.
    #[must_use]
    pub fn devices(&self) -> Vec<AudioDevice> {
        // Poison-tolerant for the same reason as `status`: a plain snapshot with
        // no cross-field invariant, and the UI must keep rendering regardless.
        self.devices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Post a command to the player thread.
    ///
    /// Fire-and-forget by design - the UI must never wait on the player - so the
    /// only failure is a thread that is no longer there, in which case every
    /// command from here on is a no-op and the interface looks frozen. Say so
    /// once per command rather than letting it happen in silence.
    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_err() {
            log::error!("the player thread is gone; this command was dropped");
        }
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

/// The message inside a joined thread's panic payload.
///
/// `join` hands back a `Box<dyn Any>`, which holds a `&'static str` for a plain
/// `panic!("...")` and a `String` for a formatted one. Anything else carries no
/// readable message.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        // The join result carries the panic payload, and dropping it is how a
        // dead player thread stayed invisible. Release builds abort on panic, so
        // this only ever reports in a development build - which is exactly where
        // it is read.
        if let Some(h) = self.handle.take()
            && let Err(payload) = h.join()
        {
            log::error!("the player thread panicked: {}", panic_text(&*payload));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::HwParams;
    use std::time::{Duration, Instant};

    /// A player wired to mpv's null output: real command handling, no audio
    /// device, so it behaves identically on a headless machine.
    fn silent() -> Player {
        Player::new(Some("null".into())).expect("player")
    }

    #[test]
    fn a_panic_payload_is_read_back_whatever_shape_it_was_thrown_in() {
        // Goal: a player thread that panicked is reported by what it said, and
        // `join` hands the message back in one of two shapes depending on
        // whether the panic was formatted. Reading only one of them would report
        // half the panics as blank.
        assert_eq!(panic_text(&"a literal"), "a literal");
        assert_eq!(
            panic_text(&String::from("a formatted one")),
            "a formatted one"
        );
        assert_eq!(panic_text(&7u8), "no message", "an odd payload still reads");
    }

    fn wait_for(p: &Player, cond: impl Fn(&PlaybackStatus) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if cond(&p.status()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_new_player_starts_idle() {
        // Goal: the UI reads this snapshot before anything is loaded, and the
        // end-of-track fallback keys off `ended`, so a fresh player must not
        // look like a finished one.
        let p = silent();
        let s = p.status();
        assert!(!s.loaded);
        assert!(!s.playing);
        assert!(!s.ended);
        assert_eq!(s.current_id, 0);
    }

    #[test]
    fn commands_are_non_blocking_and_reach_the_thread() {
        // Goal: every command is fire-and-forget by design - the UI thread must
        // never wait on the player. Volume is the one with a value we can watch
        // come back through the shared snapshot.
        let p = silent();
        let before = Instant::now();
        p.set_volume(55.0);
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "sending must not block the caller"
        );
        assert!(
            wait_for(&p, |s| (s.volume - 55.0).abs() < f64::EPSILON),
            "the player thread should have applied it"
        );
    }

    #[test]
    fn the_device_list_is_published_rather_than_waited_for() {
        // Goal: the picker opens on the UI thread, which may not block, so the
        // devices arrive the same way the playback status does - asked for with
        // a fire-and-forget command and read back from a snapshot. Nothing is
        // enumerated until something asks, because it questions every audio
        // driver on the machine.
        let p = silent();
        assert!(
            p.devices().is_empty(),
            "a player that was never asked has nothing to report"
        );
        let before = Instant::now();
        p.refresh_devices();
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "asking must not block the caller"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && p.devices().is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let devices = p.devices();
        assert!(
            devices.iter().any(|d| d.name == "auto"),
            "the player thread should have published the list: {devices:?}"
        );
    }

    #[test]
    fn a_player_nobody_asked_reports_a_shared_device() {
        // Goal: the indicator may never claim a connection priel does not have,
        // and the default is the ordinary shared path. An idle player holds
        // nothing at all, which reads the same way.
        let p = silent();
        assert_eq!(p.status().access, OutputAccess::Shared);
        assert_eq!(
            PlaybackStatus::default().access,
            OutputAccess::Shared,
            "the default must be the modest answer, not the flattering one"
        );
    }

    #[test]
    fn a_direct_device_is_known_to_have_no_sound_server_behind_it() {
        // Goal: the graph overlay has nothing to show on the direct path, and
        // the reason is the ideal rather than a failure - priel holds the card
        // itself, so there is no graph between it and the DAC. The player knows
        // which it is; the interface must not have to guess.
        let mut s = PlaybackStatus {
            audio_device: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            ..PlaybackStatus::default()
        };
        assert!(s.bypasses_sound_server());

        s.audio_device = "pipewire/alsa_output.usb-x".into();
        assert!(!s.bypasses_sound_server());

        s.audio_device = "alsa/pipewire".into();
        assert!(
            !s.bypasses_sound_server(),
            "the server's own ALSA device is still the server"
        );

        assert!(!PlaybackStatus::default().bypasses_sound_server());
    }

    #[test]
    fn asking_for_the_device_exclusively_does_not_block_the_caller() {
        // Goal: this is reachable from the picker, which is drawn on the UI
        // thread, so it has to be fire-and-forget like every other command. The
        // answer comes back through the status a tick later.
        let p = silent();
        let before = Instant::now();
        p.set_exclusive(true);
        assert!(
            before.elapsed() < Duration::from_millis(50),
            "asking must not block the caller"
        );
        p.set_exclusive(false);
        assert!(wait_for(&p, |_| true), "the thread is still answering");
    }

    #[test]
    fn volume_is_clamped_to_a_sane_range() {
        // Goal: mpv accepts absurd values and will happily distort; the clamp is
        // the only thing between a scroll wheel and blown ears.
        let p = silent();
        p.set_volume(1_000.0);
        assert!(
            wait_for(&p, |s| s.volume <= 130.0),
            "must not exceed the ceiling"
        );
        p.set_volume(-50.0);
        assert!(wait_for(&p, |s| s.volume >= 0.0), "must not go negative");
    }

    #[test]
    fn every_command_is_accepted_without_a_track_loaded() {
        // Goal: the UI does not gate its controls on playback state, so pressing
        // skip or seek on an empty queue must be harmless rather than a panic.
        let p = silent();
        p.play_now(1, PlayableSource::Direct("http://127.0.0.1:1/a".into()));
        p.append_next(
            2,
            PlayableSource::Segments(vec!["http://127.0.0.1:1/b".into()]),
        );
        p.skip_next();
        p.toggle_pause();
        p.seek(10.0);
        p.seek_relative(-5.0);
        p.stop();
        assert!(wait_for(&p, |_| true), "the thread is still answering");
    }

    #[test]
    fn dropping_a_player_stops_its_thread() {
        // Goal: `Drop` sends Quit and joins. If it did not, quitting priel would
        // leave mpv holding the audio device open.
        let started = Instant::now();
        drop(silent());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "drop must not hang waiting for the player thread"
        );
    }

    #[test]
    fn an_idle_player_keeps_ticking_without_commands() {
        // Goal: with nothing playing the thread waits on a longer timeout and
        // then loops. If the timeout arm were wrong the status would freeze at
        // whatever it was when the last command arrived.
        let p = silent();
        std::thread::sleep(Duration::from_millis(700));
        p.set_volume(33.0);
        assert!(
            wait_for(&p, |s| (s.volume - 33.0).abs() < f64::EPSILON),
            "the thread should still be responsive after idling"
        );
    }

    /// A status as it looks mid-playback; tests override the fields they care
    /// about.
    fn playing(in_rate: u32, out_rate: u32, in_fmt: &str, out_fmt: &str) -> PlaybackStatus {
        PlaybackStatus {
            loaded: true,
            playing: true,
            volume: 100.0,
            sample_rate: out_rate,
            out_format: out_fmt.into(),
            in_sample_rate: in_rate,
            in_format: in_fmt.into(),
            ..PlaybackStatus::default()
        }
    }

    #[test]
    fn a_wider_output_container_is_still_bit_perfect() {
        // Goal: 24-bit content played out as s32 is the *normal* case on USB
        // DACs - the samples sit in the high bits and nothing is altered.
        // Reporting that as a downgrade would cry wolf on almost every track.
        let s = playing(192_000, 192_000, "s32", "s32");
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);

        // And 24-bit content in a 24-bit container likewise.
        let s = playing(96_000, 96_000, "s32", "s24");
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);
    }

    #[test]
    fn a_narrower_output_truncates_and_must_be_reported() {
        // Goal: the case worth warning about. 24-bit content leaving as s16
        // discards eight bits, which is exactly what a listener paying for
        // hi-res does not want happening silently.
        let s = playing(44_100, 44_100, "s32", "s16");
        assert_eq!(s.fidelity(24), Fidelity::Altered(Alteration::Truncated));
    }

    #[test]
    fn sixteen_bit_content_at_sixteen_bits_is_bit_perfect() {
        // Goal: the same s16 output is correct for 16-bit content. The judgement
        // is about the source, not about the number being small.
        let s = playing(44_100, 44_100, "s16", "s16");
        assert_eq!(s.fidelity(16), Fidelity::BitPerfect);
    }

    #[test]
    fn a_rate_change_between_decode_and_output_is_resampling() {
        // Goal: the most damaging failure and the one a locked sink causes.
        // 44.1k content forced out at 48k is interpolated end to end.
        let s = playing(44_100, 48_000, "s32", "s32");
        assert_eq!(s.fidelity(24), Fidelity::Altered(Alteration::Resampled));
    }

    #[test]
    fn resampling_is_reported_ahead_of_truncation() {
        // Goal: when both are wrong, name the one that does more damage.
        let s = playing(44_100, 48_000, "s32", "s16");
        assert_eq!(s.fidelity(24), Fidelity::Altered(Alteration::Resampled));
    }

    #[test]
    fn software_volume_below_unity_breaks_the_chain() {
        // Goal: mpv's volume multiplies every sample. This is the one cause the
        // listener controls, so it has to be distinguishable from the others.
        let mut s = playing(96_000, 96_000, "s32", "s32");
        s.volume = 80.0;
        assert_eq!(
            s.fidelity(24),
            Fidelity::NearBitPerfect(Alteration::VolumeScaled),
            "a level change is graded, not lumped in with a rebuilt stream"
        );
        s.volume = 100.0;
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);
    }

    #[test]
    fn float_output_carries_twenty_four_bits_exactly() {
        // Goal: 32-bit float has a 24-bit mantissa, so it represents any 24-bit
        // integer source exactly. Counting it as 32 bits would be flattering.
        let s = playing(96_000, 96_000, "s32", "float");
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);
    }

    #[test]
    fn nothing_is_claimed_before_the_parameters_are_known() {
        // Goal: an unknown chain must read as unknown, never as bit-perfect. A
        // false green light is worse than no light.
        assert_eq!(PlaybackStatus::default().fidelity(24), Fidelity::Unknown);

        let mut s = playing(96_000, 96_000, "s32", "s32");
        s.loaded = false;
        assert_eq!(s.fidelity(24), Fidelity::Unknown);

        let mut s = playing(96_000, 96_000, "s32", "");
        s.out_format = "wat".into();
        assert_eq!(s.fidelity(24), Fidelity::Unknown);
    }

    #[test]
    fn an_unknown_source_depth_falls_back_to_the_container_width() {
        // Goal: before the manifest metadata arrives the decoded container is
        // the only bound available. It is conservative, not wrong.
        let s = playing(44_100, 44_100, "s32", "s16");
        assert_eq!(s.fidelity(0), Fidelity::Altered(Alteration::Truncated));

        let s = playing(44_100, 44_100, "s16", "s16");
        assert_eq!(s.fidelity(0), Fidelity::BitPerfect);
    }

    #[test]
    fn the_audio_servers_own_volume_also_breaks_the_chain() {
        // Goal: PipeWire attenuates in software just as mpv does, so a stream
        // turned down in the system mixer is not bit-perfect either - and priel
        // being at unity does not make it so.
        let mut s = playing(96_000, 96_000, "s32", "s32");
        s.ao_volume = Some(60.0);
        assert_eq!(
            s.fidelity(24),
            Fidelity::NearBitPerfect(Alteration::ServerVolumeScaled)
        );

        s.ao_volume = Some(100.0);
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);
    }

    #[test]
    fn an_output_with_no_volume_concept_is_not_a_fault() {
        // Goal: `ao-volume` is absent on drivers that do not expose one. Absent
        // must not read as "turned down".
        let mut s = playing(96_000, 96_000, "s32", "s32");
        s.ao_volume = None;
        assert_eq!(s.fidelity(24), Fidelity::BitPerfect);
    }

    #[test]
    fn priels_own_volume_is_reported_before_the_servers() {
        // Goal: when both are down, name the one the listener can clear with a
        // keypress first.
        let mut s = playing(96_000, 96_000, "s32", "s32");
        s.volume = 50.0;
        s.ao_volume = Some(50.0);
        assert_eq!(
            s.fidelity(24),
            Fidelity::NearBitPerfect(Alteration::VolumeScaled)
        );
    }

    #[test]
    fn the_hardware_readout_overrules_what_the_server_claimed() {
        // Goal: the whole point of reading /proc/asound. PipeWire accepts a
        // 44.1 kHz stream, tells mpv it got 44.1 kHz, and clocks the card at
        // 48 kHz. Trusting mpv alone would show a green light on a resample.
        let mut s = playing(44_100, 44_100, "s32", "s32");
        assert_eq!(
            s.fidelity(24),
            Fidelity::BitPerfect,
            "with no hardware readout there is nothing to contradict the server"
        );

        s.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        assert_eq!(
            s.fidelity(24),
            Fidelity::Altered(Alteration::Resampled),
            "the device is clocked elsewhere, so this is a resample"
        );
    }

    #[test]
    fn alsa_format_names_are_understood_too() {
        // Goal: /proc speaks `S24_3LE`, mpv speaks `s24`. Both must map to the
        // same width or the hardware readout would read as unknown.
        let mut s = playing(96_000, 96_000, "s32", "s32");
        for (fmt, verdict) in [
            ("S32_LE", Fidelity::BitPerfect),
            ("S24_3LE", Fidelity::BitPerfect),
            ("FLOAT_LE", Fidelity::BitPerfect),
            ("S16_LE", Fidelity::Altered(Alteration::Truncated)),
        ] {
            s.hw = Some(HwParams {
                card: "AUDIO".into(),
                rate: 96_000,
                format: fmt.into(),
                channels: 2,
            });
            assert_eq!(s.fidelity(24), verdict, "format {fmt}");
        }
    }

    #[test]
    fn the_source_of_the_verdict_is_reported() {
        // Goal: the badge says `DAC` or `OUT` based on this, and claiming the
        // hardware when the reading came from the server would be the exact
        // overstatement this work removed.
        let mut s = playing(96_000, 96_000, "s32", "s32");
        assert!(!s.verdict_is_from_hardware());
        assert_eq!(s.effective_output(), (96_000, "s32"));

        s.hw = Some(HwParams {
            card: "AUDIO".into(),
            rate: 96_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        assert!(s.verdict_is_from_hardware());
        assert_eq!(s.effective_output(), (96_000, "S32_LE"));
    }
}
