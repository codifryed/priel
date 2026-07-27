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

//! The changes priel can make to the sound server on the listener's behalf.
//!
//! These are the actions that reach outside priel's own files, and they are kept
//! to the narrowest shape that can. **priel only ever writes files it is the
//! sole author of** - [`RATES_CONF`] under the user's `pipewire.conf.d` and
//! [`RESERVE_CONF`] under their `wireplumber.conf.d` - and never edits the sound
//! server's own configuration or one the listener wrote. Dropping a file into
//! those directories is how each is meant to be configured, so this is priel
//! managing its own files, not priel editing another program's.
//!
//! Every content here is a pure function of what it is given
//! ([`rates_conf_text`], [`reserve_conf_text`]) and every command is a pure
//! function that can be read without being run ([`restart_argv`],
//! [`clear_pin_argv`], [`switch_argv`]) - so the preview a listener approves and
//! what actually happens are computed the same way, once.
//!
//! The one change that is not a file is [`clear_rate_pin`]: `clock.force-rate`
//! is live metadata rather than configuration, so it is set back to zero and
//! takes effect at once, with no restart and nothing left behind to delete.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::run::{self, RunError};

/// How long the sound-server restart gets before priel stops waiting on it.
const RESTART_TIMEOUT: Duration = Duration::from_secs(15);

/// The drop-in priel writes into the server's own configuration directory, and
/// the only file it writes there.
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

/// The directory priel's drop-in belongs in: the user's `pipewire.conf.d`.
///
/// `$XDG_CONFIG_HOME/pipewire/pipewire.conf.d`, or `~/.config/...` when
/// `XDG_CONFIG_HOME` is unset or empty. `None` when neither is known, which is
/// the one machine priel cannot offer to set up.
#[must_use]
pub fn conf_dir() -> Option<PathBuf> {
    conf_dir_from(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

/// [`conf_dir`] over its two inputs, so the resolution is a table of tests
/// rather than something that reads the real environment.
fn conf_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("pipewire").join("pipewire.conf.d"))
}

/// Write priel's drop-in with `allowed_hz` into `dir`, creating the directory if
/// it is not there, and return the path written.
///
/// `dir` is passed in and never resolved from a global here, so a test writes to
/// a directory it made and never to a real home - the rule the whole crate keeps.
///
/// # Errors
///
/// The `std::io::Error` from creating the directory or writing the file.
pub fn write_rates(dir: &Path, allowed_hz: &[u32]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(RATES_CONF);
    std::fs::write(&path, rates_conf_text(allowed_hz))?;
    Ok(path)
}

/// The command that restarts the user's sound server so a new drop-in takes
/// effect, as a program and its arguments.
///
/// Pure, so what would be run can be checked without running it. `--user`
/// because priel wrote a user drop-in and touches nothing system-wide, and both
/// the server and its session manager, because the rate list is read at start.
#[must_use]
pub fn restart_argv() -> (&'static str, [&'static str; 4]) {
    (
        "systemctl",
        ["--user", "restart", "pipewire", "wireplumber"],
    )
}

/// Restart the user's sound server. Blocking, so it runs off the UI thread.
///
/// # Errors
///
/// A human-readable reason when `systemctl` is missing, is killed, fails, or
/// does not finish in time.
pub fn restart_pipewire() -> Result<(), String> {
    let (cmd, args) = restart_argv();
    run::capture(cmd, &args, RESTART_TIMEOUT)
        .map(|_| ())
        .map_err(|e| why(cmd, "the restart", e))
}

/// One failed child process, as a sentence naming what was being attempted.
///
/// Shared by every command priel runs at the sound server, so a new one cannot
/// arrive with a different vocabulary for the same six failures. `program` is
/// named only where the binary itself is missing, which is the one failure the
/// listener fixes by installing something; the rest read better as what priel
/// was doing.
fn why(program: &str, noun: &str, e: RunError) -> String {
    match e {
        RunError::NotFound => format!("{program} was not found"),
        RunError::TimedOut => format!("{noun} did not finish in time"),
        RunError::Spawn(reason) => format!("{noun} could not be started: {reason}"),
        RunError::Failed(Some(code)) => format!("{noun} exited with status {code}"),
        RunError::Failed(None) => format!("{noun} was killed by a signal"),
        RunError::Unreadable => format!("{noun} output could not be read"),
    }
}

/// The drop-in that stops the sound server claiming one card, and the only other
/// file priel writes.
///
/// `99-` for the same reason as [`RATES_CONF`], and priel's own name in it for
/// the same reason too: anyone who finds it knows what put it there, and
/// deleting it is how the card goes back to the server.
pub const RESERVE_CONF: &str = "99-priel-reserve.conf";

/// The contents of priel's reservation drop-in for one card.
///
/// The rule is the one the report has always printed, so what a listener was
/// told to write by hand and what priel writes are the same bytes. It opens by
/// saying what it costs, because the file outlives the screen that explained it:
/// nothing else on the machine can play through the card while it is in place.
#[must_use]
pub fn reserve_conf_text(card_name: &str) -> String {
    format!(
        "# Written by priel: keeps the sound server from claiming this card, so\n\
         # priel can open it directly and play bit-perfect. Nothing else on this\n\
         # machine can play through it while this file is here. Safe to delete.\n\
         monitor.alsa.rules = [\n  \
         {{\n    \
         matches = [\n      \
         {{ device.name = \"{card_name}\" }}\n    \
         ]\n    \
         actions = {{ update-props = {{ device.disabled = true }} }}\n  \
         }}\n\
         ]\n"
    )
}

/// The directory priel's reservation drop-in belongs in: the user's
/// `wireplumber.conf.d`.
///
/// The session manager reads it, not the server, so it is a different directory
/// from [`conf_dir`] - and the rule would be ignored in the other one.
#[must_use]
pub fn reserve_conf_dir() -> Option<PathBuf> {
    reserve_conf_dir_from(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

/// [`reserve_conf_dir`] over its two inputs, table-testable like
/// [`conf_dir_from`].
fn reserve_conf_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = xdg
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wireplumber").join("wireplumber.conf.d"))
}

/// Write priel's reservation drop-in for `card_name` into `dir`, and return the
/// path written.
///
/// `dir` is passed in and never resolved from a global here, so a test writes to
/// a directory it made and never to a real home - the rule the whole crate keeps.
///
/// # Errors
///
/// The `std::io::Error` from creating the directory or writing the file.
pub fn write_reserve(dir: &Path, card_name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(RESERVE_CONF);
    std::fs::write(&path, reserve_conf_text(card_name))?;
    Ok(path)
}

/// The command that clears the sound server's rate pin, as a program and its
/// arguments.
///
/// Pure, so what would run can be checked without running it. `clock.force-rate`
/// holds the whole graph at one rate and the permitted list is not consulted at
/// all, so this is the only change that helps there - extending a list the
/// server has stopped reading does nothing. Zero is how the setting is spelled
/// off; there is no separate delete.
#[must_use]
pub fn clear_pin_argv() -> (&'static str, [&'static str; 5]) {
    (
        "pw-metadata",
        ["-n", "settings", "0", "clock.force-rate", "0"],
    )
}

/// Clear the sound server's rate pin. Blocking, so it runs off the UI thread.
///
/// Takes effect immediately - the setting is live metadata rather than a file,
/// so unlike the two drop-ins this needs no restart.
///
/// # Errors
///
/// A human-readable reason when `pw-metadata` is missing, is killed, fails, or
/// does not finish in time.
pub fn clear_rate_pin() -> Result<(), String> {
    let (cmd, args) = clear_pin_argv();
    run::capture(cmd, &args, RESTART_TIMEOUT)
        .map(|_| ())
        .map_err(|e| why(cmd, "clearing the pin", e))
}

/// The command that switches a Bluetooth device's A2DP profile, as a program and
/// its arguments.
///
/// Pure, so what would run can be checked without running it. `wpctl set-profile
/// <device> <profile>` selects one of the device's profiles - on a bluez device
/// each A2DP codec is a profile, so this is how the codec is changed. User-level;
/// nothing here needs root.
#[must_use]
pub fn switch_argv(device_id: u32, profile_index: u32) -> (&'static str, [String; 3]) {
    (
        "wpctl",
        [
            "set-profile".to_string(),
            device_id.to_string(),
            profile_index.to_string(),
        ],
    )
}

/// Switch a Bluetooth device to the A2DP profile `profile_index`. Blocking, so
/// it runs off the UI thread.
///
/// Selecting a codec profile re-negotiates the A2DP link, so the sound briefly
/// drops - that is the device's doing, not a fault here.
///
/// # Errors
///
/// A human-readable reason when `wpctl` is missing, is killed, fails, or does not
/// finish in time.
pub fn switch_bt_codec(device_id: u32, profile_index: u32) -> Result<(), String> {
    let (cmd, args) = switch_argv(device_id, profile_index);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run::capture(cmd, &args, RESTART_TIMEOUT)
        .map(|_| ())
        .map_err(|e| why(cmd, "the switch", e))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{
        conf_dir_from, desired_allowed_hz, rates_conf_text, restart_argv, switch_argv, write_rates,
    };

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

    #[test]
    fn the_config_dir_prefers_xdg_then_falls_back_to_home() {
        // Goal: the resolution the write path is handed, without reading the real
        // environment. XDG wins when set, ~/.config stands in when it is not, an
        // empty XDG counts as unset (per the spec), and no home is no offer.
        assert_eq!(
            conf_dir_from(Some("/x/config".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/config/pipewire/pipewire.conf.d"))
        );
        assert_eq!(
            conf_dir_from(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/pipewire/pipewire.conf.d"))
        );
        assert_eq!(
            conf_dir_from(Some(OsString::new()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/pipewire/pipewire.conf.d")),
            "an empty XDG_CONFIG_HOME is unset"
        );
        assert_eq!(conf_dir_from(None, None), None, "no home, nothing to offer");
    }

    #[test]
    fn writing_the_drop_in_creates_the_directory_and_the_file() {
        // Goal: the file that lands carries the content the preview showed, in
        // the directory priel was told to write - which in a test is a temporary
        // one it made, never a real home.
        let dir = std::env::temp_dir().join(format!("priel-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write_rates(&dir, &[44_100, 48_000, 96_000]).expect("write");
        assert_eq!(path, dir.join(super::RATES_CONF));
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.contains("44100 48000 96000"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_restart_targets_the_user_server_and_its_session_manager() {
        // Goal: --user, because priel touched only a user drop-in and nothing
        // system-wide; and both the server and its manager, because the rate
        // list is read at start.
        let (cmd, args) = restart_argv();
        assert_eq!(cmd, "systemctl");
        assert!(args.contains(&"--user"), "user scope, nothing system-wide");
        assert!(args.contains(&"pipewire") && args.contains(&"wireplumber"));
    }

    #[test]
    fn the_reservation_rule_is_the_one_the_report_prints() {
        // Goal: what a listener was told to write by hand and what the key
        // writes are the same bytes, in the directory the session manager reads
        // - not the server's, where the rule would be silently ignored.
        let text = super::reserve_conf_text("alsa_card.usb-Studio_DAC-00");
        assert!(text.contains("monitor.alsa.rules"), "{text}");
        assert!(
            text.contains("device.name = \"alsa_card.usb-Studio_DAC-00\""),
            "{text}"
        );
        assert!(text.contains("device.disabled = true"), "{text}");
        assert!(text.contains("priel"), "says what wrote it: {text}");
        assert!(
            text.contains("Nothing else on this"),
            "and what it costs, because the file outlives the screen: {text}"
        );
        assert_eq!(
            super::reserve_conf_dir_from(None, Some("/home/u".into())),
            Some(PathBuf::from(
                "/home/u/.config/wireplumber/wireplumber.conf.d"
            )),
            "the session manager's directory, not the server's"
        );
    }

    #[test]
    fn writing_the_reservation_creates_the_directory_and_the_file() {
        // Goal: the same rule the rates drop-in keeps - the bytes that land are
        // the ones the preview showed, in a directory priel was told to write,
        // which in a test is a temporary one it made and never a real home.
        let dir = std::env::temp_dir().join(format!("priel-reserve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = super::write_reserve(&dir, "alsa_card.usb-x").expect("write");
        assert_eq!(path, dir.join(super::RESERVE_CONF));
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.contains("alsa_card.usb-x"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_the_pin_sets_the_forced_rate_back_to_zero() {
        // Goal: a pin holds the whole graph at one rate and the permitted list
        // is not consulted at all, so extending that list changes nothing and
        // this is the only thing that helps. Zero is how the setting is spelled
        // off - there is no separate delete - and it is live metadata, so no
        // file is written and no restart is needed.
        let (cmd, args) = super::clear_pin_argv();
        assert_eq!(cmd, "pw-metadata");
        assert_eq!(args, ["-n", "settings", "0", "clock.force-rate", "0"]);
    }

    #[test]
    fn the_codec_switch_sets_the_devices_profile() {
        // Goal: `wpctl set-profile <device> <profile>` names the bluez device
        // and the profile that carries the wanted codec - user-level, no root.
        let (cmd, args) = switch_argv(56, 131_079);
        assert_eq!(cmd, "wpctl");
        assert_eq!(args, ["set-profile", "56", "131079"]);
    }
}
