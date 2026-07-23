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

//! What sits between priel and the output device, read from `pw-dump`.
//!
//! [`hw`](crate::hw) answers whether the chain is clean by reading the device's
//! own live parameters. It cannot say *what* made it that way, because a
//! `/proc/asound` readout has no idea what is upstream of it. This module reads
//! the other end: the `PipeWire` graph, node by node, from the stream priel
//! feeds to the device that plays it.
//!
//! Which stream is priel's is settled by process id rather than by name. The
//! node `PipeWire` shows is libmpv's and is called `mpv`; matching on that would
//! also match a separate mpv the user happens to be running, and would break the
//! day libmpv renames it. The client object carries
//! `application.process.id`, so the answer is exact.
//!
//! The path is also *judged*, by [`AudioGraph::attribute`]: every node is
//! compared against the track and the first one that diverges is named. That is
//! a pure function over a parsed path, so the whole of it is a table of tests
//! over recorded dumps rather than something only a live session can show, and
//! its most important answer is the one that names nobody - a chain that is
//! readable and accounts for nothing gets an admitted gap, never the nearest
//! candidate.
//!
//! The same dump also carries the server's own clock settings, which is the
//! other half of that diagnosis. A rate the server is not permitted to use is
//! resampled *before* any node on the path sees a sample, so the chain diverges
//! nowhere and something still moved - the exact shape of
//! [`Attribution::Unexplained`]. [`ClockRates`] reads what the server published
//! and [`ClockRates::advise`] turns it into the one-line change that would fix
//! it, both pure and both table-tested. Reading the setting out of the dump is
//! deliberate: priel reads no configuration of its own and has no business
//! opening another application's, and the server publishes what it is actually
//! running on rather than what a file asked for.
//!
//! The third fact the same dump carries is who has the device at the end of the
//! chain open. A server that passes samples through untouched still owns the
//! device and can be reshaped by the next application that starts, so
//! [`DeviceHolder`] names the holder whether or not the chain is clean, and
//! [`DeviceHolder::lines`] gives the change that would free the card - all of it
//! read through [`GraphNode::id`], which was kept for exactly this.
//!
//! The fourth is the volume on that sink, which is the one stage nothing else
//! can see: mpv reports its own stream's level and the device's own parameters
//! say nothing about a mixer upstream of them. [`SinkVolume`] reads it and
//! [`SinkVolume::stage`] judges it, and the reason it takes two fields to do so
//! is the whole of the finding - `channelVolumes` is the control and
//! `softVolumes` is what the server actually multiplies by, and on a real
//! machine they disagree.
//!
//! [`NodeRole`] separates the hops that sit between the stream and the device,
//! which is where a second application holding the device would show up.
//!
//! Linux-only by nature, like the rest of the audio plumbing. Everywhere else
//! `pw-dump` is simply not installed, which is one of the answers.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

use crate::Alteration;
use crate::run::{self, RunError};

/// Where a node sits on the path from priel to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// The stream priel itself feeds into the graph.
    Stream,
    /// A hop between the stream and the device: a loopback, a filter chain, a
    /// virtual sink.
    Intermediate,
    /// The end of the chain - the device node, unless the links stop short of
    /// one.
    Device,
}

/// One node on the path, with the format it negotiated.
///
/// Every field is optional except the identity, because a node that has not
/// negotiated a format yet is a normal thing to find and is worth showing with
/// the format left blank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// The `PipeWire` object id. Kept so a later pass can look the node up
    /// again in the same dump rather than re-deriving everything here.
    pub id: u32,
    /// `node.name`, the stable identifier, e.g. `alsa_output.usb-...`.
    pub name: String,
    /// `node.description` if it has one, else the name again.
    pub description: String,
    /// `media.class` verbatim, e.g. `Audio/Sink` or `Stream/Output/Audio`.
    /// Kept because [`role`](Self::role) is positional and a later pass that has
    /// to be exact about what a node *is* needs the node's own claim.
    pub media_class: String,
    pub role: NodeRole,
    /// Negotiated rate in Hz. A rate change between two hops is a resample.
    pub rate_hz: Option<u32>,
    /// Negotiated sample format, e.g. `S32LE`.
    pub format: Option<String>,
    pub channels: Option<u32>,
}

/// The chain from priel's stream to the device that plays it, in order.
///
/// `Eq` deliberately absent: the sink's volume is a set of `f64` gains read off
/// a JSON dump, and there is no exact equality worth claiming over one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioGraph {
    /// The stream first, the device last. Never empty when this is `Ok`.
    pub path: Vec<GraphNode>,
    /// What the server is permitted to clock this chain at, read from the same
    /// dump so the chain and the setting cannot come from two moments.
    pub clock: ClockRates,
    /// What has the device at the end of the chain open, from the same dump for
    /// the same reason.
    pub holder: DeviceHolder,
    /// What the sink at the end of the chain is doing to the level, from the
    /// same dump for the same reason.
    pub volume: SinkVolume,
    /// The rates the device at the end of the chain can be clocked at, read from
    /// `/proc/asound` (see [`crate::hw::supported_playback_rates`]). Empty when
    /// not known - a non-ALSA sink, or a descriptor that could not be read - and
    /// then no capability claim is made from it. Filled in by [`probe`] rather
    /// than [`parse`], because it comes from the kernel and not from `pw-dump`.
    pub supported_hz: Vec<u32>,
}

impl AudioGraph {
    /// The rates the device supports that the server may not use: S minus A, the
    /// set a one-time "set up audio" pass would add to `clock.allowed-rates`.
    ///
    /// Empty when either side is unknown - the device's rates could not be read,
    /// or the server published no permitted list - because there is then nothing
    /// to compare, and a difference invented from a missing side would send the
    /// listener to change a setting that was never the problem.
    #[must_use]
    pub fn blocked_supported_hz(&self) -> Vec<u32> {
        blocked_rates(self.clock.permitted_hz().as_deref(), &self.supported_hz)
    }

    /// The ALSA card index of the sink at the end of the chain, when it has one.
    ///
    /// A server-held ALSA device names its PCM as `hw:N,M`, and `N` is the
    /// `/proc/asound/cardN` its supported rates are read from. `None` for a
    /// non-ALSA sink - a Bluetooth or network device names no card - or a chain
    /// that reaches no device at all.
    fn sink_card_index(&self) -> Option<u32> {
        let DeviceHolder::Server(held) = &self.holder else {
            return None;
        };
        held.pcm
            .as_deref()?
            .strip_prefix("hw:")?
            .split(',')
            .next()?
            .parse()
            .ok()
    }
}

/// The rates in `supported` that `permitted` does not include - S minus A, the
/// set a "set up audio" pass would add to `clock.allowed-rates`. A `None`
/// permitted list is the server naming no rates at all, so nothing is claimed
/// against it; an empty `supported` is the device's rates being unknown.
fn blocked_rates(permitted: Option<&[u32]>, supported: &[u32]) -> Vec<u32> {
    let Some(allowed) = permitted else {
        return Vec::new();
    };
    supported
        .iter()
        .copied()
        .filter(|hz| !allowed.contains(hz))
        .collect()
}

/// The volume the sound server has on the sink at the end of the chain.
///
/// The third stage that can alter the samples, after priel's own volume and the
/// server's volume for priel's own stream. It is the one nothing looked at, and
/// the one that cannot be read from mpv at all: mpv sees its own stream, not the
/// device everything on the machine is mixed into.
///
/// The three arms are three different things and none of them may stand in for
/// another. [`Absent`](Self::Absent) is a stage that cannot exist - the chain
/// reaches no sink - and counts as fully evidenced. [`Unread`](Self::Unread) is
/// a sink that is there and said nothing, which is an admission. Only
/// [`Read`](Self::Read) is a reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SinkVolume {
    /// The chain reaches no sink, so there is no volume of one to read.
    ///
    /// Also what the direct path reports, told rather than parsed: priel holds
    /// the card itself and there is no server sink between it and the DAC.
    #[default]
    Absent,
    /// A sink is at the end of the chain and what its volume is doing could not
    /// be read - the dump published no `Props`, or published one without the
    /// field the verdict rests on.
    ///
    /// Also what a reader that has not asked yet holds, which is the same
    /// statement: there is a stage here and priel cannot say what it did.
    Unread,
    /// What the sink's `Props` said.
    Read(SinkLevels),
}

/// The two volume figures a sink publishes, which are not the same figure.
///
/// This is the whole point of the reading. `channelVolumes` is the *control* -
/// what a mixer shows and what the listener set - and says nothing about where
/// it is applied. `softVolumes` is what the server's own conversion stage
/// multiplies every sample by, and that is the only one that costs resolution.
///
/// Measured on a real machine, they disagree: a USB DAC sink sat at
/// `channelVolumes: [0.027, 0.027]` with `softVolumes: [1.0, 1.0]`, on a card
/// exposing no ALSA volume control at all. The server was multiplying nothing;
/// the control had been routed away from the software stage and, on that
/// profile, to nothing at all. Reading only the first would have reported a
/// 31 dB loss that was not happening, and reading only the second would have
/// hidden the control entirely.
#[derive(Clone, Debug, PartialEq)]
pub struct SinkLevels {
    /// `channelVolumes`, per channel: the value of the control.
    pub set: Vec<f64>,
    /// `softVolumes`, per channel: the factor the server's own stage applies.
    pub software: Vec<f64>,
    /// `softMute`: the server's own stage is passing silence, whatever the
    /// figures beside it say.
    pub silenced: bool,
}

/// What one volume stage is doing to the samples, and how well that is known.
///
/// Pure over a reading, so the whole of the judgement is a table of tests rather
/// than something only a live sound server can show - the same shape
/// [`AudioGraph::attribute`] and [`ClockRates::advise`] already have.
///
/// Two of the six arms are admissions rather than findings, and keeping them
/// apart is the point. [`Absent`](Self::Absent) counts as fully evidenced;
/// [`Unread`](Self::Unread) and [`Elsewhere`](Self::Elsewhere) are gaps, and
/// what rests on them has to say so.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SinkStage {
    /// There is no such stage here, so there is nothing it could have done.
    Absent,
    /// The stage is there and what it is doing could not be read.
    Unread,
    /// Read: nothing is set and nothing is applied. The one clean answer.
    Unity,
    /// Read: the server's own stage is passing silence.
    ///
    /// Its own arm rather than a gain of zero, because zero has no decibel
    /// figure and no bit count, and a screen reading `-inf dB` is worse than a
    /// word.
    Silenced,
    /// Read: the server's own stage multiplies every sample by `gain`.
    ///
    /// The worst channel where they differ - the one that loses the most bits
    /// is the one the verdict has to be made on.
    InSoftware { gain: f64 },
    /// Read: the control is away from unity and the server is *not* applying
    /// it.
    ///
    /// Where it lands is not in this dump. A hardware mixer on the card would
    /// take it and cost nothing; a profile that routes volume nowhere - which
    /// is what was measured - drops it entirely. priel cannot tell those apart
    /// from here, so it says so rather than picking the flattering one.
    Elsewhere { set: f64 },
}

/// How much attenuation costs a bit of resolution, in decibels.
///
/// The rule the README already records. Six is the round number people use;
/// the exact figure is 6.02, and no display here is precise enough to care.
const DB_PER_BIT: f64 = 6.0;

impl SinkStage {
    /// A linear gain as decibels. Negative for attenuation, as a mixer writes
    /// it.
    #[must_use]
    pub fn db(gain: f64) -> f64 {
        20.0 * gain.log10()
    }

    /// Roughly how many bits of resolution a software gain costs.
    ///
    /// Zero for unity, for a gain above it, and for silence: only attenuation
    /// discards low bits, and silence has no bits left to count. Rounded down,
    /// because claiming a bit that is only partly gone would be the flattering
    /// direction of the two.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "display-only: the quotient is bounded above by the guard and non-negative"
    )]
    pub fn bits_lost(gain: f64) -> u32 {
        if gain <= 0.0 || gain >= 1.0 {
            return 0;
        }
        (-Self::db(gain) / DB_PER_BIT) as u32
    }
}

/// Is a gain far enough from unity to have altered anything?
///
/// The figures arrive as `f64` from a JSON dump and a control set to exactly
/// unity arrives as exactly `1.0`, so this is a guard against arithmetic noise
/// rather than a tolerance for a volume that is nearly right.
fn is_unity(gain: f64) -> bool {
    (gain - 1.0).abs() <= f64::EPSILON
}

/// The worst of a set of per-channel gains: the one the verdict has to be made
/// on.
///
/// Attenuation is the only thing that costs resolution, so the quietest channel
/// decides wherever one is turned down. Where nothing is, a channel *above*
/// unity has still multiplied every sample and is what is left to report -
/// which is not the same finding and carries no bit count.
///
/// `None` when every channel is at unity, and for an empty list, which is not a
/// reading of anything.
fn worst(gains: &[f64]) -> Option<f64> {
    let mut quietest: Option<f64> = None;
    let mut loudest: Option<f64> = None;
    for &gain in gains {
        if is_unity(gain) {
            continue;
        }
        if gain < 1.0 {
            quietest = Some(quietest.map_or(gain, |q: f64| q.min(gain)));
        } else {
            loudest = Some(loudest.map_or(gain, |l: f64| l.max(gain)));
        }
    }
    quietest.or(loudest)
}

impl SinkVolume {
    /// What this stage did to the samples.
    ///
    /// Pure and table-tested. The order is what makes it honest: what the
    /// server *applies* is decided first and from `softVolumes` alone, and the
    /// control is only consulted to explain a stage that applied nothing.
    #[must_use]
    pub fn stage(&self) -> SinkStage {
        let levels = match self {
            Self::Absent => return SinkStage::Absent,
            Self::Unread => return SinkStage::Unread,
            Self::Read(levels) => levels,
        };
        if levels.silenced || levels.software.contains(&0.0) {
            return SinkStage::Silenced;
        }
        if let Some(gain) = worst(&levels.software) {
            return SinkStage::InSoftware { gain };
        }
        // Nothing is being applied here. A control still away from unity has
        // gone somewhere this dump does not name, which is a gap rather than a
        // clean bill.
        match worst(&levels.set) {
            Some(set) => SinkStage::Elsewhere { set },
            None => SinkStage::Unity,
        }
    }
}

/// The rates the sound server is permitted to clock its graph at.
///
/// This is where a resample that no node on the path accounts for usually comes
/// from. [`AudioGraph::attribute`] compares node against node, and a rate the
/// server was never allowed to run at is refused *before* any of them sees a
/// sample - so the chain diverges nowhere and something still moved. That is
/// [`Attribution::Unexplained`], and this is the other half of the answer.
///
/// Every field is `None` for "the dump did not say", which is deliberately not
/// the same as an empty list: an empty `clock.allowed-rates` is a finding - the
/// server may not switch rates at all - and no list is the absence of one.
/// Reporting the second as the first would advise a change from data that was
/// never read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClockRates {
    /// The rates the server may switch between, in the order it publishes them.
    ///
    /// `Some([])` is an empty published list, which pins the graph to
    /// [`current_hz`](Self::current_hz); `None` is a dump that named no list at
    /// all.
    pub allowed_hz: Option<Vec<u32>>,
    /// `clock.rate`: the rate the graph runs at when it is not switching.
    pub current_hz: Option<u32>,
    /// `clock.force-rate`, when something set it: the graph is pinned here and
    /// the permitted list is not consulted at all. Zero on the wire means unset
    /// and arrives here as `None`, because "pinned to 0 Hz" is not a thing.
    pub forced_hz: Option<u32>,
}

impl ClockRates {
    /// What the server may actually clock at right now, or `None` when the dump
    /// did not say enough to know.
    ///
    /// Not simply [`allowed_hz`](Self::allowed_hz): a pin overrides the list
    /// outright, and an empty list is not "no rates" but "no switching", which
    /// leaves exactly the one rate the graph is already clocked at. The readout
    /// and [`advise`](Self::advise) both read this, so the overlay cannot list
    /// rates the advice has already ruled out.
    #[must_use]
    pub fn permitted_hz(&self) -> Option<Vec<u32>> {
        if let Some(forced_hz) = self.forced_hz {
            return Some(vec![forced_hz]);
        }
        match self.allowed_hz.as_deref() {
            Some([]) => self.current_hz.map(|hz| vec![hz]),
            Some(rates_hz) => Some(rates_hz.to_vec()),
            None => None,
        }
    }

    /// Whether this track may be played at its own rate, and what to change.
    ///
    /// Pure over the published setting, the device's supported rates and one
    /// rate, so the whole of the advice is a table of tests rather than
    /// something only a live sound server can show. A `track_rate_hz` of zero
    /// means there is no rate to check - nothing playing, or a source whose rate
    /// is not known - and is never treated as a rate the server refused.
    ///
    /// `supported_hz` is the device's own rates (see
    /// [`AudioGraph::supported_hz`]); an empty slice means they are not known,
    /// and no capability claim is made from it.
    #[must_use]
    pub fn advise(&self, track_rate_hz: u32, supported_hz: &[u32]) -> RateAdvice {
        if track_rate_hz == 0 {
            return RateAdvice::NoTrack;
        }
        // The hardware's own limit is checked first: if the device cannot be
        // clocked at this rate, no server setting changes that, and proposing a
        // config edit would send the listener to fix a file that fixes nothing.
        // An empty list is "not known", so no claim is made from it.
        if !supported_hz.is_empty() && !supported_hz.contains(&track_rate_hz) {
            return RateAdvice::Unsupported {
                ceiling_hz: supported_hz.iter().copied().max().unwrap_or(0),
            };
        }
        // Checked before the list, because a pin applies whether or not the
        // dump published a list at all.
        if let Some(at_hz) = self.forced_hz {
            return if at_hz == track_rate_hz {
                RateAdvice::Permitted
            } else {
                RateAdvice::Pinned { at_hz }
            };
        }
        let Some(mut proposed_hz) = self.permitted_hz() else {
            return RateAdvice::Unknown;
        };
        if proposed_hz.contains(&track_rate_hz) {
            return RateAdvice::Permitted;
        }
        // The proposal *adds* to what is already permitted and never replaces
        // it. A list that dropped a rate would fix this track by taking one
        // away from whatever else on the machine is using it.
        proposed_hz.push(track_rate_hz);
        proposed_hz.sort_unstable();
        proposed_hz.dedup();
        RateAdvice::Missing { proposed_hz }
    }
}

/// Whether the track's rate is one the server may use, and what to do if not.
///
/// Three of the five arms are silence. Advice printed over a working setup
/// teaches the reader to ignore it, and advice invented from a setting that was
/// never read is the failure this whole module is written against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RateAdvice {
    /// There is no rate to check: nothing is playing, or the source's rate is
    /// not known yet.
    NoTrack,
    /// The dump published no permitted rates. Nothing is claimed either way -
    /// distinct from [`Permitted`](Self::Permitted), which is a finding.
    Unknown,
    /// The track's rate is one the server may clock at. Nothing to change.
    Permitted,
    /// The rate is not among the permitted ones, and
    /// [`proposed_hz`](Self::Missing::proposed_hz) is the list that would
    /// include it: everything already permitted, plus this rate, ascending.
    Missing { proposed_hz: Vec<u32> },
    /// `clock.force-rate` pins the graph to `at_hz` and the permitted list is
    /// not consulted at all, so extending it would change nothing.
    Pinned { at_hz: u32 },
    /// The device itself cannot be clocked at this rate - it is above what the
    /// hardware does - so no server setting helps and the resample is forced by
    /// the hardware's own limit. `ceiling_hz` is the highest rate it can do.
    Unsupported { ceiling_hz: u32 },
}

/// The longest line the audio-graph overlay can draw without losing its tail.
///
/// The box is 76 columns at most, two of which are the border and two the
/// indent the overlay adds to prose.
const ADVICE_WIDTH: usize = 70;

/// How many rates go on one line once the list stops fitting on one.
const RATES_PER_LINE: usize = 6;

impl RateAdvice {
    /// What to tell the reader, one line per row of the overlay.
    ///
    /// Empty where there is nothing to advise. Lines rather than a paragraph
    /// because the overlay draws one row per line and cannot rewrap without its
    /// scroll bounds and its output disagreeing about how many rows there are -
    /// the same reason [`GraphError::hint`] is separate from its `Display`.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::NoTrack | Self::Unknown | Self::Permitted => Vec::new(),
            Self::Missing { proposed_hz } => {
                let mut out = vec![
                    "This rate is not one the server is permitted to use.".to_string(),
                    "Put this in ~/.config/pipewire/pipewire.conf.d/10-rates.conf:".to_string(),
                    "  context.properties = {".to_string(),
                ];
                out.extend(allowed_rates_lines(proposed_hz));
                out.push("  }".to_string());
                out.push("Restart the sound server for it to take effect.".to_string());
                out
            }
            Self::Pinned { at_hz } => vec![
                format!("The server is pinned to {at_hz} Hz, whatever the list allows."),
                "Clear the pin with:".to_string(),
                "  pw-metadata -n settings 0 clock.force-rate 0".to_string(),
            ],
            Self::Unsupported { ceiling_hz } => vec![
                format!(
                    "Your DAC does not do this rate; it tops out at {}.",
                    khz(*ceiling_hz)
                ),
                "This track is resampled, and no audio setting changes that.".to_string(),
            ],
        }
    }
}

/// One rate as kilohertz, e.g. `192 kHz` or `44.1 kHz`.
fn khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{:.1} kHz", f64::from(hz) / 1000.0)
    }
}

/// The setting itself, in lines that fit the box it is drawn in.
///
/// A ten-rate list runs well past the overlay, and the renderer clips rather
/// than wrapping - which would leave a configuration line that still looks
/// copyable and is not. The server's own core object publishes the list across
/// several lines, so the spread-out spelling is one it writes itself.
fn allowed_rates_lines(rates_hz: &[u32]) -> Vec<String> {
    let one = format!("    default.clock.allowed-rates = [ {} ]", spaced(rates_hz));
    if one.chars().count() <= ADVICE_WIDTH {
        return vec![one];
    }
    let mut out = vec!["    default.clock.allowed-rates = [".to_string()];
    out.extend(
        rates_hz
            .chunks(RATES_PER_LINE)
            .map(|chunk| format!("      {}", spaced(chunk))),
    );
    out.push("    ]".to_string());
    out
}

fn spaced(rates_hz: &[u32]) -> String {
    rates_hz
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// What has the output device open.
///
/// The sound server passing samples through untouched is still a mixer: it owns
/// the device, and the graph can be reshaped the moment another application
/// starts. Anyone who wants the DAC out of the graph has to know that it is
/// claimed before anything else, so this is reported whether or not the chain is
/// clean.
///
/// The three answers that are not [`Server`](Self::Server) are all silence, and
/// deliberately so. [`Unknown`](Self::Unknown) is an admission and never the
/// nearest card on the machine - a rule matching a name priel guessed at would
/// disable something that was working. [`NoDevice`](Self::NoDevice) is a
/// different admission: nothing is at the end of the chain to have a holder.
/// And [`Direct`](Self::Direct) is the destination rather than a problem, so it
/// gets no advice at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceHolder {
    /// The chain reaches no output device: priel's stream ends before one.
    ///
    /// Also what an empty graph reports, which is the same statement about a
    /// path with nothing on it.
    #[default]
    NoDevice,
    /// The chain ends at `sink` and the dump does not say what device is behind
    /// it, so there is nothing to name.
    Unknown { sink: String },
    /// The sound server has an output device open and mixes into it.
    Server(HeldDevice),
    /// priel has the device itself, with nothing between it and the DAC.
    ///
    /// Never read from a dump: a direct card device puts priel outside the
    /// sound server, so there is no graph for it to appear in. The player knows
    /// which device it opened and says so, exactly as it does for
    /// [`GraphError::Bypassed`].
    Direct { device: String },
}

/// The device the sound server is holding, as the dump describes it.
///
/// Everything but the sink is optional, and each `None` is a separate thing the
/// dump did not say. `card_name` especially: the holder being known and the card
/// being nameable are two facts, and only the second one can produce a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldDevice {
    /// The sink the samples end at, by description.
    pub sink: String,
    /// What opened it: `application.process.binary` of the client that created
    /// the node, which on a normal desktop is the session manager rather than
    /// the server itself.
    pub opened_by: Option<String>,
    /// `api.alsa.path` of the sink, e.g. `hw:2,0` - the PCM the card admits one
    /// opener on.
    pub pcm: Option<String>,
    /// `device.name` of the card object, e.g. `alsa_card.usb-...`.
    ///
    /// Only ever filled in for an ALSA card, because the rule below can reserve
    /// nothing else. A Bluetooth or network device leaves this `None` and takes
    /// the advice with it.
    pub card_name: Option<String>,
}

impl DeviceHolder {
    /// What it would take to hand the device over, one line per row.
    ///
    /// Empty wherever there is nothing to hand over or nothing to name. The
    /// advice is about freeing the card *from the server*, which is a change to
    /// the server's own configuration; asking priel to take a free card is a
    /// separate thing that already has a flag and a toggle, and repeating it
    /// here would teach the reader that this section restates what they know.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let Self::Server(held) = self else {
            return Vec::new();
        };
        let mut out = vec![
            "The sound server has this device open and mixes everything on".to_string(),
            "the machine into it.".to_string(),
        ];
        let Some(card_name) = held.card_name.as_deref() else {
            out.push("The graph does not name the card behind it, so there is no".to_string());
            out.push("rule to reserve here.".to_string());
            return out;
        };
        out.push("To reserve the card, stop the server from claiming it. Put".to_string());
        out.push("this in ~/.config/wireplumber/wireplumber.conf.d/51-reserve.conf:".to_string());
        out.extend(reserve_lines(card_name));
        out.push("Restart the sound server for it to take effect.".to_string());
        out.push("Nothing else on this machine will be able to play through this".to_string());
        out.push("device while that rule is in place.".to_string());
        out
    }
}

/// The rule that stops the sound server claiming one card.
///
/// Spread over lines rather than written as the one-liner it could be: the card
/// name is the longest thing in the overlay and the renderer clips instead of
/// wrapping, so the compact spelling would lose its tail on exactly the cards
/// with the longest names. This layout leaves the name over forty columns, which
/// is more than the bus-derived names ALSA builds have ever needed.
fn reserve_lines(card_name: &str) -> Vec<String> {
    vec![
        "  monitor.alsa.rules = [".to_string(),
        "    {".to_string(),
        "      matches = [".to_string(),
        format!("        {{ device.name = \"{card_name}\" }}"),
        "      ]".to_string(),
        "      actions = { update-props = { device.disabled = true } }".to_string(),
        "    }".to_string(),
        "  ]".to_string(),
    ]
}

/// The track as the decoder produced it, which every node is compared against.
///
/// Both fields are zero when the answer is not known - between tracks, or for a
/// source whose depth the service did not declare and whose container width says
/// nothing useful. Zero means "do not compare", never "zero of them".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceFormat {
    /// The decoded sample rate in Hz.
    pub rate_hz: u32,
    /// The source's true bit depth, *not* the width of the container it arrives
    /// in: 24-bit content decodes into a 32-bit word, and comparing containers
    /// would call a lossless `S32LE -> S24LE` hop a truncation.
    pub bits: u32,
}

/// What the graph has to say about the samples being altered.
///
/// The badge already reports *that* they were altered, from the device's own
/// parameters. This is the other half of the question - which node did it - and
/// its most important arm is [`Unexplained`](Self::Unexplained): the chain is
/// readable, it accounts for nothing, and the nearest candidate is a guess. A
/// wrong name sends the reader to reconfigure something that was working, which
/// is worse than admitting the gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// Every node carries the track's own rate and enough width for it. There
    /// is no accusation to make.
    Clean,
    /// `path[index]` is the first node whose rate or width differs from the
    /// track's. Everything downstream of it is already altered, so it is the
    /// only one worth naming.
    Node {
        index: usize,
        alteration: Alteration,
    },
    /// The samples are being altered and no node on the path accounts for it.
    ///
    /// The normal cause is a resample the sound server performs inside a node
    /// rather than between two of them, which the published formats do not show.
    Unexplained(Alteration),
    /// There was nothing to compare: the track's own format is not known yet, or
    /// no node on the path has published one.
    ///
    /// Distinct from [`Clean`](Self::Clean), which is a finding. This one is the
    /// absence of a finding, and reporting it as the former would turn silence
    /// into a green light.
    NothingToCompare,
}

impl AudioGraph {
    /// Name the node that is altering the samples, if the chain names one.
    ///
    /// Pure, so the whole of it is a table of tests against recorded dumps
    /// rather than something only a live sound server can show. `observed` is
    /// the alteration the player already graded from the device's parameters,
    /// or `None` when it graded none; it is what makes the difference between
    /// "the chain is clean" and "the chain is clean and something still moved".
    #[must_use]
    pub fn attribute(&self, source: SourceFormat, observed: Option<Alteration>) -> Attribution {
        if let Some(named) = self.first_divergence(source) {
            return named;
        }
        // A graph publishes rates and widths and nothing else, so a volume
        // control is neither something it can name nor something it can fail to
        // explain - it is judged against whatever there was to compare at all.
        let compared = match observed {
            Some(Alteration::Resampled) => self.compared_rates(source),
            Some(Alteration::Truncated) => self.compared_widths(source),
            Some(
                Alteration::VolumeScaled
                | Alteration::ServerVolumeScaled
                | Alteration::SinkVolumeScaled,
            )
            | None => self.compared_rates(source) || self.compared_widths(source),
        };
        if !compared {
            return Attribution::NothingToCompare;
        }
        match observed {
            Some(a @ (Alteration::Resampled | Alteration::Truncated)) => {
                Attribution::Unexplained(a)
            }
            _ => Attribution::Clean,
        }
    }

    /// The first hop that is off the track's rate or below its width.
    ///
    /// The two are searched independently and the earlier one wins, because
    /// everything downstream of a divergence is working on samples that have
    /// already been rebuilt. Where both land on the same hop the rate is
    /// reported: it replaces every sample, where a narrowing keeps them and
    /// drops the low bits.
    fn first_divergence(&self, source: SourceFormat) -> Option<Attribution> {
        let resampler = self.first(|n| {
            source.rate_hz > 0 && n.rate_hz.is_some_and(|rate_hz| rate_hz != source.rate_hz)
        });
        let narrower =
            self.first(|n| source.bits > 0 && node_bits(n).is_some_and(|bits| bits < source.bits));
        match (resampler, narrower) {
            (Some(rate_hop), Some(width_hop)) if width_hop < rate_hop => Some(Attribution::Node {
                index: width_hop,
                alteration: Alteration::Truncated,
            }),
            (Some(index), _) => Some(Attribution::Node {
                index,
                alteration: Alteration::Resampled,
            }),
            (None, Some(index)) => Some(Attribution::Node {
                index,
                alteration: Alteration::Truncated,
            }),
            (None, None) => None,
        }
    }

    fn first(&self, matches: impl Fn(&GraphNode) -> bool) -> Option<usize> {
        self.path.iter().position(matches)
    }

    /// Was there a rate on both sides to compare at all?
    fn compared_rates(&self, source: SourceFormat) -> bool {
        source.rate_hz > 0 && self.path.iter().any(|n| n.rate_hz.is_some())
    }

    /// Was there a width on both sides to compare at all?
    fn compared_widths(&self, source: SourceFormat) -> bool {
        source.bits > 0 && self.path.iter().any(|n| node_bits(n).is_some())
    }
}

/// How many bits of resolution a node's negotiated format carries.
///
/// `None` covers both "it has not negotiated one" and "it named a format priel
/// does not know the width of", which come to the same thing here: nothing to
/// compare.
fn node_bits(node: &GraphNode) -> Option<u32> {
    node.format.as_deref().and_then(crate::format_bits)
}

/// Why the graph could not be read.
///
/// The caller only ever displays these, but they stay distinct rather than
/// collapsing into one string: "the tools are not installed" and "nothing is
/// playing" are different situations and want different sentences, and a later
/// slice may want to act on the difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// `pw-dump` is not on `PATH`. Usually means no `PipeWire` at all.
    NotInstalled,
    /// It is there but could not be run, or it ran and failed.
    Unavailable(String),
    /// It did not answer inside the deadline and was killed.
    TimedOut,
    /// It answered with something that is not the object list it documents.
    Unreadable,
    /// The dump parsed, but priel has no stream in the graph.
    NoStream,
    /// There is no graph to read: priel is playing straight to the hardware.
    ///
    /// Distinct from [`Self::NoStream`], which means the graph exists and priel
    /// is not in it yet - that reads as "nothing is playing", and this is the
    /// opposite situation. Nothing sits between priel and the DAC at all, which
    /// is the point of the direct path rather than a failure of it. The player
    /// knows which device it is using, so this is *told* to the overlay rather
    /// than inferred from an absence.
    Bypassed,
}

impl GraphError {
    /// What to do about it, where there is anything to do.
    ///
    /// Separate from [`Display`](fmt::Display) rather than a second sentence
    /// inside it, because the overlay draws one row per line and cannot rewrap
    /// a paragraph without its scroll bounds and its output disagreeing about
    /// how many lines there are.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NotInstalled => Some("It ships with PipeWire's command-line tools."),
            Self::Unavailable(_) => None,
            Self::TimedOut => Some("The graph was busy. Try again."),
            Self::Unreadable => Some("Worth reporting, with the log from [M]."),
            Self::NoStream => Some("It appears once playback starts."),
            Self::Bypassed => Some("Nothing sits between priel and the DAC. That is the point."),
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("pw-dump was not found."),
            Self::Unavailable(why) => write!(f, "pw-dump could not be run: {why}."),
            Self::TimedOut => f.write_str("pw-dump did not answer within two seconds."),
            Self::Unreadable => f.write_str("pw-dump answered with something priel cannot parse."),
            Self::NoStream => f.write_str("priel has no stream in the graph."),
            Self::Bypassed => {
                f.write_str("There is no graph: priel holds the output device directly.")
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// How long `pw-dump` gets. It normally answers in tens of milliseconds; this
/// is long enough for a loaded machine and short enough that a wedged one does
/// not hold the worker thread for a noticeable time.
const DUMP_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest chain worth following.
///
/// A stream normally reaches the device in one hop, and two or three with a
/// loopback or a filter chain in the way. The bound is what makes a graph that
/// links back on itself terminate.
const MAX_HOPS: usize = 8;

/// Read the graph for this process.
///
/// **Runs a subprocess and waits for it.** Call it from the worker thread, never
/// from the render loop.
///
/// # Errors
///
/// Returns the reason rather than an empty result: every failure here is
/// something the user is shown a sentence about.
pub fn probe() -> Result<AudioGraph, GraphError> {
    let out = run::capture("pw-dump", &[], DUMP_TIMEOUT).map_err(|e| match e {
        RunError::NotFound => GraphError::NotInstalled,
        RunError::TimedOut => GraphError::TimedOut,
        RunError::Spawn(why) => GraphError::Unavailable(why),
        RunError::Failed(Some(code)) => {
            GraphError::Unavailable(format!("it exited with status {code}"))
        }
        RunError::Failed(None) => GraphError::Unavailable("it was killed by a signal".into()),
        RunError::Unreadable => GraphError::Unavailable("its output could not be read".into()),
    })?;
    let text = String::from_utf8(out).map_err(|_| GraphError::Unreadable)?;
    let mut graph = parse(&text, std::process::id())?;
    // The device's rates come from the kernel, not the dump: `parse` cannot read
    // them and stay pure, so they are filled in here, keyed by the sink's card.
    if let Some(index) = graph.sink_card_index() {
        graph.supported_hz = crate::hw::supported_rates_for_card(index);
    }
    Ok(graph)
}

/// A sound-server sink that fronts an ALSA card.
///
/// This is the join between the two ways of naming one DAC. A hardware device
/// is `alsa/hw:CARD=AUDIO,DEV=0` and the server's entry for the same card is
/// `pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0`: the two
/// strings share no substring at all, so nothing textual can pair them. The
/// server publishes the card on its own node, and that is the only reliable
/// link there is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSink {
    /// `node.name`, which the player's device identifier is built from by
    /// putting the driver in front of it.
    pub node_name: String,
    /// `alsa.card` - the card index, numbered as `/proc/asound/cardN` is.
    pub card_index: Option<u32>,
    /// `alsa.id` - the card id, spelled as `hw:CARD=<id>` spells it.
    pub card_id: Option<String>,
}

/// The sound server's sinks, or nothing at all.
///
/// **Runs a subprocess and waits for it**, so this belongs on a thread that can
/// afford to wait. It is called once, on a refused exclusive open, to find
/// somewhere to keep playing.
///
/// Every failure is an empty list rather than an error: a machine with no sound
/// server has no sinks, which is a perfectly good answer to "what else could
/// play this", and the caller's next step is the same either way.
#[must_use]
pub fn sinks() -> Vec<ServerSink> {
    let Ok(out) = run::capture("pw-dump", &[], DUMP_TIMEOUT) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(out) else {
        return Vec::new();
    };
    parse_sinks(&text)
}

/// Pull the ALSA-backed sinks out of a `pw-dump` object list.
///
/// Separate from [`sinks`] so the mapping is testable against a recorded dump
/// with no sound server anywhere near the machine running the tests. Anything
/// unparseable is an empty list, for the same reason as above.
#[must_use]
pub fn parse_sinks(dump: &str) -> Vec<ServerSink> {
    let Ok(objects) = serde_json::from_str::<Vec<Value>>(dump) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Node"))
        .filter(|o| prop_str(o, "media.class") == Some("Audio/Sink"))
        .filter_map(|o| {
            let node_name = prop_str(o, "node.name")?;
            let sink = ServerSink {
                node_name: node_name.to_string(),
                card_index: prop_u32(o, "alsa.card"),
                card_id: prop_str(o, "alsa.id").map(ToString::to_string),
            };
            // A sink with no card behind it is a virtual one, and there is no
            // hardware device it could ever be the shared spelling of.
            (sink.card_index.is_some() || sink.card_id.is_some()).then_some(sink)
        })
        .collect()
}

/// Pull the server's clock settings out of a `pw-dump` object list.
///
/// Separate from [`parse`] so the reading is testable against a recorded dump,
/// and because it does not need priel to have a stream in the graph: the setting
/// is the server's, not this process's.
///
/// Anything unreadable is [`ClockRates::default`], which claims nothing. There
/// is no error arm because there is nothing a caller could do with one that it
/// would not also do with "the dump did not say".
#[must_use]
pub fn parse_clock(dump: &str) -> ClockRates {
    let Ok(objects) = serde_json::from_str::<Vec<Value>>(dump) else {
        return ClockRates::default();
    };
    clock_of(&objects)
}

/// The live settings first, the configured ones second.
///
/// The `settings` metadata is what the server is running on *now* and can be
/// changed while it runs; the core object carries what its configuration asked
/// for. Preferring the file's answer over the running one would advise a change
/// that has already been made.
fn clock_of(objects: &[Value]) -> ClockRates {
    let live = objects.iter().find(|o| {
        is_type(o, "PipeWire:Interface:Metadata")
            && o.pointer("/props/metadata.name").and_then(Value::as_str) == Some("settings")
    });
    let core = objects
        .iter()
        .find(|o| is_type(o, "PipeWire:Interface:Core"));
    let configured = |key: &str| core.and_then(|o| o.pointer(&format!("/info/props/{key}")));

    ClockRates {
        allowed_hz: live
            .and_then(|o| setting(o, "clock.allowed-rates"))
            .and_then(rate_list)
            .or_else(|| configured("default.clock.allowed-rates").and_then(rate_list)),
        current_hz: live
            .and_then(|o| setting(o, "clock.rate"))
            .and_then(as_u32)
            .or_else(|| configured("default.clock.rate").and_then(as_u32)),
        // Only the live metadata carries this one: it is set at runtime and has
        // no spelling in the configuration file.
        forced_hz: live
            .and_then(|o| setting(o, "clock.force-rate"))
            .and_then(as_u32)
            .filter(|&hz| hz > 0),
    }
}

/// One entry out of a metadata object's list.
fn setting<'a>(metadata: &'a Value, key: &str) -> Option<&'a Value> {
    metadata
        .get("metadata")?
        .as_array()?
        .iter()
        .find(|e| e.get("key").and_then(Value::as_str) == Some(key))
        .and_then(|e| e.get("value"))
}

/// Every rate in a rate list, however this build chose to spell one.
///
/// The same list arrives from the live metadata as `"[ 44100, 48000 ]"` and
/// from the core object as a multi-line array of *quoted* strings, and a build
/// that published a real JSON array would be a third spelling. So the runs of
/// digits are read and the punctuation between them is not: nothing in a rate
/// list is a rate except a number.
///
/// `None` for a value that is neither text nor an array, because an empty list
/// is a finding and must not be invented out of something unread.
fn rate_list(v: &Value) -> Option<Vec<u32>> {
    if let Some(items) = v.as_array() {
        return Some(items.iter().filter_map(as_u32).collect());
    }
    let text = v.as_str()?;
    Some(
        text.split(|c: char| !c.is_ascii_digit())
            .filter_map(|run| run.parse().ok())
            .collect(),
    )
}

/// Pull the path belonging to `pid` out of a `pw-dump` object list.
///
/// Separate from [`probe`] so the parsing is testable against a recorded dump
/// with no `PipeWire` session anywhere near the machine running the tests.
///
/// # Errors
///
/// [`GraphError::Unreadable`] if the text is not the object list `pw-dump`
/// documents, [`GraphError::NoStream`] if it is but holds no stream for `pid`.
pub fn parse(dump: &str, pid: u32) -> Result<AudioGraph, GraphError> {
    let objects: Vec<Value> = serde_json::from_str(dump).map_err(|_| GraphError::Unreadable)?;
    let start = our_stream(&objects, pid).ok_or(GraphError::NoStream)?;

    // Walk the links downstream, bounded twice over: by `MAX_HOPS`, and by
    // refusing to visit a node already on the path. A loopback makes the link
    // graph a cycle, and either guard alone is enough to stop it - both are
    // here because the second also keeps the list honest, and the first also
    // covers a chain that is merely absurd rather than circular.
    let mut ids = Vec::new();
    let mut current = start;
    for _ in 0..MAX_HOPS {
        ids.push(current);
        let Some(next) = downstream(&objects, current) else {
            break;
        };
        if ids.contains(&next) {
            break;
        }
        current = next;
    }

    let last = ids.len().saturating_sub(1);
    let path: Vec<GraphNode> = ids
        .iter()
        .enumerate()
        .filter_map(|(hop, id)| node_at(&objects, *id, role_of(hop, last)))
        .collect();
    if path.is_empty() {
        return Err(GraphError::NoStream);
    }
    Ok(AudioGraph {
        holder: holder_of(&objects, &path),
        volume: sink_volume_of(&objects, &path),
        path,
        clock: clock_of(&objects),
        // The kernel names the device's rates, not `pw-dump`; `probe` fills this
        // in. `parse` stays a pure function of the dump so it keeps its tests.
        supported_hz: Vec::new(),
    })
}

/// What the sink at the end of the path is doing to the level.
///
/// A per-concern extractor over the already-parsed object list, like
/// [`clock_of`] and [`holder_of`]: pure, so the whole of it is a table of tests
/// over recorded dumps rather than something only a live sound server can show.
///
/// Every failure to answer stops at [`SinkVolume::Unread`] rather than falling
/// through to unity. A sink that published nothing is not a sink at unity, and
/// the difference is the whole of the grade the badge rests on.
fn sink_volume_of(objects: &[Value], path: &[GraphNode]) -> SinkVolume {
    let Some(sink) = path.last().filter(|n| n.role == NodeRole::Device) else {
        return SinkVolume::Absent;
    };
    let Some(node) = object_at(objects, "PipeWire:Interface:Node", sink.id) else {
        return SinkVolume::Unread;
    };
    // A sink publishes more than one `Props`: the conversion stage's, which
    // carries the volumes, and the ALSA device's, which carries the PCM. Found
    // by the field that is wanted rather than by position, because which comes
    // first is the server's business and not a thing to depend on.
    let Some(props) = node
        .pointer("/info/params/Props")
        .and_then(Value::as_array)
        .and_then(|all| all.iter().find(|p| p.get("softVolumes").is_some()))
    else {
        return SinkVolume::Unread;
    };
    let (Some(set), Some(software)) = (gains(props, "channelVolumes"), gains(props, "softVolumes"))
    else {
        return SinkVolume::Unread;
    };
    SinkVolume::Read(SinkLevels {
        set,
        software,
        silenced: props
            .get("softMute")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// One per-channel gain list out of a `Props` object.
///
/// `None` for anything that is not a list of numbers, and for an empty one: no
/// channels is not a reading of a volume, and defaulting it to unity would hand
/// out a clean tick from data that was never published.
fn gains(props: &Value, key: &str) -> Option<Vec<f64>> {
    let list: Vec<f64> = props
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    (!list.is_empty()).then_some(list)
}

/// What has the device at the end of the path open.
///
/// A per-concern extractor over the already-parsed object list, like
/// [`clock_of`]: pure, so the whole of it is a table of tests over recorded
/// dumps rather than something only a live sound server can show.
///
/// Every step can fail to answer and each failure stops here rather than
/// falling through to a likelier-looking one. A sink with no device behind it in
/// this dump is [`DeviceHolder::Unknown`], not the card that happens to match
/// its name: the reader acts on this by disabling something, and disabling the
/// wrong thing is worse than being told priel could not tell.
fn holder_of(objects: &[Value], path: &[GraphNode]) -> DeviceHolder {
    let Some(sink) = path.last().filter(|n| n.role == NodeRole::Device) else {
        return DeviceHolder::NoDevice;
    };
    let unknown = || DeviceHolder::Unknown {
        sink: sink.description.clone(),
    };
    let Some(node) = object_at(objects, "PipeWire:Interface:Node", sink.id) else {
        return unknown();
    };
    let Some(card) = prop_u32(node, "device.id")
        .and_then(|id| object_at(objects, "PipeWire:Interface:Device", id))
    else {
        return unknown();
    };
    DeviceHolder::Server(HeldDevice {
        sink: sink.description.clone(),
        opened_by: prop_u32(node, "client.id")
            .and_then(|id| object_at(objects, "PipeWire:Interface:Client", id))
            .and_then(|c| prop_str(c, "application.process.binary"))
            .map(ToString::to_string),
        pcm: prop_str(node, "api.alsa.path").map(ToString::to_string),
        // Filled in only for an ALSA card, because `monitor.alsa.rules` can
        // reserve nothing else and a name in a rule that matches nothing is
        // advice that silently does nothing.
        card_name: (prop_str(card, "device.api") == Some("alsa"))
            .then(|| prop_str(card, "device.name"))
            .flatten()
            .map(ToString::to_string),
    })
}

/// One object of a given interface, by id.
fn object_at<'a>(objects: &'a [Value], interface: &str, id: u32) -> Option<&'a Value> {
    objects
        .iter()
        .filter(|o| is_type(o, interface))
        .find(|o| id_of(o) == Some(id))
}

/// Where a hop sits, from its position rather than from its `media.class`.
///
/// Position is what the reader is being shown - the chain ends where the links
/// end, whatever the last node calls itself. `GraphNode::media_class` carries
/// the node's own claim for anything that needs to be exact about it.
fn role_of(hop: usize, last: usize) -> NodeRole {
    if hop == 0 {
        NodeRole::Stream
    } else if hop == last {
        NodeRole::Device
    } else {
        NodeRole::Intermediate
    }
}

/// The id of the output stream this process owns.
///
/// Prefers one that is running: libmpv can leave a suspended node behind after
/// a device change, and the running one is the chain actually in use.
fn our_stream(objects: &[Value], pid: u32) -> Option<u32> {
    let clients: Vec<u32> = objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Client"))
        .filter(|o| prop_u32(o, "application.process.id") == Some(pid))
        .filter_map(id_of)
        .collect();

    let mut suspended = None;
    for o in objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Node"))
    {
        let Some(client) = prop_u32(o, "client.id") else {
            continue;
        };
        if !clients.contains(&client) {
            continue;
        }
        if !prop_str(o, "media.class").is_some_and(|c| c.starts_with("Stream/Output")) {
            continue;
        }
        let Some(id) = id_of(o) else {
            continue;
        };
        if o.pointer("/info/state").and_then(Value::as_str) == Some("running") {
            return Some(id);
        }
        if suspended.is_none() {
            suspended = Some(id);
        }
    }
    suspended
}

/// The node the given one feeds into, if anything.
///
/// A stereo pair is two links between the same two nodes, so the first match is
/// the answer and the rest say the same thing again.
fn downstream(objects: &[Value], from: u32) -> Option<u32> {
    objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Link"))
        .find(|o| field_u32(o, "output-node-id") == Some(from))
        .and_then(|o| field_u32(o, "input-node-id"))
}

/// Build the reportable node for an id, or nothing if the dump has no such node.
fn node_at(objects: &[Value], id: u32, role: NodeRole) -> Option<GraphNode> {
    let o = object_at(objects, "PipeWire:Interface:Node", id)?;
    let name = prop_str(o, "node.name").unwrap_or_default().to_string();
    let description = prop_str(o, "node.description")
        .filter(|d| !d.is_empty())
        .map_or_else(|| name.clone(), ToString::to_string);
    let format = o
        .pointer("/info/params/Format")
        .and_then(Value::as_array)
        .and_then(|f| f.first());
    Some(GraphNode {
        id,
        name,
        description,
        media_class: prop_str(o, "media.class").unwrap_or_default().to_string(),
        role,
        rate_hz: format.and_then(|f| f.get("rate")).and_then(as_u32),
        format: format
            .and_then(|f| f.get("format"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        channels: format.and_then(|f| f.get("channels")).and_then(as_u32),
    })
}

fn is_type(o: &Value, name: &str) -> bool {
    o.get("type").and_then(Value::as_str) == Some(name)
}

fn id_of(o: &Value) -> Option<u32> {
    o.get("id").and_then(as_u32)
}

fn field_u32(o: &Value, key: &str) -> Option<u32> {
    o.pointer("/info").and_then(|i| i.get(key)).and_then(as_u32)
}

fn prop_u32(o: &Value, key: &str) -> Option<u32> {
    o.pointer("/info/props")
        .and_then(|p| p.get(key))
        .and_then(as_u32)
}

fn prop_str<'a>(o: &'a Value, key: &str) -> Option<&'a str> {
    o.pointer("/info/props")
        .and_then(|p| p.get(key))
        .and_then(Value::as_str)
}

/// A `PipeWire` number, however this build of it chose to write one.
///
/// Properties are strings on the wire and numbers in the JSON dump depending on
/// where they came from, and a negotiated rate is sometimes a range object with
/// the agreed value under `default`. Accepting all three here keeps that out of
/// every call site.
fn as_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    v.get("default").and_then(Value::as_u64)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Attribution, AudioGraph, ClockRates, DeviceHolder, GraphError, HeldDevice, NodeRole,
        RateAdvice, SinkLevels, SinkStage, SinkVolume, SourceFormat, parse, parse_clock,
        parse_sinks,
    };
    use crate::Alteration;

    /// A real `pw-dump`, taken while priel was playing a 44.1 kHz track into a
    /// USB DAC. Trimmed to the objects on the path plus one unrelated sink, and
    /// with the host and user names replaced; nothing else was touched.
    const DUMP: &str = include_str!("../tests/fixtures/pw-dump-usb-dac.json");

    /// A real `pw-dump`, trimmed to the two objects that carry the server's
    /// clock settings: the `settings` metadata it publishes live, and the core
    /// object carrying what the configuration asked for. Host and user names
    /// replaced and the unrelated properties dropped; the clock entries are
    /// verbatim, including the two different ways this build spells a rate
    /// list.
    ///
    /// Separate from [`DUMP`] rather than spliced into it: that capture was
    /// trimmed to the path before this slice existed, and a dump with no clock
    /// settings in it is itself a case worth keeping.
    const CLOCK: &str = include_str!("../tests/fixtures/pw-dump-clock-settings.json");

    /// The pid priel was running under when the fixture was captured.
    const PID: u32 = 3_124_085;

    /// **Synthetic**, not captured: a chain shaped like the real one with a
    /// loopback wedged into it, running at 48 kHz and narrowing to `S16LE` at
    /// the device. The captured dump has a clean chain, so there was nothing in
    /// it to attribute; this is what a machine with a virtual sink in the way
    /// looks like.
    ///
    /// Its sink names the card behind it but not the process that opened it,
    /// which is the case that stops the holder's name being taken from whatever
    /// client the dump happens to list first.
    const RESAMPLING: &str = include_str!("../tests/fixtures/pw-dump-resampling-loopback.json");

    /// **Synthetic**, not captured: the same shape with no loopback and the rate
    /// intact, so the only thing wrong is the width the sink settled on.
    ///
    /// It also carries a card object the sink does *not* point at, which is
    /// what makes the unknown-holder case above a real test rather than a test
    /// of an empty dump.
    const TRUNCATING: &str = include_str!("../tests/fixtures/pw-dump-truncating-sink.json");

    /// The pid the two hand-authored dumps are written for.
    const SYNTHETIC_PID: u32 = 4242;

    fn track(rate_hz: u32, bits: u32) -> SourceFormat {
        SourceFormat { rate_hz, bits }
    }

    fn path_of(dump: &str, pid: u32) -> AudioGraph {
        match parse(dump, pid) {
            Ok(g) => g,
            Err(e) => panic!("the fixture should parse: {e}"),
        }
    }

    #[test]
    fn the_chain_to_the_device_is_listed_in_order_with_what_each_node_negotiated() {
        // Goal: this is the whole point of the overlay - which nodes are in the
        // way, and what each of them is doing to the audio. Order matters: the
        // reader follows it top to bottom from priel to the hardware.
        let g = path_of(DUMP, PID);
        assert_eq!(g.path.len(), 2, "stream then device, nothing in between");

        let stream = &g.path[0];
        assert_eq!(stream.role, NodeRole::Stream);
        assert_eq!(stream.rate_hz, Some(44_100));
        assert_eq!(stream.format.as_deref(), Some("S16LE"));
        assert_eq!(stream.channels, Some(2));

        let device = &g.path[1];
        assert_eq!(device.role, NodeRole::Device);
        assert_eq!(
            device.name,
            "alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0"
        );
        assert_eq!(device.description, "SMSL USB AUDIO Pro");
        assert_eq!(device.rate_hz, Some(44_100), "the device follows the rate");
        assert_eq!(device.format.as_deref(), Some("S32LE"));
        assert_eq!(device.media_class, "Audio/Sink");
    }

    #[test]
    fn a_server_sink_carries_the_card_that_ties_it_to_a_hardware_device() {
        // Goal: this is the join that lets a refused `hw:` device fall back to
        // the same physical DAC through the sound server. The two identifiers
        // share no substring - `alsa/hw:CARD=AUDIO,DEV=0` against
        // `alsa_output.usb-SMSL_...`, so any textual heuristic would silently
        // do nothing - and the card the server publishes is the only link.
        let sinks = parse_sinks(DUMP);
        let dac = sinks
            .iter()
            .find(|s| s.card_id.as_deref() == Some("AUDIO"))
            .expect("the fixture's USB DAC should be listed");
        assert_eq!(
            dac.node_name,
            "alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0"
        );
        assert_eq!(dac.card_index, Some(2), "the index, for an `hw:2,0` device");
        assert!(
            sinks.iter().any(|s| s.card_id.as_deref() == Some("Audio")),
            "the second card is listed too, and differs only in case: {sinks:?}"
        );
    }

    #[test]
    fn nothing_is_reported_when_there_is_no_dump_to_read() {
        // Goal: a machine with no sound server has no sinks, which is an
        // answer rather than a failure - the caller falls through to the
        // default sink either way. Refusing to parse must not become an error
        // path with nothing to do about it.
        assert!(parse_sinks("").is_empty());
        assert!(parse_sinks("not json at all").is_empty());
        assert!(parse_sinks("[]").is_empty());
        assert!(
            parse_sinks(&DUMP[..DUMP.len() / 2]).is_empty(),
            "a truncated dump is no better than none"
        );
    }

    #[test]
    fn a_sink_nothing_is_linked_to_stays_off_the_path() {
        // Goal: a machine has several sinks and the overlay must show the one in
        // use, not a device list. The fixture carries a second, idle sink.
        let g = path_of(DUMP, PID);
        assert!(
            !g.path.iter().any(|n| n.name.contains("HiFi__Speaker")),
            "an unconnected sink is not part of this chain"
        );
    }

    #[test]
    fn another_processs_stream_is_not_mistaken_for_ours() {
        // Goal: the node is libmpv's and is called `mpv`, so a name match would
        // happily report a completely separate mpv. Identity is the pid.
        assert_eq!(parse(DUMP, PID + 1), Err(GraphError::NoStream));
    }

    #[test]
    fn output_that_is_not_a_dump_is_reported_rather_than_panicking() {
        // Goal: pw-dump on a machine mid-upgrade, or a wrapper script printing a
        // warning first. None of these may take the overlay down with them.
        assert_eq!(parse("", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("not json at all", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("{\"id\": 1}", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("[]", PID), Err(GraphError::NoStream));
        // A dump cut off mid-object, which is what a killed pw-dump leaves.
        let truncated = &DUMP[..DUMP.len() / 2];
        assert_eq!(parse(truncated, PID), Err(GraphError::Unreadable));
    }

    #[test]
    fn a_stream_linked_to_nothing_is_still_worth_showing() {
        // Goal: between tracks the stream can sit unlinked. Reporting only the
        // stream is a truthful answer; reporting nothing hides that priel is
        // there at all.
        let unlinked: String = DUMP.replace(
            "\"PipeWire:Interface:Link\"",
            "\"PipeWire:Interface:Other\"",
        );
        let g = path_of(&unlinked, PID);
        assert_eq!(g.path.len(), 1);
        assert_eq!(g.path[0].role, NodeRole::Stream);
    }

    #[test]
    fn a_graph_that_links_back_on_itself_terminates() {
        // Goal: a loopback can make the link graph a cycle. Following it must
        // stop rather than build a list until memory runs out.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "b"}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}},
          {"id": 5, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 3, "input-node-id": 2}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(g.path.len(), 2, "each node once, then stop");
    }

    #[test]
    fn a_node_that_has_not_negotiated_a_format_reports_the_gap_rather_than_a_zero() {
        // Goal: a suspended device has no Format param. Showing 0 Hz would read
        // as a fact; showing nothing reads as what it is.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(g.path[0].rate_hz, None);
        assert_eq!(g.path[0].format, None);
        assert_eq!(g.path[0].description, "a", "the name stands in for it");
    }

    #[test]
    fn a_hop_between_the_stream_and_the_device_is_marked_as_one() {
        // Goal: the roles are what a later slice reads to say which node
        // resamples, so a loopback in the middle must not be filed as either
        // end of the chain.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink/Virtual", "node.name": "loop"}}},
          {"id": 4, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"}}},
          {"id": 5, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}},
          {"id": 6, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 3, "input-node-id": 4}}
        ]"#;
        let g = path_of(dump, 42);
        let roles: Vec<NodeRole> = g.path.iter().map(|n| n.role).collect();
        assert_eq!(
            roles,
            vec![NodeRole::Stream, NodeRole::Intermediate, NodeRole::Device]
        );
    }

    #[test]
    fn every_failure_says_which_one_it_was_in_a_line_that_fits() {
        // Goal: the overlay draws one row per line and does not rewrap, so a
        // sentence longer than the box silently loses its tail - and the tail
        // is where the advice is.
        let all = [
            GraphError::NotInstalled,
            GraphError::Unavailable("it exited with status 1".into()),
            GraphError::TimedOut,
            GraphError::Unreadable,
            GraphError::NoStream,
            GraphError::Bypassed,
        ];
        for e in all {
            let line = e.to_string();
            assert!(line.chars().count() <= 60, "too long to draw: {line}");
            if let Some(hint) = e.hint() {
                assert!(hint.chars().count() <= 60, "too long to draw: {hint}");
            }
        }
    }

    #[test]
    fn who_alters_the_samples_is_decided_by_a_table_over_the_recorded_chains() {
        // Goal: the whole feature in one place. The badge says *that* the
        // samples were altered; this says *which node did it*, and - the case
        // that matters as much - refuses to name one when the chain does not
        // account for what was measured. Each row is (dump, pid, the track, what
        // the player graded, what the graph may say about it).
        let cases: [(&str, u32, SourceFormat, Option<Alteration>, Attribution); 12] = [
            // The captured chain carries the track's own rate end to end, and
            // widens 16 bits into a 32-bit word, which alters nothing.
            (DUMP, PID, track(44_100, 16), None, Attribution::Clean),
            // The same chain with a resample measured at the hardware. Nothing
            // on the path did it, and saying so is the point: the nearest
            // candidate is the DAC, and blaming it would be a guess.
            (
                DUMP,
                PID,
                track(44_100, 16),
                Some(Alteration::Resampled),
                Attribution::Unexplained(Alteration::Resampled),
            ),
            (
                DUMP,
                PID,
                track(44_100, 16),
                Some(Alteration::Truncated),
                Attribution::Unexplained(Alteration::Truncated),
            ),
            // A volume control is not a thing the graph publishes, so there is
            // nothing here to admit a gap about either.
            (
                DUMP,
                PID,
                track(44_100, 16),
                Some(Alteration::VolumeScaled),
                Attribution::Clean,
            ),
            (
                DUMP,
                PID,
                track(44_100, 16),
                Some(Alteration::ServerVolumeScaled),
                Attribution::Clean,
            ),
            // The loopback is the first node off the track's rate. The device
            // behind it is at 48 kHz too, and naming that one would send the
            // reader to reconfigure the wrong thing.
            (
                RESAMPLING,
                SYNTHETIC_PID,
                track(44_100, 24),
                None,
                Attribution::Node {
                    index: 1,
                    alteration: Alteration::Resampled,
                },
            ),
            (
                RESAMPLING,
                SYNTHETIC_PID,
                track(44_100, 24),
                Some(Alteration::Resampled),
                Attribution::Node {
                    index: 1,
                    alteration: Alteration::Resampled,
                },
            ),
            // That chain also narrows, at the device two hops down. The rate
            // goes first because it is the earlier divergence: everything after
            // it is already rebuilt.
            (
                RESAMPLING,
                SYNTHETIC_PID,
                track(44_100, 24),
                Some(Alteration::Truncated),
                Attribution::Node {
                    index: 1,
                    alteration: Alteration::Resampled,
                },
            ),
            // The rate holds all the way down, so only the width is left.
            (
                TRUNCATING,
                SYNTHETIC_PID,
                track(44_100, 24),
                None,
                Attribution::Node {
                    index: 1,
                    alteration: Alteration::Truncated,
                },
            ),
            // The same chain with a 16-bit track: `S16LE` carries it exactly,
            // and a chain that alters nothing accuses nobody.
            (
                TRUNCATING,
                SYNTHETIC_PID,
                track(44_100, 16),
                None,
                Attribution::Clean,
            ),
            // Nothing known about the track. "Clean" would be a claim made from
            // no data, which is the flattering answer rather than the true one.
            (DUMP, PID, track(0, 0), None, Attribution::NothingToCompare),
            (
                DUMP,
                PID,
                track(0, 0),
                Some(Alteration::Resampled),
                Attribution::NothingToCompare,
            ),
        ];

        for (dump, pid, source, observed, expected) in cases {
            let g = path_of(dump, pid);
            assert_eq!(
                g.attribute(source, observed),
                expected,
                "track {source:?} observed {observed:?}"
            );
        }
    }

    #[test]
    fn the_named_node_is_the_one_the_reader_has_to_go_and_fix() {
        // Goal: an index is only useful if it points at the node a person
        // recognises. The loopback and the device are both at 48 kHz, so an
        // off-by-one here would send them to the DAC's settings for a problem
        // a virtual sink is causing.
        let g = path_of(RESAMPLING, SYNTHETIC_PID);
        let Attribution::Node { index, .. } = g.attribute(track(44_100, 24), None) else {
            panic!("the loopback should have been named");
        };
        assert_eq!(g.path[index].description, "Studio loopback");
        assert_eq!(g.path[index].role, NodeRole::Intermediate);
    }

    #[test]
    fn a_node_that_resamples_and_narrows_at_once_is_named_for_the_worse_of_the_two() {
        // Goal: both divergences land on the same hop, and the two answers are
        // not equal - a rate change rebuilds every sample, where a narrowing
        // keeps them and drops the low bits. Reporting the milder one would
        // understate what happened.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"},
                    "params": {"Format": [{"format": "S32LE", "rate": 44100, "channels": 2}]}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"},
                    "params": {"Format": [{"format": "S16LE", "rate": 48000, "channels": 2}]}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(
            g.attribute(track(44_100, 24), None),
            Attribution::Node {
                index: 1,
                alteration: Alteration::Resampled,
            }
        );
    }

    #[test]
    fn priels_own_stream_is_named_when_it_is_the_one_that_moved() {
        // Goal: the node at the top of the chain is libmpv's, and it is not
        // above suspicion - a player told to output at a fixed rate resamples
        // before the graph ever sees the samples. Skipping the first hop would
        // report "nothing here explains it" for a cause priel owns.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"},
                    "params": {"Format": [{"format": "S32LE", "rate": 48000, "channels": 2}]}}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(
            g.attribute(track(44_100, 24), None),
            Attribution::Node {
                index: 0,
                alteration: Alteration::Resampled,
            }
        );
    }

    #[test]
    fn a_chain_that_has_negotiated_nothing_admits_it_rather_than_reporting_a_clean_bill() {
        // Goal: a suspended node publishes no Format at all. There is a
        // difference between "compared and found nothing wrong" and "had
        // nothing to compare", and collapsing them turns silence into a green
        // light.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"}}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(
            g.attribute(track(44_100, 24), None),
            Attribution::NothingToCompare
        );
        assert_eq!(
            g.attribute(track(44_100, 24), Some(Alteration::Resampled)),
            Attribution::NothingToCompare
        );
    }

    #[test]
    fn a_gap_is_admitted_per_measurement_rather_than_for_the_chain_as_a_whole() {
        // Goal: a node can publish a rate and no width, which leaves the rate
        // answerable and the width not. Treating "some of it is comparable" as
        // "all of it is" would let a truncation be reported as unexplained when
        // it was never looked at.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"},
                    "params": {"Format": [{"rate": 44100, "channels": 2}]}}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(
            g.attribute(track(44_100, 24), Some(Alteration::Resampled)),
            Attribution::Unexplained(Alteration::Resampled),
            "the rates were all there and all agreed"
        );
        assert_eq!(
            g.attribute(track(44_100, 24), Some(Alteration::Truncated)),
            Attribution::NothingToCompare,
            "no node said how wide it was, so nothing was checked"
        );
    }

    #[test]
    fn the_rates_the_server_may_clock_at_come_from_the_dump_already_being_read() {
        // Goal: the permitted list is the other half of the diagnosis, and it
        // has to come from the dump priel already runs rather than from a
        // second source of truth on disk. The live `settings` metadata is what
        // the server is using *now*, which is the question being asked.
        let clock = parse_clock(CLOCK);
        assert_eq!(
            clock.allowed_hz.as_deref(),
            Some(
                [
                    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600,
                    768_000
                ]
                .as_slice()
            )
        );
        assert_eq!(
            clock.current_hz,
            Some(48_000),
            "clock.rate, what it sits at"
        );
        assert_eq!(
            clock.forced_hz, None,
            "a force-rate of zero is not pinned, not pinned to zero"
        );
    }

    #[test]
    fn a_dump_that_says_nothing_about_the_clock_is_unknown_rather_than_empty() {
        // Goal: the discipline this whole slice turns on. An empty list is a
        // finding - the server may not switch rates at all - and no list is the
        // absence of one. Reporting the second as the first would advise a
        // change from data that was never there.
        let unknown = ClockRates::default();
        assert_eq!(parse_clock(DUMP), unknown, "trimmed to the path, no clock");
        assert_eq!(parse_clock(""), unknown);
        assert_eq!(parse_clock("not json at all"), unknown);
        assert_eq!(parse_clock("[]"), unknown);
        assert_eq!(
            parse_clock(&CLOCK[..CLOCK.len() / 2]),
            unknown,
            "a truncated dump is no better than none"
        );
        assert!(unknown.allowed_hz.is_none(), "unknown, not an empty list");

        // A value that is neither text nor an array is something priel does not
        // understand, and reading it as an empty list would turn a spelling it
        // has never seen into the finding that the server may not switch rates.
        let odd = r#"[
          {"id": 32, "type": "PipeWire:Interface:Metadata",
           "props": {"metadata.name": "settings"},
           "metadata": [
             {"subject": 0, "key": "clock.allowed-rates", "type": "", "value": true}
           ]}
        ]"#;
        assert!(parse_clock(odd).allowed_hz.is_none(), "unread, not empty");
    }

    #[test]
    fn a_server_that_may_not_switch_rates_at_all_says_so_rather_than_going_quiet() {
        // Goal: an empty `clock.allowed-rates` is the default on a great many
        // machines and is exactly the setup this feature exists to explain. It
        // must parse as a known, empty list - the one case where every track
        // off the server's own rate is resampled.
        let dump = r#"[
          {"id": 32, "type": "PipeWire:Interface:Metadata",
           "props": {"metadata.name": "settings"},
           "metadata": [
             {"subject": 0, "key": "clock.rate", "type": "", "value": 48000},
             {"subject": 0, "key": "clock.allowed-rates", "type": "", "value": "[ ]"}
           ]}
        ]"#;
        let clock = parse_clock(dump);
        assert_eq!(clock.allowed_hz.as_deref(), Some([].as_slice()));
        assert_eq!(clock.current_hz, Some(48_000));
    }

    #[test]
    fn what_the_server_is_running_now_wins_over_what_its_configuration_asked_for() {
        // Goal: the metadata can be changed at runtime and the core object
        // still carries what the file said. Reading the file's answer while the
        // server runs on another would advise a change that is already made.
        let dump = r#"[
          {"id": 0, "type": "PipeWire:Interface:Core",
           "info": {"props": {"default.clock.allowed-rates": "[ \"48000\" ]",
                              "default.clock.rate": 48000}}},
          {"id": 32, "type": "PipeWire:Interface:Metadata",
           "props": {"metadata.name": "settings"},
           "metadata": [
             {"subject": 0, "key": "clock.rate", "type": "", "value": 44100},
             {"subject": 0, "key": "clock.allowed-rates", "type": "",
              "value": "[ 44100, 48000 ]"}
           ]}
        ]"#;
        let clock = parse_clock(dump);
        assert_eq!(
            clock.allowed_hz.as_deref(),
            Some([44_100, 48_000].as_slice())
        );
        assert_eq!(clock.current_hz, Some(44_100));
    }

    #[test]
    fn the_configured_list_is_read_when_the_server_publishes_no_live_one() {
        // Goal: not every build publishes a `settings` metadata, and the core
        // object carries the configured list in a different spelling again -
        // quoted strings across several lines. Both are the same list.
        let dump = r#"[
          {"id": 0, "type": "PipeWire:Interface:Core",
           "info": {"props": {
             "default.clock.allowed-rates": "[\n  \"44100\",\n  \"96000\"\n]",
             "default.clock.rate": 48000}}}
        ]"#;
        let clock = parse_clock(dump);
        assert_eq!(
            clock.allowed_hz.as_deref(),
            Some([44_100, 96_000].as_slice())
        );
        assert_eq!(clock.current_hz, Some(48_000));
    }

    #[test]
    fn a_server_pinned_to_one_rate_is_reported_as_pinned_and_not_as_a_list() {
        // Goal: `clock.force-rate` overrides the permitted list outright. A
        // machine with it set would otherwise be told to add a rate to a list
        // the server is no longer consulting, which is a change that does
        // nothing - the exact wrong-explanation failure this slice guards.
        let dump = r#"[
          {"id": 32, "type": "PipeWire:Interface:Metadata",
           "props": {"metadata.name": "settings"},
           "metadata": [
             {"subject": 0, "key": "clock.rate", "type": "", "value": 48000},
             {"subject": 0, "key": "clock.allowed-rates", "type": "",
              "value": "[ 44100, 48000 ]"},
             {"subject": 0, "key": "clock.force-rate", "type": "", "value": 48000}
           ]}
        ]"#;
        let clock = parse_clock(dump);
        assert_eq!(clock.forced_hz, Some(48_000));
        assert_eq!(
            clock.allowed_hz.as_deref(),
            Some([44_100, 48_000].as_slice()),
            "the list is still reported; it is simply not what decides"
        );
    }

    #[test]
    fn a_metadata_that_is_not_the_settings_one_is_not_read_as_it() {
        // Goal: a session carries several metadata objects - default sinks,
        // route settings, a schema - and they share the entry shape. Only the
        // one named `settings` holds the clock.
        let dump = r#"[
          {"id": 30, "type": "PipeWire:Interface:Metadata",
           "props": {"metadata.name": "default"},
           "metadata": [
             {"subject": 0, "key": "clock.allowed-rates", "type": "", "value": "[ 96000 ]"}
           ]}
        ]"#;
        assert_eq!(parse_clock(dump), ClockRates::default());
    }

    #[test]
    fn the_graph_carries_the_clock_settings_read_from_the_same_dump() {
        // Goal: one `pw-dump` answers both questions, so the overlay cannot
        // show a chain from one moment and a setting from another.
        let g = path_of(DUMP, PID);
        assert_eq!(g.clock, ClockRates::default(), "this capture carries none");
    }

    fn clock(allowed_hz: Option<&[u32]>, current_hz: Option<u32>) -> ClockRates {
        ClockRates {
            allowed_hz: allowed_hz.map(<[u32]>::to_vec),
            current_hz,
            forced_hz: None,
        }
    }

    fn pinned_at(at_hz: u32, allowed_hz: Option<&[u32]>) -> ClockRates {
        ClockRates {
            forced_hz: Some(at_hz),
            ..clock(allowed_hz, Some(48_000))
        }
    }

    #[test]
    fn whether_the_track_may_have_its_own_rate_is_decided_by_a_table() {
        // Goal: the whole of the advice in one place. A rate the server is not
        // permitted to use is resampled before any node on the path sees a
        // sample, which is why the chain can diverge nowhere and something
        // still moved. Each row is (what the server published, the track's
        // rate, what to say about it).
        let missing = |proposed: &[u32]| RateAdvice::Missing {
            proposed_hz: proposed.to_vec(),
        };
        let cases: [(ClockRates, u32, RateAdvice); 13] = [
            // The rate is on the list. Nothing to change, so nothing is said.
            (
                clock(Some(&[44_100, 48_000]), Some(48_000)),
                44_100,
                RateAdvice::Permitted,
            ),
            // It is not, and the list that would include it keeps every rate
            // that was already there - a proposal that dropped one would take
            // away a rate something else on the machine is using.
            (
                clock(Some(&[44_100, 48_000]), Some(48_000)),
                96_000,
                missing(&[44_100, 48_000, 96_000]),
            ),
            // Ascending however the server published it, and never twice.
            (
                clock(Some(&[48_000, 44_100]), Some(48_000)),
                88_200,
                missing(&[44_100, 48_000, 88_200]),
            ),
            // A hand-edited list can name the same rate twice, and a proposal
            // that repeated it back would look like priel made the mess.
            (
                clock(Some(&[48_000, 48_000]), Some(48_000)),
                44_100,
                missing(&[44_100, 48_000]),
            ),
            // An empty list means the server does not switch at all: it stays
            // on the one rate it is clocked at, which is a permitted rate.
            (
                clock(Some(&[]), Some(44_100)),
                44_100,
                RateAdvice::Permitted,
            ),
            // The same setup with a track it cannot follow. The rate it sits on
            // has to survive the proposal, or the change fixes this track by
            // breaking everything else.
            (
                clock(Some(&[]), Some(48_000)),
                44_100,
                missing(&[44_100, 48_000]),
            ),
            // An empty list and no rate published either. Nothing is known, so
            // nothing is claimed.
            (clock(Some(&[]), None), 44_100, RateAdvice::Unknown),
            // No list at all. Distinct from an empty one, and the answer is the
            // admitted gap rather than the flattering guess.
            (clock(None, Some(48_000)), 44_100, RateAdvice::Unknown),
            (ClockRates::default(), 44_100, RateAdvice::Unknown),
            // Nothing playing, or a track whose rate is not known yet. There is
            // no question to answer, whatever the server says.
            (clock(Some(&[48_000]), Some(48_000)), 0, RateAdvice::NoTrack),
            (ClockRates::default(), 0, RateAdvice::NoTrack),
            // A pin overrides the list outright, so the advice must be to clear
            // the pin. Telling the reader to extend a list the server has
            // stopped consulting is a change that does nothing.
            (
                pinned_at(48_000, Some(&[44_100, 48_000])),
                44_100,
                RateAdvice::Pinned { at_hz: 48_000 },
            ),
            // Pinned to the rate the track wants, which is the one setup where
            // a pin is doing no harm.
            (
                pinned_at(44_100, Some(&[48_000])),
                44_100,
                RateAdvice::Permitted,
            ),
        ];

        for (clock, track_rate_hz, expected) in cases {
            assert_eq!(
                clock.advise(track_rate_hz, &[]),
                expected,
                "{clock:?} against {track_rate_hz} Hz"
            );
        }
    }

    #[test]
    fn a_rate_the_device_cannot_do_is_not_a_config_the_reader_should_change() {
        // Goal: the difference the descriptor buys. A rate above what the
        // hardware does is resampled whatever the server allows, so proposing a
        // file edit there would send the reader to fix something that fixes
        // nothing. The ceiling is what the device tops out at.
        let clock = clock(Some(&[44_100, 48_000, 96_000]), Some(48_000));
        let supported = [44_100, 48_000, 96_000];
        assert_eq!(
            clock.advise(192_000, &supported),
            RateAdvice::Unsupported { ceiling_hz: 96_000 },
            "192 kHz is above the device's 96 kHz ceiling"
        );
        let text = clock.advise(192_000, &supported).lines().join("\n");
        assert!(text.contains("96 kHz"), "names the ceiling: {text}");
        assert!(
            !text.contains("allowed-rates"),
            "and does not send the reader to change a setting: {text}"
        );
    }

    #[test]
    fn a_rate_the_device_can_do_but_the_server_blocks_is_still_a_config_to_add() {
        // Goal: the fixable case must survive the new check. The device does
        // 88.2 kHz and the server is not permitted to use it, so the advice is
        // still to add it - the descriptor only rules out what the hardware
        // truly cannot do.
        assert_eq!(
            clock(Some(&[44_100, 48_000]), Some(48_000))
                .advise(88_200, &[44_100, 48_000, 88_200, 96_000]),
            RateAdvice::Missing {
                proposed_hz: vec![44_100, 48_000, 88_200]
            }
        );
    }

    #[test]
    fn no_capability_claim_is_made_when_the_devices_rates_are_not_known() {
        // Goal: an empty supported set is "not known", not "the device can do
        // none". The advice must fall back to what it said before the descriptor
        // existed rather than calling every rate unsupported.
        assert_eq!(
            clock(Some(&[48_000]), Some(48_000)).advise(44_100, &[]),
            RateAdvice::Missing {
                proposed_hz: vec![44_100, 48_000]
            },
            "unknown rates propose the change, they do not refuse it"
        );
    }

    #[test]
    fn the_rates_a_device_can_do_that_the_server_blocks_are_the_set_to_add() {
        // Goal: the "set up audio" check - S minus A. Only what the device
        // supports and the server does not permit, and nothing when either side
        // is unknown, because a difference invented from a missing side would
        // point at a setting that was never the problem.
        assert_eq!(
            super::blocked_rates(Some(&[44_100, 48_000]), &[44_100, 48_000, 88_200, 96_000]),
            vec![88_200, 96_000],
            "the supported rates the server is not allowing"
        );
        assert!(
            super::blocked_rates(None, &[44_100, 48_000]).is_empty(),
            "no permitted list means nothing is claimed"
        );
        assert!(
            super::blocked_rates(Some(&[44_100, 48_000]), &[]).is_empty(),
            "unknown device rates claim nothing either"
        );
    }

    #[test]
    fn the_sink_card_index_is_read_from_the_held_pcm() {
        // Goal: the join from the graph to /proc/asound. A server-held ALSA
        // device names its PCM as hw:N,M, and N is the card its rates are read
        // from; a holder that names no ALSA card yields nothing to read.
        let held = AudioGraph {
            holder: DeviceHolder::Server(HeldDevice {
                sink: "DAC".into(),
                opened_by: None,
                pcm: Some("hw:2,0".into()),
                card_name: None,
            }),
            ..AudioGraph::default()
        };
        assert_eq!(held.sink_card_index(), Some(2));
        assert_eq!(
            AudioGraph {
                holder: DeviceHolder::NoDevice,
                ..AudioGraph::default()
            }
            .sink_card_index(),
            None
        );
    }

    #[test]
    fn what_the_server_may_clock_at_reads_the_pin_and_the_empty_list_the_same_way_the_advice_does()
    {
        // Goal: the readout and the advice must not disagree about one server.
        // Listing every allowed rate while the advice says the graph is pinned
        // to one of them would read as two opinions.
        assert_eq!(
            pinned_at(48_000, Some(&[44_100, 48_000])).permitted_hz(),
            Some(vec![48_000]),
            "a pin is the whole of what it may use"
        );
        assert_eq!(
            clock(Some(&[]), Some(44_100)).permitted_hz(),
            Some(vec![44_100]),
            "an empty list is the one rate it is clocked at"
        );
        assert_eq!(clock(Some(&[]), None).permitted_hz(), None);
        assert_eq!(clock(None, Some(48_000)).permitted_hz(), None);
        assert_eq!(
            clock(Some(&[44_100, 48_000]), Some(48_000)).permitted_hz(),
            Some(vec![44_100, 48_000])
        );
    }

    #[test]
    fn a_permitted_rate_is_told_to_change_nothing() {
        // Goal: advice on a working setup teaches the reader to ignore it. The
        // three answers that are not a finding all say nothing at all.
        for advice in [
            RateAdvice::Permitted,
            RateAdvice::Unknown,
            RateAdvice::NoTrack,
        ] {
            assert!(
                advice.lines().is_empty(),
                "{advice:?} has nothing to advise"
            );
        }
    }

    #[test]
    fn the_change_is_quoted_whole_with_where_it_goes_and_that_it_needs_a_restart() {
        // Goal: the three things that make the advice actionable rather than
        // merely correct - the exact text, the file it belongs in, and the fact
        // that the server has to be restarted before any of it applies.
        let advice = ClockRates {
            allowed_hz: Some(vec![48_000]),
            current_hz: Some(48_000),
            forced_hz: None,
        }
        .advise(44_100, &[]);
        let lines = advice.lines();
        let text = lines.join("\n");
        assert!(
            text.contains("default.clock.allowed-rates = [ 44100 48000 ]"),
            "the whole setting, copyable: {text}"
        );
        assert!(
            text.contains("context.properties = {"),
            "and the section it has to sit in, or pasting it does nothing: {text}"
        );
        assert!(
            text.contains("~/.config/pipewire/pipewire.conf.d/"),
            "which file it goes in: {text}"
        );
        assert!(
            text.to_lowercase().contains("restart"),
            "and that it takes a restart: {text}"
        );
    }

    #[test]
    fn a_pinned_server_is_told_to_clear_the_pin_rather_than_to_extend_the_list() {
        // Goal: the wrong-explanation case. `clock.force-rate` is set at
        // runtime and has no spelling in the configuration file, so advising a
        // file edit here would be a change that cannot work.
        let lines = pinned_at(48_000, Some(&[44_100, 48_000]))
            .advise(44_100, &[])
            .lines();
        let text = lines.join("\n");
        assert!(text.contains("clock.force-rate"), "names the pin: {text}");
        assert!(
            !text.contains("allowed-rates"),
            "and does not send the reader to the list: {text}"
        );
        assert!(
            !text.to_lowercase().contains("restart"),
            "clearing it applies at once: {text}"
        );
    }

    #[test]
    fn every_line_of_advice_fits_the_box_it_is_drawn_in() {
        // Goal: the overlay draws one row per line and does not rewrap, so a
        // line longer than the box loses its tail - and a configuration line
        // with its tail clipped is worse than no advice at all, because it
        // still looks like something that can be copied.
        let long = [
            44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600,
        ];
        let advice = clock(Some(&long), Some(48_000)).advise(768_000, &[]);
        let lines = advice.lines();
        for line in &lines {
            assert!(line.chars().count() <= 70, "too long to draw: {line}");
        }
        let text = lines.join("\n");
        for rate in long.iter().chain(std::iter::once(&768_000)) {
            assert!(
                text.contains(&rate.to_string()),
                "{rate} was dropped from the wrapped list: {text}"
            );
        }
        assert!(
            text.contains("default.clock.allowed-rates = ["),
            "still the same setting, just spread over lines: {text}"
        );
    }

    // ---- what has the output device open ----

    /// **Synthetic**, not captured: priel's stream with no link leaving it, so
    /// the chain reaches no device at all. A capture of that would be a capture
    /// of a moment too short to catch by hand.
    const UNLINKED: &str = include_str!("../tests/fixtures/pw-dump-unlinked-stream.json");

    #[test]
    fn the_sound_server_is_named_as_what_has_the_output_device_open() {
        // Goal: the first half of the question - what holds the device now.
        // Every part of the answer is read from the dump: the sink the samples
        // end at, the process that opened it, the PCM behind it, and the card
        // object, which is the only thing a reservation rule can match on.
        let g = path_of(DUMP, PID);
        let held = match &g.holder {
            DeviceHolder::Server(held) => held,
            other => panic!("the server has this card open: {other:?}"),
        };
        assert_eq!(held.sink, "SMSL USB AUDIO Pro");
        assert_eq!(
            held.opened_by.as_deref(),
            Some("wireplumber"),
            "the session manager is what actually opened the PCM"
        );
        assert_eq!(held.pcm.as_deref(), Some("hw:2,0"));
        assert_eq!(
            held.card_name.as_deref(),
            Some("alsa_card.usb-SMSL_SMSL_USB_AUDIO-00")
        );
    }

    #[test]
    fn a_chain_ending_on_a_sink_with_no_device_behind_it_says_unknown() {
        // Goal: the honesty case, and the mutation it exists to kill. The sink
        // in this dump names no device object, and there *is* a card in the
        // same dump that carries the same ALSA index and the same id - so any
        // code that reaches for the likeliest candidate rather than the one the
        // sink points at will name it, and this test is what stops that being
        // mistaken for an answer.
        assert_eq!(
            path_of(TRUNCATING, SYNTHETIC_PID).holder,
            DeviceHolder::Unknown {
                sink: "Example DAC Analog Stereo".to_string()
            }
        );
    }

    #[test]
    fn a_sink_the_dump_says_nothing_about_the_opener_of_names_nobody() {
        // Goal: the second guess this could make. The card behind this sink is
        // named, so the holder is known - but nothing in the dump says which
        // process opened the node, and the only client in it is priel's own.
        // Reaching for that one would put priel's name on a device the sound
        // server is holding, which is the opposite of what the section says.
        assert_eq!(
            path_of(RESAMPLING, SYNTHETIC_PID).holder,
            DeviceHolder::Server(HeldDevice {
                sink: "Example DAC Analog Stereo".to_string(),
                opened_by: None,
                pcm: Some("hw:3,0".to_string()),
                card_name: Some("alsa_card.usb-Example_DAC-00".to_string()),
            })
        );
    }

    #[test]
    fn a_stream_that_reaches_no_device_is_not_reported_as_unknown() {
        // Goal: "nothing is at the end of this chain" and "something is and
        // priel cannot tell what" are different findings. Collapsing them would
        // put an unanswerable question on screen where there is no question.
        assert_eq!(
            path_of(UNLINKED, SYNTHETIC_PID).holder,
            DeviceHolder::NoDevice
        );
    }

    #[test]
    fn reserving_the_card_names_the_card_the_file_and_what_it_costs() {
        // Goal: the second half of the question - what it would take to hand
        // the device over. All three parts have to be there: the change, the
        // file it goes in, and the thing that is given up by making it.
        let text = path_of(DUMP, PID).holder.lines().join("\n");
        assert!(
            text.contains("wireplumber.conf.d"),
            "the file the rule goes in: {text}"
        );
        assert!(
            text.contains("alsa_card.usb-SMSL_SMSL_USB_AUDIO-00"),
            "the card the rule matches: {text}"
        );
        assert!(
            text.contains("device.disabled = true"),
            "the change itself: {text}"
        );
        assert!(
            text.to_lowercase().contains("restart"),
            "a rule in a file does nothing until the server is restarted: {text}"
        );
        assert!(
            text.contains("Nothing else on this machine"),
            "what is given up: {text}"
        );
    }

    #[test]
    fn a_card_the_dump_did_not_name_gets_no_rule_to_copy() {
        // Goal: a rule matching a card name priel invented would disable
        // something else, or nothing at all. Knowing the server has the device
        // and not knowing which card it is are two separate facts, and only the
        // second one silences the advice.
        let held = DeviceHolder::Server(HeldDevice {
            sink: "Example DAC".to_string(),
            opened_by: None,
            pcm: None,
            card_name: None,
        });
        let text = held.lines().join("\n");
        assert!(
            text.contains("has this device open"),
            "the holder is still named: {text}"
        );
        assert!(
            !text.contains("monitor.alsa.rules"),
            "no rule can be written without the card: {text}"
        );
    }

    #[test]
    fn only_a_device_the_server_holds_is_advised_about() {
        // Goal: advice printed where there is nothing to do teaches the reader
        // to ignore it. A device priel holds itself is the destination, not a
        // problem, and an unknown holder cannot be advised about at all.
        assert!(
            DeviceHolder::Direct {
                device: "alsa/hw:2,0".to_string()
            }
            .lines()
            .is_empty(),
            "priel already has it; there is nothing to hand over"
        );
        assert!(DeviceHolder::NoDevice.lines().is_empty());
        assert!(
            DeviceHolder::Unknown {
                sink: "Example DAC".to_string()
            }
            .lines()
            .is_empty()
        );
    }

    #[test]
    fn every_line_of_the_reservation_advice_fits_the_box_it_is_drawn_in() {
        // Goal: the same rule the rate advice follows. A configuration line
        // with its tail clipped still looks copyable and is not, and a card
        // name is the longest thing in this one.
        let held = DeviceHolder::Server(HeldDevice {
            sink: "Some Interface".to_string(),
            opened_by: Some("wireplumber".to_string()),
            pcm: Some("hw:3,0".to_string()),
            card_name: Some("alsa_card.usb-Example_Audio_Interface-00".to_string()),
        });
        let lines = held.lines();
        for line in &lines {
            assert!(line.chars().count() <= 70, "too long to draw: {line}");
        }
        assert!(
            lines
                .join("\n")
                .contains("alsa_card.usb-Example_Audio_Interface-00"),
            "the name survived the layout: {lines:?}"
        );
    }

    // ---- the sink's own volume ----

    fn levels(set: &[f64], software: &[f64]) -> SinkVolume {
        SinkVolume::Read(SinkLevels {
            set: set.to_vec(),
            software: software.to_vec(),
            silenced: false,
        })
    }

    #[test]
    fn the_sinks_volume_is_read_from_the_same_dump_as_the_chain() {
        // Goal: the fourth thing the dump carries, and the one this whole slice
        // exists for. The captured sink is set to 2.7% and its software stage
        // is at unity, which is the reading that has to survive parsing intact
        // - collapsing the two fields into one number is the mistake the whole
        // feature is written against.
        let g = path_of(DUMP, PID);
        assert_eq!(
            g.volume,
            SinkVolume::Read(SinkLevels {
                set: vec![0.027_001, 0.027_001],
                software: vec![1.0, 1.0],
                silenced: false,
            })
        );
    }

    #[test]
    fn a_chain_with_no_device_on_it_has_no_sink_volume_rather_than_an_unread_one() {
        // Goal: the distinction the badge is graded on. A stage that cannot
        // exist is fully evidenced; only a stage that is there and could not be
        // read earns a question mark. A stream linked to nothing reaches no
        // sink at all.
        let unlinked: String = DUMP.replace(
            "\"PipeWire:Interface:Link\"",
            "\"PipeWire:Interface:Other\"",
        );
        let g = path_of(&unlinked, PID);
        assert_eq!(g.volume, SinkVolume::Absent);
        assert_eq!(g.volume.stage(), SinkStage::Absent);
    }

    #[test]
    fn a_sink_that_publishes_no_volume_is_admitted_rather_than_assumed_to_be_at_unity() {
        // Goal: "say unknown rather than guess". The synthetic chains carry no
        // Props at all, which is what an older server - or a node that has not
        // published one yet - looks like. Reading that as unity would hand out
        // a clean tick from data that was never there.
        let g = path_of(RESAMPLING, SYNTHETIC_PID);
        assert_eq!(g.volume, SinkVolume::Unread);
        assert_eq!(g.volume.stage(), SinkStage::Unread);
    }

    #[test]
    fn a_sink_attenuating_in_software_is_read_from_the_field_that_says_so() {
        // Goal: the finding this issue exists to make. `softVolumes` is the
        // only field that says the server is multiplying our samples, and a
        // sink with no hardware mixer mirrors the control onto it.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"},
                    "params": {"Props": [
                      {"device": "hw:1,0"},
                      {"volume": 1.0, "mute": false,
                       "channelVolumes": [0.5, 0.5], "softMute": false,
                       "softVolumes": [0.5, 0.5]}]}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(g.volume, levels(&[0.5, 0.5], &[0.5, 0.5]));
        assert_eq!(g.volume.stage(), SinkStage::InSoftware { gain: 0.5 });
    }

    #[test]
    fn what_one_volume_stage_is_doing_is_decided_by_a_table() {
        // Goal: the per-stage verdict, pure and in one place, the way
        // `attribute` and `advise` already are. Five answers, and the two that
        // are not findings are the ones that matter: a stage that cannot exist
        // is not a stage that went unread, and a control set away from unity
        // that the server is *not* applying has gone somewhere this dump does
        // not show.
        let cases: [(SinkVolume, SinkStage); 10] = [
            (SinkVolume::Absent, SinkStage::Absent),
            (SinkVolume::Unread, SinkStage::Unread),
            // Nothing set anywhere: the one arm that is a clean bill.
            (levels(&[1.0, 1.0], &[1.0, 1.0]), SinkStage::Unity),
            // The server is multiplying. Costs resolution, whatever the control
            // says it was set to.
            (
                levels(&[0.5, 0.5], &[0.5, 0.5]),
                SinkStage::InSoftware { gain: 0.5 },
            ),
            // Above unity multiplies every sample too; it is not a clean pass.
            (
                levels(&[1.0, 1.0], &[1.5, 1.5]),
                SinkStage::InSoftware { gain: 1.5 },
            ),
            // Channels that disagree are graded on the worst of them, which is
            // the one that loses the most bits.
            (
                levels(&[1.0, 0.25], &[1.0, 0.25]),
                SinkStage::InSoftware { gain: 0.25 },
            ),
            // One channel turned down and one turned up. Only the turned-down
            // one costs resolution, so it is the one the verdict is made on -
            // picking whichever is further from unity would report the louder
            // channel and quote no loss at all.
            (
                levels(&[0.5, 2.0], &[0.5, 2.0]),
                SinkStage::InSoftware { gain: 0.5 },
            ),
            // The measured machine: set to 2.7% with the software stage at
            // unity. The server is not touching the samples and priel cannot
            // see what is.
            (
                levels(&[0.027_001, 0.027_001], &[1.0, 1.0]),
                SinkStage::Elsewhere { set: 0.027_001 },
            ),
            // Silence is not a fidelity finding with a decibel figure attached;
            // it is its own answer, and it keeps -inf off the screen.
            (
                SinkVolume::Read(SinkLevels {
                    set: vec![0.0, 0.0],
                    software: vec![0.0, 0.0],
                    silenced: true,
                }),
                SinkStage::Silenced,
            ),
            // Muted with the volumes left at unity: still silence.
            (
                SinkVolume::Read(SinkLevels {
                    set: vec![1.0, 1.0],
                    software: vec![1.0, 1.0],
                    silenced: true,
                }),
                SinkStage::Silenced,
            ),
        ];
        for (reading, expected) in cases {
            assert_eq!(reading.stage(), expected, "reading {reading:?}");
        }
    }

    #[test]
    fn a_volume_with_no_channels_on_it_says_nothing_rather_than_unity() {
        // Goal: an empty list is not a reading. Averaging or defaulting an
        // absent channel list to 1.0 is exactly the guess the indicator may not
        // make.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"},
                    "params": {"Props": [
                      {"channelVolumes": [], "softVolumes": []}]}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}}
        ]"#;
        assert_eq!(path_of(dump, 42).volume, SinkVolume::Unread);
    }

    #[test]
    fn a_control_read_without_the_field_that_says_where_it_lands_is_no_reading_at_all() {
        // Goal: the control alone cannot be graded - it says what was set, not
        // whether the samples moved. Without `softVolumes` there is no verdict
        // to reach, and inventing one from `channelVolumes` is the
        // overstatement this issue is about.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "mpv"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"},
                    "params": {"Props": [{"channelVolumes": [0.5, 0.5]}]}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}}
        ]"#;
        assert_eq!(path_of(dump, 42).volume, SinkVolume::Unread);
    }

    #[test]
    fn a_level_is_reported_in_decibels_and_in_the_bits_it_costs() {
        // Goal: the figure that answers "how much did I lose", by the
        // one-bit-per-6-dB rule the README already records. 0.027 linear is
        // about -31 dB, which is about five bits - and nothing says so today.
        assert!((SinkStage::db(0.027_001) + 31.37).abs() < 0.01);
        assert_eq!(SinkStage::bits_lost(0.027_001), 5);
        assert_eq!(SinkStage::bits_lost(0.5), 1, "half is one bit");
        assert_eq!(SinkStage::bits_lost(1.0), 0, "unity costs nothing");
        assert_eq!(
            SinkStage::bits_lost(1.5),
            0,
            "gain above unity costs no resolution, whatever else it risks"
        );
        assert_eq!(SinkStage::bits_lost(0.0), 0, "silence is not a bit count");
    }

    #[test]
    #[ignore = "runs pw-dump against whatever PipeWire session the machine has"]
    fn probing_a_live_session_never_panics() {
        let _ = super::probe();
    }
}
