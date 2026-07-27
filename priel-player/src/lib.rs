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

use crate::graph::{SinkStage, SinkVolume, SourceFormat};
use crate::hw::HwParams;

pub mod graph;
pub mod hw;
mod run;
pub mod setup;

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
    /// The sound server's graph clock, read by the frontend at track start.
    ///
    /// Set from outside via [`Player::set_clock`], because reading it needs a
    /// `pw-dump` the player thread must not run. It is a *better fallback* than
    /// [`Self::sample_rate`] where there is no ALSA readout ([`Self::hw`] is
    /// `None`) - a Bluetooth or network sink - because mpv reports only the rate
    /// it handed the server, not the rate the server clocked the graph at, which
    /// is where a resample it performed on our behalf would otherwise hide. See
    /// [`Self::effective_output`].
    pub clock: Option<crate::graph::ClockRates>,
    /// The rate the sink node at the end of the graph negotiated, when it
    /// published one.
    ///
    /// Threaded from the graph like [`Self::clock`], via
    /// [`Player::set_sink_rate`], and it **outranks the clock**: see
    /// [`Self::effective_output`]. Held across tracks rather than cleared with
    /// each one, exactly as [`Self::bt_codec`] is - the reading comes from the
    /// same graph reads, a link's negotiated rate does not usually move between
    /// tracks, and dropping it would put the weaker reading back in charge for
    /// the seconds until the next read.
    pub sink_rate_hz: Option<u32>,
    /// The Bluetooth A2DP codec of the current output, or `None` when the output
    /// is not a Bluetooth link.
    ///
    /// Threaded from the graph like [`Self::clock`], via [`Player::set_bt_codec`]:
    /// the frontend reads it off the sink node (`api.bluez5.codec`), which the
    /// player thread cannot. `Some` means the link re-encodes every sample, so
    /// [`Self::fidelity`] grades it [`Fidelity::Bluetooth`] whatever the rate.
    pub bt_codec: Option<String>,
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
    /// The output is a Bluetooth A2DP link, which re-encodes every sample, so
    /// nothing downstream is bit-perfect whatever the rate or depth.
    ///
    /// A grade of its own rather than an [`Altered`](Self::Altered) cause: the
    /// loss is inherent to the link, not something the graph rebuilt and could
    /// be asked to account for, and it is not a change a listener can undo from
    /// here. The codec name is carried in [`PlaybackStatus::bt_codec`] and named
    /// at the UI, so this stays a unit variant and [`Fidelity`] stays `Copy`.
    Bluetooth,
}

impl Fidelity {
    /// The rebuilt sample stream the graph could be asked to account for, if
    /// there is one.
    ///
    /// Only [`Altered`](Self::Altered) qualifies, and only ever with a cause the
    /// graph publishes: a volume control is not a node on the path, so handing
    /// one to [`AudioGraph::attribute`](graph::AudioGraph::attribute) would ask
    /// the chain to explain something it never shows.
    #[must_use]
    pub fn alteration(self) -> Option<Alteration> {
        match self {
            Self::Altered(a @ (Alteration::Resampled | Alteration::Truncated)) => Some(a),
            _ => None,
        }
    }
}

/// A grade, and how much of the chain it was reached from.
///
/// The two travel together because neither is usable alone. A grade with no
/// account of what it rests on is the overstatement this indicator exists to
/// avoid - today's tick already means "as far as I looked" and nothing says so -
/// and an account with no grade is a diagnostic nobody asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub fidelity: Fidelity,
    pub evidence: Evidence,
}

/// How much of the chain a [`Verdict`] was reached from.
///
/// **A stage that cannot exist counts as fully evidenced.** The direct path has
/// no server stage and no sink stage at all, and marking it would put a
/// permanent question mark on the cleanest chain the player can produce - which
/// would teach the reader to ignore the mark everywhere it does mean something.
/// It is the same distinction [`ClockRates`](graph::ClockRates) already draws
/// between a permitted-rate list that is empty and one that could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// Every stage that exists here was read.
    Complete,
    /// At least one stage exists and could not be read. The grade still stands -
    /// it is what everything that *was* read says - and says so.
    Partial,
}

impl Verdict {
    /// Does this grade have to say what it rests on?
    ///
    /// Only where the missing evidence could have changed the answer, which is
    /// the clean tick and nothing else: every other grade is already at least
    /// as bad as an unread volume stage could have made it, so marking those
    /// would cost a column and say nothing.
    #[must_use]
    pub fn needs_qualifying(&self) -> bool {
        self.evidence == Evidence::Partial && self.fidelity == Fidelity::BitPerfect
    }
}

/// The sound server's volume for priel's own stream.
///
/// The middle of the three stages, and the one mpv can answer for. Three arms
/// for the same reason [`SinkVolume`] has three: mpv reports no `ao-volume`
/// both when there is no sound server to have one and when there is one it
/// could not ask, and those are not the same statement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StreamVolume {
    /// No sound server sits between priel and the device, so there is no such
    /// stage. Also what an idle player reports: nothing is open to be scaled.
    Absent,
    /// A sound server is in the chain and its level for our stream could not be
    /// read.
    Unread,
    /// The level, as a percentage, where 100 is unity.
    Read(f64),
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
    /// The audio server is attenuating the *sink* everything on the machine is
    /// mixed into, which is a stage above our own stream and one nothing else
    /// can see.
    ///
    /// Graded from `softVolumes` and never from the control beside it: the
    /// control says what was set, and only that field says whether the server
    /// multiplied a sample. See [`SinkVolume`](graph::SinkVolume).
    SinkVolumeScaled,
}

/// Bits of resolution a sample format name carries, endianness already removed.
///
/// `float` is 32-bit with a 24-bit mantissa, which represents any integer source
/// up to 24 bits exactly - so it counts as 24, not 32. `s24_32` and `s24_3` are
/// both 24 bits, differing only in how many bytes they are carried in.
#[allow(
    clippy::match_same_arms,
    reason = "s24 and float coincide numerically for unrelated reasons; merging them would hide why"
)]
fn base_format_bits(f: &str) -> Option<u32> {
    match f {
        "u8" | "s8" => Some(8),
        "s16" => Some(16),
        "s24" | "s24_3" | "s24_32" => Some(24),
        "s32" => Some(32),
        "float" | "f32" => Some(24),
        "float64" | "double" | "f64" => Some(53),
        _ => None,
    }
}

/// Bits of resolution a sample format can carry, in any of the spellings priel
/// meets.
///
/// Three vocabularies reach this: mpv's (`s32`, `floatp`), ALSA's from
/// /proc/asound (`S32_LE`, `S24_3LE`) and the sound server's from the graph
/// (`S16LE`, `S24_32LE`, `F32P`), which writes the endianness with no separator
/// at all.
///
/// The endianness is only stripped when what is left is a width this knows,
/// rather than trimmed off unconditionally: `double` ends in the same two
/// letters a little-endian suffix does, and blind trimming turned it into
/// nothing.
pub(crate) fn format_bits(fmt: &str) -> Option<u32> {
    let lower = fmt.to_ascii_lowercase();
    // The planar marker, on both mpv's `floatp` and the server's `F32P`.
    let f = lower.strip_suffix('p').unwrap_or(&lower);
    if let Some(bits) = base_format_bits(f) {
        return Some(bits);
    }
    ["_le", "_be", "le", "be"]
        .iter()
        .find_map(|suffix| f.strip_suffix(suffix).and_then(base_format_bits))
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
    ///
    /// Four readings of the rate, strongest first, because each one below is
    /// further from the samples than the one above it:
    ///
    /// 1. [`hw`](Self::hw) - the ALSA device itself, the only unmediated view.
    /// 2. [`sink_rate_hz`](Self::sink_rate_hz) - what the node at the end of the
    ///    graph negotiated. A direct reading of where the samples end up.
    /// 3. [`clock`](Self::clock) - what the server's *global* clock settings say
    ///    it may run at. An inference, and one that does not govern a sink which
    ///    drives itself; it is here for the sinks that publish no rate at all.
    /// 4. mpv's own `sample_rate` - only the rate priel handed over, which hides
    ///    any resample performed below it.
    ///
    /// The format does not follow the rate down this list. It stays mpv's
    /// wherever there is no ALSA readout, because a *narrowing* happens at the
    /// first stage that does it: samples priel handed over as `s16` are not
    /// restored by a sink that later widens them to `s24`, so the sink's own
    /// width would be the flattering reading rather than the true one.
    #[must_use]
    pub fn effective_output(&self) -> (u32, &str) {
        // The ALSA readout is the truth when there is one.
        if let Some(h) = &self.hw {
            return (h.rate, h.format.as_str());
        }
        let rate = self.observed_output_rate().unwrap_or(self.sample_rate);
        (rate, self.out_format.as_str())
    }

    /// The output rate as something downstream actually reported it, if anything
    /// did: the first three rungs of the ladder above and never the fourth.
    ///
    /// mpv's own `sample_rate` is the rate priel *handed over*, which is an echo
    /// of the input rather than an observation of the output - so reading it as
    /// one would have the output agree with the track by construction, and
    /// anything asking "is this rate evidently in use?" would always answer yes.
    /// That is why this is separate from [`Self::effective_output`], which needs
    /// a rate to display and takes the echo rather than showing nothing.
    ///
    /// `None` means nothing below priel said where the samples end up.
    #[must_use]
    pub fn observed_output_rate(&self) -> Option<u32> {
        self.hw
            .as_ref()
            .map(|h| h.rate)
            .or(self.sink_rate_hz)
            .or_else(|| {
                self.clock
                    .as_ref()
                    .and_then(|c| c.output_rate_for(self.in_sample_rate))
            })
            .filter(|&r| r > 0)
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

    /// Is any output actually open?
    ///
    /// Asked wherever a statement about the output would otherwise be made with
    /// nothing behind it: the device readout, the access badge, and the two
    /// volume stages below, which cannot be missing from a chain that does not
    /// exist yet.
    #[must_use]
    pub fn output_is_open(&self) -> bool {
        let (rate_hz, format) = self.effective_output();
        rate_hz > 0 || !format.is_empty()
    }

    /// The sound server's level for priel's own stream, and whether there was
    /// one to read.
    #[must_use]
    pub fn stream_volume(&self) -> StreamVolume {
        // Nothing open, or nothing between priel and the card: either way there
        // is no such stage, which is not the same as one that went unread.
        if !self.output_is_open() || self.bypasses_sound_server() {
            return StreamVolume::Absent;
        }
        match self.ao_volume {
            Some(level) => StreamVolume::Read(level),
            None => StreamVolume::Unread,
        }
    }

    /// The whole chain: what it is doing to the samples, and what that rests on.
    ///
    /// `sink` is the third stage, which only the graph dump carries - mpv can
    /// see its own stream and the device's own parameters, and neither of them
    /// says anything about the mixer everything on the machine shares. It is
    /// passed in rather than reached for, like every other lookup path in this
    /// workspace, so the grading stays pure and table-testable.
    ///
    /// Builds on [`fidelity`](Self::fidelity) rather than restating it: that is
    /// the half mpv can answer for, and it is also what the graph is asked to
    /// attribute, so the two cannot come to different conclusions about one
    /// track.
    #[must_use]
    pub fn verdict(&self, source_bits: u32, sink: &SinkVolume) -> Verdict {
        let stage = sink.stage();
        Verdict {
            fidelity: self.graded(source_bits, stage),
            evidence: self.evidence(stage),
        }
    }

    /// The grade with the sink folded in.
    ///
    /// Only a chain that was otherwise clean can be moved by it: every other
    /// answer already names something at least as damaging, and the order they
    /// are named in is settled - priel's own volume first, because it is the one
    /// a keypress clears.
    fn graded(&self, source_bits: u32, sink: SinkStage) -> Fidelity {
        let graded = self.fidelity(source_bits);
        if graded != Fidelity::BitPerfect {
            return graded;
        }
        match sink {
            SinkStage::InSoftware { .. } | SinkStage::Silenced => {
                Fidelity::NearBitPerfect(Alteration::SinkVolumeScaled)
            }
            SinkStage::Absent
            | SinkStage::Unread
            | SinkStage::Unity
            | SinkStage::Elsewhere { .. } => graded,
        }
    }

    /// Was there a stage that exists here and could not be read?
    ///
    /// A control set away from unity that the server is not applying counts:
    /// something is attenuating and this dump does not say what. That is a gap
    /// in the evidence even though the server's own stage was read perfectly
    /// well.
    fn evidence(&self, sink: SinkStage) -> Evidence {
        let unread = matches!(self.stream_volume(), StreamVolume::Unread)
            || matches!(sink, SinkStage::Unread | SinkStage::Elsewhere { .. });
        if unread {
            Evidence::Partial
        } else {
            Evidence::Complete
        }
    }

    /// The track as the decoder produced it, for comparing a graph's nodes
    /// against.
    ///
    /// Shares the source rule with [`fidelity`](Self::fidelity) rather than
    /// restating it: a declared depth wins, and only where the service declared
    /// none does the container width stand in as an upper bound. Two readings
    /// of the same track that disagreed about how wide it was would put the
    /// badge and the graph overlay at odds.
    #[must_use]
    pub fn decoded_format(&self, source_bits: u32) -> SourceFormat {
        SourceFormat {
            rate_hz: self.in_sample_rate,
            bits: if source_bits > 0 {
                source_bits
            } else {
                format_bits(&self.in_format).unwrap_or(0)
            },
        }
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
        // Zero means the width is not known from either end, which is not a
        // verdict about anything.
        let source_bits = self.decoded_format(source_bits).bits;
        if source_bits == 0 {
            return Fidelity::Unknown;
        }

        // A Bluetooth A2DP link re-encodes every sample, so nothing downstream
        // is bit-perfect whatever the rate or depth. It outranks the rate,
        // depth and volume grades below because it is the dominant, inherent
        // loss; the codec name is carried in `bt_codec` and named at the UI.
        if self.bt_codec.is_some() {
            return Fidelity::Bluetooth;
        }

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

/// Fetch fresh segment URLs for a track whose signed URLs have expired.
///
/// Given a track id, returns a fresh ordered list of segment URLs, or `None`
/// when it cannot (the source is not segmented, or the request failed). Called
/// from the downloader thread when a segment 403s partway through a long track,
/// so the download can resume with fresh URLs rather than ending the track
/// short. The player thread has no authenticated client of its own, so the
/// frontend supplies this - it does the round-trip and blocks the caller until
/// it answers; see [`Player::set_resolver`].
pub type SegmentResolver = Arc<dyn Fn(u64) -> Option<Vec<String>> + Send + Sync>;

/// Commands sent to the player thread.
#[allow(dead_code)] // fields go unread in the stub (no-libmpv) backend
pub(crate) enum Cmd {
    /// Start fresh: replace the playlist with this track (id + source).
    Load(u64, PlayableSource),
    /// Preload the next track by appending it to the playlist (for gapless).
    Append(u64, PlayableSource),
    /// Skip to the preloaded next entry.
    Next,
    /// Pause, or resume. Absolute rather than a cycle, because the callers that
    /// are not a keystroke - a media key, a panel applet - say which one they
    /// mean, and answering them with a cycle pauses a playing track when the
    /// play button is pressed twice.
    SetPaused(bool),
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
    /// Carry the sound server's graph clock into the status, read by the
    /// frontend at track start. The player thread cannot read it itself - it
    /// needs a `pw-dump`, which must not run on this thread - so it arrives as a
    /// command and is published unchanged in every status until the next one.
    SetClock(Option<crate::graph::ClockRates>),
    /// Carry the Bluetooth A2DP codec of the current output into the status,
    /// read by the frontend from the graph. Like [`Cmd::SetClock`] it needs a
    /// `pw-dump` the player thread must not run, so it arrives as a command and
    /// is published unchanged in every status until the next one.
    SetBtCodec(Option<String>),
    /// Carry the rate the graph's sink node negotiated into the status, read by
    /// the frontend from the same graph reads as [`Cmd::SetBtCodec`]. It becomes
    /// the output rate the verdict is made on wherever there is no ALSA readout.
    SetSinkRate(Option<u32>),
    /// Give the player the callback it uses to fetch fresh segment URLs when a
    /// signed URL expires mid-track. See [`SegmentResolver`].
    SetResolver(SegmentResolver),
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
    /// Pause, or resume, whichever the player is not doing.
    ///
    /// The other way round from every other control here: it reads the snapshot
    /// and sends the absolute, so that there is one way to change the pause
    /// state rather than one for a keystroke and another for a remote control.
    pub fn toggle_pause(&self) {
        self.set_paused(!self.status().paused);
    }

    /// Pause, or resume, whichever was asked for.
    ///
    /// Absolute because MPRIS is: `Play` on an already-playing track must leave
    /// it playing.
    pub fn set_paused(&self, paused: bool) {
        self.send(Cmd::SetPaused(paused));
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

    /// Tell the player the sound server's graph clock, read at track start.
    ///
    /// Fire-and-forget like every other command. It becomes the output rate the
    /// verdict falls back to when there is no ALSA readout - a Bluetooth or
    /// network sink - so a resample the server performed on our behalf is caught
    /// rather than hidden. `None` clears it.
    pub fn set_clock(&self, clock: Option<crate::graph::ClockRates>) {
        self.send(Cmd::SetClock(clock));
    }
    /// Tell the player the Bluetooth codec of the current output, read from the
    /// graph.
    ///
    /// Fire-and-forget like [`Self::set_clock`]. `Some` makes the verdict grade
    /// the output [`Fidelity::Bluetooth`] - a lossy link that cannot be
    /// bit-perfect - and `None` clears it when the output is not Bluetooth.
    pub fn set_bt_codec(&self, codec: Option<String>) {
        self.send(Cmd::SetBtCodec(codec));
    }
    /// Tell the player the rate the graph's sink node negotiated.
    ///
    /// Fire-and-forget like [`Self::set_clock`], and read from the same graph.
    /// It outranks the clock as the output rate the verdict is made on - see
    /// [`PlaybackStatus::effective_output`] - because the node says where the
    /// samples end up and the clock only says what the server may run at.
    /// `None` clears it, leaving the clock to stand in as before.
    pub fn set_sink_rate(&self, rate_hz: Option<u32>) {
        self.send(Cmd::SetSinkRate(rate_hz));
    }
    /// Give the player the callback that fetches fresh segment URLs when a
    /// signed URL expires mid-track.
    ///
    /// Fire-and-forget like [`Self::set_clock`]; the player holds it and hands
    /// it to each segment download. Set once at startup. See [`SegmentResolver`].
    pub fn set_resolver(&self, resolver: SegmentResolver) {
        self.send(Cmd::SetResolver(resolver));
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
    /// The player holds the choice for as long as it runs and no longer: it
    /// keeps no file of its own. Whether a choice outlives the session is the
    /// frontend's business, and the terminal frontend remembers it.
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
    /// Like the device, the choice lasts as long as the player does; keeping it
    /// beyond that is the frontend's business.
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
    use crate::graph::SinkLevels;
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

    // Drives the player thread, which only the real backend runs: the stub has
    // no loop to apply a command or to keep the snapshot moving.
    #[cfg(feature = "libmpv")]
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

    // Drives the player thread, which only the real backend runs: the stub has
    // no loop to apply a command or to keep the snapshot moving.
    #[cfg(feature = "libmpv")]
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

    // Drives the player thread, which only the real backend runs: the stub
    // applies no command and never moves the snapshot.
    #[cfg(feature = "libmpv")]
    #[test]
    fn the_toggle_and_the_absolute_are_one_path_to_the_pause_state() {
        // Goal: priel has a toggle where a remote control has absolutes, and
        // there must be one implementation rather than two that can drift. The
        // toggle reads the snapshot and sends the absolute; asking for a state
        // that is already in force leaves it there.
        let p = silent();
        p.toggle_pause();
        assert!(wait_for(&p, |s| s.paused), "the toggle reached mpv");
        p.set_paused(true);
        assert!(wait_for(&p, |s| s.paused), "asking again is not a cycle");
        p.toggle_pause();
        assert!(wait_for(&p, |s| !s.paused), "and back the other way");
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

    // Drives the player thread, which only the real backend runs: the stub has
    // no loop to apply a command or to keep the snapshot moving.
    #[cfg(feature = "libmpv")]
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

    /// A graph clock allowed to run at `allowed`, currently at `current`.
    fn clock_at(allowed: &[u32], current: u32) -> crate::graph::ClockRates {
        crate::graph::ClockRates {
            allowed_hz: Some(allowed.to_vec()),
            current_hz: Some(current),
            forced_hz: None,
        }
    }

    #[test]
    fn a_resample_the_server_hid_is_caught_from_the_clock_when_there_is_no_readout() {
        // Goal: a Bluetooth sink has no /proc/asound readout, so mpv reports only
        // the 44.1 kHz it handed the server and hides the 48 kHz the graph is
        // clocked at. The clock, read at track start, is the fallback that
        // catches it - the verdict must not call a resampled, volume-scaled
        // stream near bit-perfect while the report's own clock section says the
        // rate is not permitted.
        let mut s = PlaybackStatus {
            volume: 50.0,
            clock: Some(clock_at(&[48_000], 48_000)),
            ..playing(44_100, 44_100, "s16", "s16")
        };
        assert_eq!(s.fidelity(16), Fidelity::Altered(Alteration::Resampled));

        // Without the clock it is missed and only the volume shows - proof the
        // clock is what catches it, not something else in the reading.
        s.clock = None;
        assert_ne!(s.fidelity(16), Fidelity::Altered(Alteration::Resampled));
    }

    #[test]
    fn a_bluetooth_output_is_graded_by_its_codec_not_bit_perfect() {
        // Goal: a Bluetooth A2DP link re-encodes every sample, so it can never
        // be bit-perfect whatever the rate or depth. The codec's presence is the
        // verdict, outranking the rate, depth and volume grades below it.
        let mut s = PlaybackStatus {
            bt_codec: Some("aptx_hd".into()),
            ..playing(44_100, 44_100, "s16", "s16") // otherwise bit-perfect
        };
        assert_eq!(
            s.fidelity(16),
            Fidelity::Bluetooth,
            "an otherwise-clean link is still Bluetooth, not bit-perfect"
        );

        // It outranks a resample: the link loss dominates.
        s.sample_rate = 48_000; // out != in
        assert_eq!(
            s.fidelity(16),
            Fidelity::Bluetooth,
            "the link loss dominates a resample"
        );

        // With no codec the same status grades as before - here, the resample.
        s.bt_codec = None;
        assert_eq!(s.fidelity(16), Fidelity::Altered(Alteration::Resampled));
    }

    #[test]
    fn the_sinks_own_rate_outranks_the_graph_clock() {
        // Goal: with no ALSA readout the clock stood in for the output rate, but
        // it is the graph's *global* setting - and a Bluetooth sink is its own
        // driver at the rate bluez negotiated with the device. The sink node's
        // own negotiated rate is a direct reading of where the samples end up,
        // so it wins: a global list that does not name that rate is not evidence
        // of a resample the node itself denies. Reading the weaker of two
        // readings priel holds at once is what put a rate on screen that the
        // chain below it contradicted.
        let mut s = PlaybackStatus {
            clock: Some(clock_at(&[48_000], 48_000)),
            sink_rate_hz: Some(44_100),
            ..playing(44_100, 44_100, "s16", "s16")
        };
        assert_eq!(s.effective_output(), (44_100, "s16"));

        // Without the node's own reading the clock still stands in, as before.
        s.sink_rate_hz = None;
        assert_eq!(s.effective_output(), (48_000, "s16"));

        // And the ALSA readout still beats both: it is the only unmediated view
        // of the hardware there is.
        s.sink_rate_hz = Some(44_100);
        s.hw = Some(HwParams {
            card: "DAC".into(),
            rate: 96_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        assert_eq!(s.effective_output(), (96_000, "S32_LE"));
    }

    #[test]
    fn a_permissive_clock_does_not_invent_a_resample_for_native_playback() {
        // Goal: the fallback must never turn native playback into a false
        // resample. When the track's own rate is permitted the graph runs at it,
        // so the clock reports the track rate and nothing is altered.
        let s = PlaybackStatus {
            clock: Some(clock_at(&[44_100, 48_000], 48_000)),
            ..playing(44_100, 44_100, "s16", "s16")
        };
        assert_eq!(s.fidelity(16), Fidelity::BitPerfect);
    }

    #[test]
    fn the_hardware_readout_still_wins_over_the_clock() {
        // Goal: where there is a real readout it is the truth and the clock is
        // ignored. A DAC clocked at 44.1 is native however the graph metadata
        // would have read.
        let s = PlaybackStatus {
            hw: Some(HwParams {
                card: "DAC".into(),
                rate: 44_100,
                format: "s16".into(),
                channels: 2,
            }),
            clock: Some(clock_at(&[48_000], 48_000)),
            ..playing(44_100, 44_100, "s16", "s16")
        };
        assert_eq!(s.fidelity(16), Fidelity::BitPerfect, "the readout wins");
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
    fn the_track_the_graph_is_compared_against_falls_back_to_the_container() {
        // Goal: the graph's nodes are judged against the same source the badge
        // is, or the two would disagree about the same track. That includes the
        // rule that makes the badge honest - a declared depth wins, and only
        // when there is none does the container width stand in for it.
        let s = playing(44_100, 44_100, "s32", "s32");
        assert_eq!(
            s.decoded_format(24),
            SourceFormat {
                rate_hz: 44_100,
                bits: 24
            }
        );
        assert_eq!(
            s.decoded_format(0),
            SourceFormat {
                rate_hz: 44_100,
                bits: 32
            },
            "with nothing declared the container is the only upper bound there is"
        );

        let s = playing(44_100, 44_100, "unknowable", "s32");
        assert_eq!(
            s.decoded_format(0),
            SourceFormat {
                rate_hz: 44_100,
                bits: 0
            },
            "zero means do not compare, not zero bits"
        );
    }

    #[test]
    fn only_a_rebuilt_sample_stream_is_something_the_graph_can_be_asked_about() {
        // Goal: the attribution takes what was graded and looks for the node
        // that did it. A volume control is not a node, and handing one over
        // would have the chain asked to account for something it never
        // publishes.
        assert_eq!(Fidelity::Unknown.alteration(), None);
        assert_eq!(Fidelity::BitPerfect.alteration(), None);
        assert_eq!(
            Fidelity::NearBitPerfect(Alteration::VolumeScaled).alteration(),
            None
        );
        assert_eq!(
            Fidelity::Altered(Alteration::Resampled).alteration(),
            Some(Alteration::Resampled)
        );
        assert_eq!(
            Fidelity::Altered(Alteration::Truncated).alteration(),
            Some(Alteration::Truncated)
        );
    }

    #[test]
    fn the_sound_servers_format_names_are_understood_too() {
        // Goal: a third vocabulary reaches this. /proc writes `S16_LE`, mpv
        // writes `s16`, and the graph writes `S16LE` with no underscore at all -
        // plus `S24_32LE` for 24 bits carried in a 32-bit word. A width that
        // reads as unknown would leave a node on the path unjudged, which is
        // exactly the node the reader opened the overlay to find.
        for (fmt, bits) in [
            ("S16LE", Some(16)),
            ("S24LE", Some(24)),
            ("S24_32LE", Some(24)),
            ("S32LE", Some(32)),
            ("F32LE", Some(24)),
            ("F32P", Some(24)),
            ("F64LE", Some(53)),
            ("S16BE", Some(16)),
            ("U8", Some(8)),
            ("nonsense", None),
        ] {
            assert_eq!(format_bits(fmt), bits, "format {fmt}");
        }
    }

    #[test]
    fn the_older_format_names_still_map_to_the_same_widths() {
        // Goal: the two vocabularies that were already understood must not
        // regress while a third is added - `double` ends in the same two
        // letters an endianness suffix does, and trimming blindly would turn
        // 53 bits into nothing.
        for (fmt, bits) in [
            ("s16", Some(16)),
            ("s32", Some(32)),
            ("float", Some(24)),
            ("floatp", Some(24)),
            ("double", Some(53)),
            ("S24_3LE", Some(24)),
            ("FLOAT_LE", Some(24)),
            ("S32_LE", Some(32)),
        ] {
            assert_eq!(format_bits(fmt), bits, "format {fmt}");
        }
    }

    // ---- the whole chain, and what the grade rests on ----

    /// A sink reading with the same figure set and applied, which is what a
    /// sink with no hardware mixer publishes.
    fn sink_at(gain: f64) -> SinkVolume {
        SinkVolume::Read(SinkLevels {
            set: vec![gain, gain],
            software: vec![gain, gain],
            silenced: false,
        })
    }

    /// A status playing through the sound server with both readable stages read
    /// and at unity, so a test only has to move the one it is about.
    fn through_server(in_rate: u32, out_rate: u32) -> PlaybackStatus {
        PlaybackStatus {
            ao_volume: Some(100.0),
            audio_device: "pipewire/alsa_output.usb-x".into(),
            ..playing(in_rate, out_rate, "s32", "s32")
        }
    }

    #[test]
    fn a_sink_turned_down_in_software_is_no_longer_reported_as_bit_perfect() {
        // Goal: the whole issue. priel at unity and its own stream at unity say
        // nothing about the sink everything on the machine is mixed into, and
        // reporting a clean chain there is the overstatement this indicator
        // exists to avoid.
        let s = through_server(96_000, 96_000);
        assert_eq!(
            s.verdict(24, &sink_at(0.5)),
            Verdict {
                fidelity: Fidelity::NearBitPerfect(Alteration::SinkVolumeScaled),
                evidence: Evidence::Complete,
            }
        );
        assert_eq!(
            s.verdict(24, &sink_at(1.0)).fidelity,
            Fidelity::BitPerfect,
            "a sink at unity alters nothing and must not cry wolf"
        );
    }

    #[test]
    fn a_sink_set_below_unity_that_the_server_is_not_applying_keeps_the_tick_and_marks_it() {
        // Goal: the reading measured on a real machine - the control at 2.7%
        // with `softVolumes` at unity, on a card with no ALSA volume control at
        // all. The server is multiplying nothing, so the samples are intact;
        // where that 2.7% is applied is not in the dump, so the grade may not
        // be handed out unqualified either.
        let s = through_server(96_000, 96_000);
        let measured = SinkVolume::Read(SinkLevels {
            set: vec![0.027_001, 0.027_001],
            software: vec![1.0, 1.0],
            silenced: false,
        });
        let v = s.verdict(24, &measured);
        assert_eq!(v.fidelity, Fidelity::BitPerfect);
        assert_eq!(v.evidence, Evidence::Partial);
        assert!(v.needs_qualifying(), "the tick has to say what it rests on");
    }

    #[test]
    fn a_stage_that_cannot_exist_counts_as_fully_evidenced() {
        // Goal: the direct path is the cleanest chain the player can produce -
        // no server stage and no sink stage at all - and a permanent question
        // mark on it would make the mark meaningless everywhere else. Absent is
        // not unread.
        let s = PlaybackStatus {
            audio_device: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            ao_volume: None,
            ..playing(96_000, 96_000, "s32", "s32")
        };
        let v = s.verdict(24, &SinkVolume::Absent);
        assert_eq!(v.fidelity, Fidelity::BitPerfect);
        assert_eq!(v.evidence, Evidence::Complete);
        assert!(!v.needs_qualifying(), "a clean chain gets a clean tick");
    }

    #[test]
    fn a_sink_that_could_not_be_read_is_marked_rather_than_assumed_clean() {
        // Goal: "say unknown rather than guess". A sink is there, priel has not
        // read what its volume is doing, and the tick may not silently mean
        // "as far as I looked".
        let s = through_server(96_000, 96_000);
        let v = s.verdict(24, &SinkVolume::Unread);
        assert_eq!(v.fidelity, Fidelity::BitPerfect);
        assert_eq!(v.evidence, Evidence::Partial);
    }

    #[test]
    fn only_a_grade_an_unread_stage_could_have_changed_is_qualified() {
        // Goal: the mark costs a column and must earn it. A resample is already
        // worse than any volume stage could make it, so marking that answer
        // says nothing; only the clean tick can be falsified by a stage nobody
        // looked at.
        let mut s = through_server(44_100, 48_000);
        s.ao_volume = None;
        let v = s.verdict(24, &SinkVolume::Unread);
        assert_eq!(v.evidence, Evidence::Partial, "the gap is still recorded");
        assert!(!v.needs_qualifying(), "but the finding does not need it");
    }

    #[test]
    fn the_stream_stage_is_absent_on_the_direct_path_and_unread_only_when_it_should_be_there() {
        // Goal: the same absent-versus-unread distinction one stage up. mpv
        // reports no `ao-volume` both when there is no sound server to have one
        // and when there is one it could not ask, and treating those alike
        // would mark every exclusive session.
        let mut s = through_server(96_000, 96_000);
        assert_eq!(s.stream_volume(), StreamVolume::Read(100.0));

        s.ao_volume = None;
        assert_eq!(s.stream_volume(), StreamVolume::Unread);

        s.audio_device = "alsa/hw:CARD=AUDIO,DEV=0".into();
        assert_eq!(s.stream_volume(), StreamVolume::Absent);

        assert_eq!(
            PlaybackStatus::default().stream_volume(),
            StreamVolume::Absent,
            "nothing is open, so there is no stage to have missed"
        );
    }

    #[test]
    fn priels_own_volume_is_still_named_ahead_of_the_two_below_it() {
        // Goal: the precedence the row already had must survive a third cause.
        // The one the listener can clear with a keypress is the one to name.
        let mut s = through_server(96_000, 96_000);
        s.volume = 50.0;
        assert_eq!(
            s.verdict(24, &sink_at(0.5)).fidelity,
            Fidelity::NearBitPerfect(Alteration::VolumeScaled)
        );

        s.volume = 100.0;
        s.ao_volume = Some(50.0);
        assert_eq!(
            s.verdict(24, &sink_at(0.5)).fidelity,
            Fidelity::NearBitPerfect(Alteration::ServerVolumeScaled)
        );
    }

    #[test]
    fn a_rebuilt_sample_stream_outranks_every_volume_stage() {
        // Goal: a level change and a resample are different kinds of thing, and
        // a new cause at the bottom of the list must not displace the one that
        // rebuilds every sample.
        let s = through_server(44_100, 48_000);
        assert_eq!(
            s.verdict(24, &sink_at(0.5)).fidelity,
            Fidelity::Altered(Alteration::Resampled)
        );
    }

    #[test]
    fn an_unknown_chain_stays_unknown_however_the_sink_reads() {
        // Goal: a false green light is worse than no light, and so is a false
        // warning. With nothing playing there is no verdict to qualify.
        let s = PlaybackStatus::default();
        assert_eq!(s.verdict(24, &sink_at(0.5)).fidelity, Fidelity::Unknown);
        assert!(!s.verdict(24, &SinkVolume::Unread).needs_qualifying());
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
