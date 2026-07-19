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

//! What the hardware is *actually* doing, read from `/proc/asound`.
//!
//! mpv can only report the format it negotiated with the audio server. That is
//! not the same thing: `PipeWire` will accept a 44.1 kHz stream, tell mpv it got
//! 44.1 kHz, and resample it into a 48 kHz graph. The only place the truth is
//! visible without talking to `PipeWire` is the ALSA device itself, which
//! publishes its live parameters while a substream is open.
//!
//! Linux-only by nature. Everywhere else this reports nothing, and the caller
//! falls back to what the audio server said.

/// Live parameters of an open ALSA playback substream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HwParams {
    /// Card id from `/proc/asound/cardN/id`, e.g. `AUDIO`.
    pub card: String,
    /// Sample rate the device is clocked at, in Hz.
    pub rate: u32,
    /// ALSA sample format, e.g. `S32_LE`.
    pub format: String,
    pub channels: u32,
}

/// Parse the body of a `hw_params` file.
///
/// Returns `None` for the `closed` placeholder ALSA writes when nothing has the
/// device open, and for anything missing a rate or format - a half-populated
/// readout is worse than none, because the indicator would draw conclusions
/// from it.
#[must_use]
pub fn parse_hw_params(body: &str) -> Option<HwParams> {
    let mut params = HwParams::default();
    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            // "rate: 48000 (48000/1)" - the first field is the plain rate.
            "rate" => {
                params.rate = value
                    .split_whitespace()
                    .next()
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(0);
            }
            "format" => value.clone_into(&mut params.format),
            "channels" => params.channels = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if params.rate == 0 || params.format.is_empty() {
        return None;
    }
    Some(params)
}

/// Find an open ALSA playback substream and read its live parameters.
///
/// Scans every card for a playback substream that is not `closed`. While priel
/// is playing there is normally exactly one, which is the chain in use. `hint`
/// is matched against the card id so an explicit `--device` wins when several
/// devices are open at once; without it the first open substream is used.
#[must_use]
pub fn probe(hint: Option<&str>) -> Option<HwParams> {
    let mut first = None;
    for card in cards() {
        let Some(id) = read_trimmed(&format!("{card}/id")) else {
            continue;
        };
        for path in substreams(&card) {
            let Some(body) = std::fs::read_to_string(&path).ok() else {
                continue;
            };
            let Some(mut params) = parse_hw_params(&body) else {
                continue;
            };
            params.card.clone_from(&id);
            // A hint that names this card settles it; otherwise remember the
            // first and keep looking for a better match.
            if hint.is_some_and(|h| h.contains(&id)) {
                return Some(params);
            }
            if first.is_none() {
                first = Some(params);
            }
        }
    }
    first
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// `/proc/asound/cardN` directories.
fn cards() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/asound") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            // `card0`, not the `Generic -> card1` symlinks, which would double
            // every device up.
            let is_card = name.starts_with("card") && name[4..].parse::<u32>().is_ok();
            is_card.then(|| path.to_str().map(str::to_string)).flatten()
        })
        .collect()
}

/// `hw_params` paths under a card's playback PCMs.
fn substreams(card: &str) -> Vec<String> {
    let Ok(pcms) = std::fs::read_dir(card) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pcm in pcms.flatten() {
        let path = pcm.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // `pcm0p` is playback; `pcm0c` is capture and irrelevant here.
        if !name.starts_with("pcm") || !name.ends_with('p') {
            continue;
        }
        let Ok(subs) = std::fs::read_dir(&path) else {
            continue;
        };
        for sub in subs.flatten() {
            let sub = sub.path();
            if sub
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("sub"))
                && let Some(p) = sub.join("hw_params").to_str()
            {
                out.push(p.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_hw_params, probe};

    const OPEN: &str = "access: MMAP_INTERLEAVED
format: S32_LE
subformat: STD
channels: 2
rate: 48000 (48000/1)
period_size: 256
buffer_size: 32768";

    #[test]
    fn an_open_substream_yields_its_live_parameters() {
        // Goal: this readout is the only unmediated view of the hardware, so the
        // three fields the indicator depends on must all survive parsing.
        let p = parse_hw_params(OPEN).expect("an open device should parse");
        assert_eq!(
            p.rate, 48_000,
            "the plain rate, not the `(48000/1)` fraction"
        );
        assert_eq!(p.format, "S32_LE");
        assert_eq!(p.channels, 2);
    }

    #[test]
    fn a_closed_device_reports_nothing_rather_than_zeroes() {
        // Goal: ALSA writes the literal word `closed`. Parsing that into a
        // zero-rate reading would make the indicator claim a resample.
        assert!(parse_hw_params("closed").is_none());
        assert!(parse_hw_params("").is_none());
    }

    #[test]
    fn a_half_written_readout_is_rejected() {
        // Goal: /proc is read while the device is being set up, so a partial
        // file is normal. Drawing conclusions from half of one is not.
        assert!(
            parse_hw_params("channels: 2").is_none(),
            "no rate or format"
        );
        assert!(
            parse_hw_params("rate: 44100 (44100/1)").is_none(),
            "no format"
        );
        assert!(parse_hw_params("format: S16_LE").is_none(), "no rate");
    }

    #[test]
    fn unexpected_lines_are_ignored_rather_than_fatal() {
        // Goal: the file gains fields across kernel versions; unknown keys must
        // not break a readout that has everything needed.
        let body = "format: S24_3LE\nrate: 96000 (96000/1)\nchannels: 2\nsomething_new: 1\ngarbage";
        let p = parse_hw_params(body).expect("should still parse");
        assert_eq!(p.format, "S24_3LE");
        assert_eq!(p.rate, 96_000);
    }

    #[test]
    fn probing_never_panics_whatever_the_machine_looks_like() {
        // Goal: this walks /proc on whatever host the suite runs on - a
        // container with no sound card, a machine with several. Returning
        // nothing is a valid answer; failing is not.
        let _ = probe(None);
        let _ = probe(Some("AUDIO"));
    }
}
