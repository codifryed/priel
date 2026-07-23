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
//! It also *constructs* the direct hardware devices, for the same reason and
//! from the same directory: ALSA's discovery interface does not advertise raw
//! hardware PCMs, so mpv cannot list one and the picker cannot offer one. The
//! kernel does list them, and an identifier built from what it says is an
//! identifier `--device` accepts.
//!
//! Linux-only by nature. Everywhere else this reports nothing, and the caller
//! falls back to what the audio server said.

use std::path::{Path, PathBuf};

use crate::AudioDevice;

/// Where the kernel publishes the sound cards.
const ASOUND: &str = "/proc/asound";

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

/// The sample rates a card's playback stream advertises it can be clocked at.
///
/// Parsed from the body of `/proc/asound/cardN/streamN`, the USB-Audio
/// descriptor: the device's *capabilities*, there whether or not anything is
/// playing, unlike the `hw_params` an open substream shows. Only the
/// `Playback:` section counts - a capture-only interface, a webcam microphone,
/// is not an output - and the union across every playback altset is taken,
/// because a device may split its rates one to an altset rather than listing
/// them together. The result is sorted and deduplicated.
///
/// An empty list means "not known", not "the hardware can do none": a
/// capture-only device, a body this cannot make sense of, or a descriptor that
/// stated a continuous range rather than a list. The caller must read it that
/// way and make no capability claim from it, because telling a listener their
/// DAC cannot do a rate it can would be worse than saying nothing.
#[must_use]
pub fn supported_playback_rates(body: &str) -> Vec<u32> {
    let mut rates = Vec::new();
    let mut in_playback = false;
    for line in body.lines() {
        match line.trim() {
            "Playback:" => in_playback = true,
            "Capture:" => in_playback = false,
            rest if in_playback => {
                if let Some(list) = rest.strip_prefix("Rates:") {
                    rates.extend(list.split(',').filter_map(|r| r.trim().parse::<u32>().ok()));
                }
            }
            _ => {}
        }
    }
    rates.sort_unstable();
    rates.dedup();
    rates
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
    for (card, index) in cards_in(Path::new(ASOUND)) {
        let Some(id) = read_trimmed(&card.join("id")) else {
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

/// How many hardware entries are ever constructed.
///
/// Bounded like everything else built from outside. A machine with an audio
/// interface per channel still lands far inside this, and the HDMI card that
/// motivated the per-device entries contributes six on its own.
const CARD_DEVICES_MAX: usize = 64;

/// The direct hardware devices, which no discovery interface advertises.
///
/// ALSA's device-name hints list the plugin spellings of a card - `sysdefault`,
/// `front`, the `surround*` family, `iec958` - and never the raw `hw:` PCM
/// underneath them, so mpv cannot report one and the picker cannot offer one.
/// The kernel names every card and every PCM it has, which is exactly the
/// information an identifier is built from, so these are *constructed* rather
/// than discovered.
///
/// An empty list where there is no `/proc/asound` is the answer, not a failure:
/// a platform without one has no hardware device to name.
#[must_use]
pub fn card_devices() -> Vec<AudioDevice> {
    card_devices_in(Path::new(ASOUND))
}

/// Add the direct hardware devices to a list the player enumerated.
///
/// Called wherever that list is published, so the picker and `--list-devices`
/// cannot disagree about what exists.
#[must_use]
pub fn with_card_devices(listed: Vec<AudioDevice>) -> Vec<AudioDevice> {
    merge(listed, card_devices())
}

/// The merge itself, pure so both guards are a table of tests.
///
/// Two things it must not do. It must not offer an identifier the player would
/// reject: `alsa/hw:...` is only accepted by a build with the ALSA output in
/// it, and one that reports no ALSA device at all does not have it. And it must
/// not list a card twice under one name - nothing advertises a raw `hw:` PCM
/// today, but an `asoundrc` could, and the enumerated entry is the one to keep
/// because its description came from the driver that will open the device.
///
/// The constructed entries go last rather than beside the card they belong to.
/// The order the player reported is the order it prefers, the sound server's
/// entry is still the right default, and a stable tail is easier to find twice
/// than a position that moves with the card list.
fn merge(listed: Vec<AudioDevice>, direct: Vec<AudioDevice>) -> Vec<AudioDevice> {
    if !listed.iter().any(|d| d.name.starts_with("alsa/")) {
        return listed;
    }
    let mut out = listed;
    for device in direct {
        if out.iter().any(|d| d.name == device.name) {
            continue;
        }
        out.push(device);
    }
    out
}

/// [`card_devices`], against any directory, so it is testable against a fixture
/// on a machine with no sound card at all.
fn card_devices_in(root: &Path) -> Vec<AudioDevice> {
    // The one file carrying human-readable card names. Absent on an older
    // kernel and unreadable in a container, in which case the card id stands in
    // for the name - the identifier is still right, only the row is terser.
    let names = parse_card_names(&std::fs::read_to_string(root.join("cards")).unwrap_or_default());

    let mut cards = cards_in(root);
    // read_dir answers in whatever order the filesystem likes, and a device
    // list that reshuffles itself between two openings of the picker would move
    // the row under the pointer.
    cards.sort_by_key(|&(_, index)| index);

    let mut out = Vec::new();
    for (dir, index) in cards {
        // The id, not the index: `hw:CARD=<id>` survives a card being
        // renumbered, and this machine has both `AUDIO` and `Audio`.
        let Some(id) = read_trimmed(&dir.join("id")) else {
            continue;
        };
        let card_name = names
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(id.as_str(), |(_, name)| name.as_str());
        // Every playback PCM gets its own entry, not just the first: they are
        // separate outputs - the HDMI card's are one per connector - and their
        // device numbers are not consecutive, so an absent one cannot be
        // guessed from the one that is shown.
        for device in playback_pcms(&dir) {
            if out.len() == CARD_DEVICES_MAX {
                return out;
            }
            let pcm_name = read_field(&dir.join(format!("pcm{device}p/info")), "name");
            out.push(AudioDevice {
                name: format!("alsa/hw:CARD={id},DEV={device}"),
                description: describe(card_name, pcm_name.as_deref()),
            });
        }
    }
    out
}

/// What a hardware row says about itself.
///
/// Both names earn their place: the card name is what tells two USB cards
/// apart, and the PCM name is what tells one card's HDMI connectors apart. The
/// marker is the only thing in the list that says which spelling of a card
/// reaches it directly, so it is not optional.
fn describe(card_name: &str, pcm_name: Option<&str>) -> String {
    match pcm_name {
        Some(pcm) if !pcm.is_empty() => format!("{card_name}, {pcm} (direct hardware access)"),
        _ => format!("{card_name} (direct hardware access)"),
    }
}

/// Card index and human-readable name, from the body of `/proc/asound/cards`.
///
/// Each card takes two lines, of which only the first is wanted:
///
/// ```text
///  2 [AUDIO          ]: USB-Audio - SMSL USB AUDIO
///                       SMSL SMSL USB AUDIO at usb-0000:73:00.3-2.2, high speed
/// ```
///
/// The name is what follows the driver, which is the same string ALSA's own
/// tools print for the card. Splitting on `" - "` rather than `'-'` is what
/// keeps `HDA-Intel` and `USB-Audio` from being cut in half.
fn parse_card_names(body: &str) -> Vec<(u32, String)> {
    body.lines()
        .filter_map(|line| {
            let (index, rest) = line.split_once('[')?;
            let index: u32 = index.trim().parse().ok()?;
            let (_, rest) = rest.split_once("]: ")?;
            let (_, name) = rest.split_once(" - ")?;
            let name = name.trim();
            (!name.is_empty()).then(|| (index, name.to_string()))
        })
        .collect()
}

/// One `key: value` line out of a `/proc/asound` file.
fn read_field(path: &Path, key: &str) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    body.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, value)| value.trim().to_string())
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// `cardN` directories under `root`, each with the `N` that names it.
///
/// The index is carried out rather than re-parsed later because it is half of
/// what a device identifier can be matched against: `hw:2,0` names the card by
/// number and never by id.
fn cards_in(root: &Path) -> Vec<(PathBuf, u32)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?;
            // `card0`, not the `Generic -> card1` symlinks, which would double
            // every device up.
            let index = name.strip_prefix("card")?.parse::<u32>().ok()?;
            Some((path, index))
        })
        .collect()
}

/// The device numbers of a card's playback PCMs, in order.
///
/// `pcm0p` is playback and `pcm0c` is capture, and the number between them is
/// the `DEV=` of a hardware identifier. A card with none of the former plays
/// nothing, whatever else the directory holds.
fn playback_pcms(card: &Path) -> Vec<u32> {
    let Ok(pcms) = std::fs::read_dir(card) else {
        return Vec::new();
    };
    let mut out: Vec<u32> = pcms
        .flatten()
        .filter_map(|pcm| {
            let path = pcm.path();
            let name = path.file_name()?.to_str()?;
            name.strip_prefix("pcm")?.strip_suffix('p')?.parse().ok()
        })
        .collect();
    out.sort_unstable();
    out
}

/// `hw_params` paths under a card's playback PCMs.
fn substreams(card: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for device in playback_pcms(card) {
        let Ok(subs) = std::fs::read_dir(card.join(format!("pcm{device}p"))) else {
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
    use std::path::Path;

    use super::{
        card_devices, card_devices_in, card_matches, is_direct_card_device, parse_hw_params, probe,
        refers_to_card, supported_playback_rates,
    };

    /// A copy of a real `/proc/asound`, trimmed to the files this reads.
    ///
    /// Four cards, because the interesting cases all come from one machine: an
    /// HDMI card with several playback devices, a card with no playback device
    /// at all, and two USB cards whose ids differ only in case.
    const ASOUND: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/asound");

    /// The same directory stripped of everything optional: one card, no names
    /// file, and a PCM whose `info` read back empty.
    const BARE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/asound-bare");

    fn fixture_devices() -> Vec<crate::AudioDevice> {
        card_devices_in(Path::new(ASOUND))
    }

    fn fixture_names() -> Vec<String> {
        fixture_devices().into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn every_playback_card_is_offered_as_a_device_named_by_its_id() {
        // Goal: this is the whole issue - the enumeration advertises no raw
        // hardware device, so the one path where exclusivity means anything
        // cannot be discovered. The identifier is the card *id*, because an
        // index does not survive a card being renumbered.
        let names = fixture_names();
        assert!(
            names.contains(&"alsa/hw:CARD=AUDIO,DEV=0".to_string()),
            "the USB DAC should be offered directly: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("alsa/hw:2")),
            "named by id, never by index: {names:?}"
        );
    }

    #[test]
    fn two_cards_whose_ids_differ_only_in_case_stay_two_devices() {
        // Goal: this machine has `AUDIO` and `Audio`. Folding the case, or
        // naming either by index, would point the listener at the wrong DAC.
        let names = fixture_names();
        assert!(names.contains(&"alsa/hw:CARD=AUDIO,DEV=0".to_string()));
        assert!(names.contains(&"alsa/hw:CARD=Audio,DEV=0".to_string()));
    }

    #[test]
    fn a_card_with_several_playback_devices_offers_each_of_them() {
        // Goal: the HDMI card's playback devices are separate connectors, so
        // offering only the first would hide every screen but one - and the
        // device number is not consecutive, so it cannot be guessed either.
        let names = fixture_names();
        assert!(names.contains(&"alsa/hw:CARD=HDMI,DEV=3".to_string()));
        assert!(names.contains(&"alsa/hw:CARD=HDMI,DEV=7".to_string()));
    }

    #[test]
    fn a_capture_device_is_never_offered_as_an_output() {
        // Goal: `pcm1c` is a microphone. Offering it would be an identifier
        // that resolves and then refuses to play, which is worse than absent.
        let names = fixture_names();
        assert!(
            !names.contains(&"alsa/hw:CARD=Audio,DEV=1".to_string()),
            "the capture-only device must not be listed: {names:?}"
        );
    }

    #[test]
    fn a_card_with_no_playback_device_is_not_offered_at_all() {
        // Goal: a card that plays nothing has no identifier worth printing,
        // and its bare presence in the directory is not evidence that it does.
        let names = fixture_names();
        assert!(
            !names.iter().any(|n| n.contains("CARD=Generic")),
            "a card with no playback PCM is not an output: {names:?}"
        );
    }

    #[test]
    fn a_hardware_entry_is_described_by_the_names_the_kernel_carries() {
        // Goal: a row reading only `alsa/hw:CARD=AUDIO,DEV=0` is barely better
        // than looking the identifier up. Both halves are needed: the card name
        // tells two USB cards apart, and the device name tells one card's HDMI
        // connectors apart. The marker is what says this row is the direct one.
        let devices = fixture_devices();
        let dac = devices
            .iter()
            .find(|d| d.name == "alsa/hw:CARD=AUDIO,DEV=0")
            .expect("the fixture's USB DAC should be offered");
        assert!(dac.description.contains("SMSL USB AUDIO"), "{dac:?}");
        assert!(dac.description.contains("USB Audio"), "{dac:?}");
        assert!(dac.description.contains("direct"), "{dac:?}");
    }

    #[test]
    fn a_card_the_kernel_names_nowhere_is_still_offered_under_its_id() {
        // Goal: `/proc/asound/cards` is absent on an older kernel and
        // unreadable in some containers, and a PCM's `info` can be read while
        // it is being written. Only the prose may depend on either - losing the
        // identifier over a missing description would lose the device.
        let devices = card_devices_in(Path::new(BARE));
        assert_eq!(devices.len(), 1, "{devices:?}");
        assert_eq!(devices[0].name, "alsa/hw:CARD=AUDIO,DEV=0");
        assert_eq!(
            devices[0].description, "AUDIO (direct hardware access)",
            "the card id stands in for the name it does not have"
        );
    }

    #[test]
    fn the_devices_come_back_in_the_same_order_every_time() {
        // Goal: read_dir answers in whatever order the filesystem likes. A list
        // that reshuffled between two openings of the picker would move the row
        // out from under the pointer.
        assert_eq!(fixture_names(), fixture_names());
        assert_eq!(
            fixture_names(),
            vec![
                "alsa/hw:CARD=HDMI,DEV=3",
                "alsa/hw:CARD=HDMI,DEV=7",
                "alsa/hw:CARD=AUDIO,DEV=0",
                "alsa/hw:CARD=Audio,DEV=0",
            ],
            "by card index, then by device number"
        );
    }

    fn listed(names: &[&str]) -> Vec<crate::AudioDevice> {
        names
            .iter()
            .map(|n| crate::AudioDevice {
                name: (*n).to_string(),
                description: format!("as the player described {n}"),
            })
            .collect()
    }

    #[test]
    fn the_hardware_devices_are_added_to_what_the_player_enumerated() {
        // Goal: the constructed entries are an addition, not a replacement.
        // Everything the player reported keeps its place and its order, because
        // the sound server's entry for a card is still the right default.
        let merged = super::merge(
            listed(&[
                "auto",
                "alsa/sysdefault:CARD=AUDIO",
                "pipewire/alsa_output.x",
            ]),
            listed(&["alsa/hw:CARD=AUDIO,DEV=0"]),
        );
        let names: Vec<&str> = merged.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "auto",
                "alsa/sysdefault:CARD=AUDIO",
                "pipewire/alsa_output.x",
                "alsa/hw:CARD=AUDIO,DEV=0",
            ]
        );
    }

    #[test]
    fn a_device_the_player_already_reported_is_not_offered_twice() {
        // Goal: nothing advertises a raw `hw:` PCM today, but an asoundrc or a
        // later ALSA could. One card must not appear twice under one name, and
        // the player's own description is the one to keep - it came from the
        // driver that will open the device.
        let merged = super::merge(
            listed(&["auto", "alsa/hw:CARD=AUDIO,DEV=0"]),
            vec![crate::AudioDevice {
                name: "alsa/hw:CARD=AUDIO,DEV=0".to_string(),
                description: "constructed".to_string(),
            }],
        );
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert!(
            merged[1].description.contains("as the player described"),
            "the enumerated entry stands: {merged:?}"
        );
    }

    #[test]
    fn nothing_is_added_when_the_player_has_no_alsa_output_to_offer_them_to() {
        // Goal: every identifier in this list is one `--device` accepts, and
        // `alsa/hw:...` is only accepted by a player built with ALSA. A player
        // that reports no ALSA device at all has none, so these would be rows
        // that resolve to nothing.
        let merged = super::merge(
            listed(&["auto", "pipewire/alsa_output.x"]),
            listed(&["alsa/hw:CARD=AUDIO,DEV=0"]),
        );
        assert_eq!(merged.len(), 2, "{merged:?}");
    }

    #[test]
    fn no_kernel_sound_directory_means_nothing_extra_rather_than_a_failure() {
        // Goal: this is Linux-only by nature, and the suite runs in containers
        // with no sound card at all. Reporting nothing is the answer there.
        assert!(card_devices_in(Path::new("/nonexistent/asound")).is_empty());
        assert!(card_devices_in(Path::new(ASOUND).join("cards").as_path()).is_empty());
        let _ = card_devices();
    }

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

    /// A `stream0` from a real two-channel USB interface: a playback altset and
    /// a capture altset, each listing the same six rates.
    const SCARLETT: &str =
        "Focusrite Scarlett 2i2 USB at usb-0000:66:00.4-1.2, high speed : USB Audio

Playback:
  Status: Stop
  Interface 1
    Altset 1
    Format: S32_LE
    Channels: 2
    Endpoint: 0x01 (1 OUT) (SYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000
    Bits: 24

Capture:
  Status: Running
  Interface 2
    Altset 1
    Format: S32_LE
    Channels: 2
    Endpoint: 0x82 (2 IN) (ASYNC)
    Rates: 44100, 48000, 88200, 96000, 176400, 192000
    Bits: 24";

    /// A dock: its playback tops out at 48 kHz and its capture is a mono mic.
    const DOCK: &str = "Lenovo ThinkPad USB-C Dock Gen2 USB Au at usb-x, full speed : USB Audio

Playback:
  Status: Stop
  Interface 2
    Altset 1
    Format: S16_LE
    Channels: 2
    Rates: 8000, 16000, 32000, 44100, 48000
    Bits: 16

Capture:
  Status: Stop
  Interface 1
    Altset 1
    Format: S16_LE
    Channels: 1
    Rates: 8000, 16000, 32000, 44100, 48000
    Bits: 16";

    /// A webcam: capture only, its rates split one to an altset, no playback.
    const WEBCAM: &str = "Logitech Webcam C930e at usb-x, high speed : USB Audio

Capture:
  Status: Stop
  Interface 3
    Altset 1
    Rates: 16000
  Interface 3
    Altset 2
    Rates: 24000
  Interface 3
    Altset 3
    Rates: 32000
  Interface 3
    Altset 4
    Rates: 48000";

    #[test]
    fn a_dacs_supported_rates_are_read_from_its_playback_descriptor() {
        // Goal: this is what a resample can be checked against - the rates the
        // hardware can be clocked at, not the one it is clocked at now.
        assert_eq!(
            supported_playback_rates(SCARLETT),
            vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
        );
        assert_eq!(
            supported_playback_rates(DOCK),
            vec![8_000, 16_000, 32_000, 44_100, 48_000]
        );
    }

    #[test]
    fn the_capture_side_is_never_mistaken_for_an_output() {
        // Goal: an interface carries both, and a microphone's rates are not the
        // DAC's. A capture-only device is not an output at all, and where the
        // two sides list different rates only the playback ones may come back.
        assert!(
            supported_playback_rates(WEBCAM).is_empty(),
            "a capture-only device is not an output"
        );
        let mixed = "X at usb : USB Audio\n\nPlayback:\n  Interface 1\n    Altset 1\n    Rates: 44100, 48000\n\nCapture:\n  Interface 2\n    Altset 1\n    Rates: 192000";
        assert_eq!(
            supported_playback_rates(mixed),
            vec![44_100, 48_000],
            "the capture-only 192000 must not leak into the output rates"
        );
    }

    #[test]
    fn rates_split_one_to_an_altset_are_gathered_into_one_set() {
        // Goal: a device may advertise one rate per playback altset rather than
        // a list, so the union is what it can do - sorted and without repeats.
        let per_altset = "Y at usb : USB Audio\n\nPlayback:\n  Interface 1\n    Altset 1\n    Rates: 44100\n  Interface 1\n    Altset 2\n    Rates: 96000\n  Interface 1\n    Altset 3\n    Rates: 44100";
        assert_eq!(supported_playback_rates(per_altset), vec![44_100, 96_000]);
    }

    #[test]
    fn a_body_with_no_playback_rates_is_empty_rather_than_a_guess() {
        // Goal: an empty list means "not known", and the caller reads it that
        // way. A blank read, the `closed` placeholder, and a playback section
        // that named no rate must all come back empty rather than inventing one.
        assert!(supported_playback_rates("").is_empty());
        assert!(supported_playback_rates("closed").is_empty());
        assert!(supported_playback_rates("Playback:\n  Status: Stop").is_empty());
    }
}
