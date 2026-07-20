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

/// Does this output device identifier name this card?
///
/// Three spellings reach here, and a plain substring test only answers the
/// first:
///
/// - `pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0`, which
///   carries the card name inside a longer string;
/// - `alsa/hw:CARD=AUDIO,DEV=0` and every plugin spelling of it, which name the
///   card outright - and are then compared exactly, because a machine can have
///   both `AUDIO` and `Audio`, and one card id being a prefix of another is
///   ordinary;
/// - `alsa/hw:2,0`, which carries the card *index* and no name at all. A
///   substring test can never match that, and it is the spelling a listener is
///   most likely to type by hand for the direct path - where the readout is the
///   whole reason for going direct.
#[must_use]
pub fn card_matches(hint: &str, card_id: &str, card_index: u32) -> bool {
    if let Some(named) = named_card(hint) {
        return named == card_id;
    }
    if let Some(index) = hw_card_index(hint) {
        return index == card_index;
    }
    hint.contains(card_id)
}

/// Does this device identifier refer *directly* to this card?
///
/// The same question as [`card_matches`] asked the other way round, and
/// deliberately stricter: this is used to map a refused hardware device onto
/// the sound server's own entry for the same card, where a loose substring
/// match would silently pair a device with the wrong card and move the audio
/// somewhere nobody asked for. Only the two explicit spellings count.
///
/// Either half of the card's identity may be missing from what the server
/// published, so both are optional and an absent one simply cannot match.
#[must_use]
pub fn refers_to_card(device: &str, card_id: Option<&str>, card_index: Option<u32>) -> bool {
    if let Some(named) = named_card(device) {
        return card_id == Some(named);
    }
    if let Some(index) = hw_card_index(device) {
        return card_index == Some(index);
    }
    false
}

/// Does this identifier reach an ALSA card directly, with nothing in between?
///
/// True for `alsa/hw:CARD=AUDIO,DEV=0` and its plugin spellings, which are the
/// card itself; false for `alsa/pipewire` and `alsa/default`, which are routed
/// back into the sound server, and for every other driver, which *is* a sound
/// server. Two things turn on this: such a device is exclusive by construction
/// and has no shared spelling to fall back to, and priel is not a client of the
/// sound server while it is using one, so there is no graph to show.
#[must_use]
pub fn is_direct_card_device(device: &str) -> bool {
    let Some(("alsa", name)) = device.split_once('/') else {
        return false;
    };
    named_card(name).is_some() || hw_card_index(name).is_some()
}

/// The card id an `...:CARD=<id>...` device name spells out.
fn named_card(device: &str) -> Option<&str> {
    let named = device.split("CARD=").nth(1)?;
    Some(
        named
            .split([',', ':'])
            .next()
            .unwrap_or(named)
            .trim_end_matches('\''),
    )
}

/// The card index in an `hw:2,0` style device name, if that is what it is.
///
/// `plughw:` and the other plugin prefixes take the same argument, so the digits
/// are looked for after the last colon-separated prefix rather than after a
/// fixed one.
fn hw_card_index(hint: &str) -> Option<u32> {
    let (_, args) = hint.rsplit_once(':')?;
    let digits = args.split(',').next()?;
    digits.parse().ok()
}

/// Find an open ALSA playback substream and read its live parameters.
///
/// Scans every card for a playback substream that is not `closed`. While priel
/// is playing there is normally exactly one, which is the chain in use. `hint`
/// is the output device identifier, matched against the card by
/// [`card_matches`] so an explicit choice wins when several devices are open at
/// once; without it the first open substream is used.
#[must_use]
pub fn probe(hint: Option<&str>) -> Option<HwParams> {
    let mut first = None;
    for (card, index) in cards() {
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
            if hint.is_some_and(|h| card_matches(h, &id, index)) {
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

/// `/proc/asound/cardN` directories, each with the `N` that names it.
///
/// The index is carried out rather than re-parsed later because it is half of
/// what a device identifier can be matched against: `hw:2,0` names the card by
/// number and never by id.
fn cards() -> Vec<(String, u32)> {
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
            let index = name.strip_prefix("card")?.parse::<u32>().ok()?;
            Some((path.to_str()?.to_string(), index))
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
    use super::{card_matches, is_direct_card_device, parse_hw_params, probe, refers_to_card};

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
    fn a_sound_server_identifier_still_finds_its_card_by_name() {
        // Goal: the identifiers the sound server publishes carry the card name
        // inside a longer string, and matching them is how the readout has
        // always found the right card. Nothing here may regress.
        assert!(card_matches(
            "pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0",
            "AUDIO",
            2
        ));
        assert!(!card_matches(
            "pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0",
            "HDMI",
            0
        ));
    }

    #[test]
    fn a_direct_device_name_matches_the_card_it_names() {
        // Goal: the exclusive path is asked for with a hardware device name, and
        // the live readout is the entire justification for taking that path. An
        // `alsa/...` identifier spells the card as `CARD=<id>`, which a substring
        // test happens to get right - so it is pinned here.
        assert!(card_matches("alsa/hw:CARD=AUDIO,DEV=0", "AUDIO", 2));
        assert!(card_matches("alsa/sysdefault:CARD=AUDIO", "AUDIO", 2));
    }

    #[test]
    fn a_card_index_is_understood_as_well_as_a_card_name() {
        // Goal: `hw:2,0` is the spelling a listener is most likely to type by
        // hand, and it carries the card *index* rather than its name - so a
        // substring test against the card id can never match it, and the badge
        // would silently stop saying DAC on exactly the path that earns it.
        assert!(card_matches("alsa/hw:2,0", "AUDIO", 2));
        assert!(card_matches("alsa/plughw:2", "AUDIO", 2));
        assert!(!card_matches("alsa/hw:2,0", "HDMI", 0));
    }

    #[test]
    fn a_named_card_is_matched_exactly_rather_than_loosely() {
        // Goal: two cards on this kind of machine differ only in case (`AUDIO`
        // and `Audio`), and one card id being a prefix of another is ordinary.
        // Once the name is spelled out as `CARD=`, take it literally.
        assert!(!card_matches("alsa/hw:CARD=AUDIO,DEV=0", "Audio", 3));
        assert!(!card_matches("alsa/hw:CARD=AUDIO,DEV=0", "AUD", 1));
    }

    #[test]
    fn an_identifier_naming_no_card_matches_none() {
        // Goal: `auto` and the server's own default name no card at all, so
        // there is nothing to prefer and the first open substream is used.
        assert!(!card_matches("auto", "AUDIO", 2));
        assert!(!card_matches("pipewire", "AUDIO", 2));
    }

    #[test]
    fn a_hardware_device_is_told_from_one_routed_through_the_sound_server() {
        // Goal: two things turn on this. A card device is exclusive by
        // construction and so has no shared spelling to fall back to when it
        // refuses, and priel is not a client of the sound server while it is
        // using one - so there is no graph to show either.
        assert!(is_direct_card_device("alsa/hw:CARD=AUDIO,DEV=0"));
        assert!(is_direct_card_device("alsa/hw:2,0"));
        assert!(is_direct_card_device("alsa/plughw:2"));
        assert!(is_direct_card_device("alsa/front:CARD=AUDIO,DEV=0"));
        assert!(
            !is_direct_card_device("alsa/pipewire"),
            "the server's own ALSA device is the server, not the card"
        );
        assert!(!is_direct_card_device("alsa/default"));
        assert!(!is_direct_card_device("alsa"));
        assert!(!is_direct_card_device("pipewire/alsa_output.usb-x"));
        assert!(!is_direct_card_device("auto"));
    }

    #[test]
    fn mapping_a_device_onto_a_card_refuses_to_guess() {
        // Goal: this is what pairs a refused `hw:` device with the sound
        // server's entry for the same card, and a loose match would move the
        // audio to a device nobody asked for. Only the explicit spellings count,
        // and half an identity is not a match.
        assert!(refers_to_card(
            "alsa/hw:CARD=AUDIO,DEV=0",
            Some("AUDIO"),
            Some(2)
        ));
        assert!(refers_to_card("alsa/hw:2,0", Some("AUDIO"), Some(2)));
        assert!(
            refers_to_card("alsa/hw:2,0", None, Some(2)),
            "an index is enough on its own"
        );
        assert!(
            !refers_to_card("alsa/hw:2,0", Some("AUDIO"), None),
            "and so is its absence"
        );
        assert!(!refers_to_card(
            "alsa/hw:CARD=AUDIO,DEV=0",
            Some("Audio"),
            Some(3)
        ));
        assert!(
            !refers_to_card(
                "pipewire/alsa_output.usb-x-AUDIO-00",
                Some("AUDIO"),
                Some(2)
            ),
            "carrying the name inside a longer string is not a direct reference"
        );
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
