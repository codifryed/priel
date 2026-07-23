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

//! Permitting the rates a DAC can do, by writing priel's own `PipeWire` drop-in.
//!
//! This is the one action that reaches outside priel's own files, and it is kept
//! to the narrowest shape that can: priel writes a single file it is the sole
//! author of - [`RATES_CONF`] under the user's `pipewire.conf.d` - and never
//! edits the sound server's configuration or one the listener wrote. Dropping a
//! file into that directory is how the server is meant to be configured, so this
//! is priel managing its own file, not priel editing another program's.
//!
//! The content is a pure function of the rate list ([`rates_conf_text`]), and
//! the list itself is a pure function of what is permitted now and what the
//! device can do that is not ([`desired_allowed_hz`]) - so the preview a
//! listener approves and the bytes that land are computed the same way, once.

/// The drop-in priel writes, and the only file it ever writes here.
///
/// `99-` so it sorts last and its `allowed-rates` wins over an earlier drop-in
/// (the server takes the last value of a property, not the union), which is why
/// the list written is the whole of what should be permitted, not an addition.
pub const RATES_CONF: &str = "99-priel-rates.conf";

/// The rates a "set up audio" pass would permit: everything permitted now, plus
/// the rates the device supports that are being blocked.
///
/// Sorted and unique, and it only ever *adds*: nothing already permitted is
/// dropped, so extending the list cannot take a rate away from anything else on
/// the machine that is using it. `blocked_supported` is expected to be
/// [`crate::graph::AudioGraph::blocked_supported_hz`], but the function is pure
/// over the two lists so it is a table of tests rather than a live-graph read.
#[must_use]
pub fn desired_allowed_hz(permitted: &[u32], blocked_supported: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = permitted.iter().chain(blocked_supported).copied().collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The contents of priel's drop-in for a given allowed-rates list.
///
/// Opens with a line saying priel wrote it and that it is safe to delete, so
/// anyone who finds the file knows what put it there and that removing it is
/// harmless. The rates go on one line, spelled the way the server's own core
/// object publishes them.
#[must_use]
pub fn rates_conf_text(allowed_hz: &[u32]) -> String {
    let rates = allowed_hz
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# Written by priel: the sample rates your DAC supports, added to the\n\
         # sound server's permitted list so hi-res tracks play at their own rate\n\
         # rather than being resampled. Safe to delete; priel offers it again.\n\
         context.properties = {{\n    default.clock.allowed-rates = [ {rates} ]\n}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::{desired_allowed_hz, rates_conf_text};

    #[test]
    fn the_desired_list_adds_the_blocked_rates_and_keeps_the_permitted_ones() {
        // Goal: the list only ever grows. Everything already permitted stays, so
        // the change cannot take a rate from something else that is using it, and
        // the device's blocked-but-supported rates are added, sorted and unique.
        assert_eq!(
            desired_allowed_hz(&[44_100, 48_000], &[88_200, 176_400]),
            vec![44_100, 48_000, 88_200, 176_400]
        );
        assert_eq!(
            desired_allowed_hz(&[48_000, 44_100], &[48_000, 96_000]),
            vec![44_100, 48_000, 96_000],
            "an already-permitted rate is not doubled"
        );
        assert_eq!(
            desired_allowed_hz(&[44_100, 48_000], &[]),
            vec![44_100, 48_000],
            "nothing blocked means nothing to add"
        );
    }

    #[test]
    fn the_drop_in_is_a_valid_pipewire_fragment_that_says_priel_wrote_it() {
        // Goal: the bytes that land are the ones the preview showed. The setting
        // is the whole permitted list on one line, inside the section it has to
        // sit in, under a comment naming priel so the file is not a mystery.
        let text = rates_conf_text(&[44_100, 48_000, 88_200, 96_000]);
        assert!(
            text.contains("default.clock.allowed-rates = [ 44100 48000 88200 96000 ]"),
            "the whole list on one line: {text}"
        );
        assert!(
            text.contains("context.properties = {"),
            "inside the section, or the server ignores it: {text}"
        );
        assert!(text.contains("priel"), "and says what wrote it: {text}");
        assert!(
            text.ends_with("}\n"),
            "closed and newline-terminated: {text}"
        );
    }
}
