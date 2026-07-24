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

//! The settings priel remembers between runs.
//!
//! Four preferences, each of which a flag can also set: the palette, the output
//! device, exclusive access to it, and how much the diagnostic log records. A
//! flag wins over this file and this file wins over the default, in the shape
//! [`crate::cli::Cli::resolve_level`] already established.
//!
//! **This module owns no path.** Every entry point takes one, so nothing a test
//! constructs can reach the user's home directory - the same rule that governs
//! the credentials lookup, written after a hidden global broke three tests.
//! `main` is the only caller that resolves [`default_path`].
//!
//! **It does not log, either.** The file is read before the logger exists,
//! because it is where the log level comes from, so what would have been logged
//! is returned as [`Note`]s for `main` to emit once the sink is up.
//!
//! What stays out of the file is as deliberate as what is in it: the session and
//! the client key are runtime state and live under `$XDG_STATE_HOME`, and
//! nothing that moves on its own - volume, the queue, the selected row - belongs
//! here at all. See `docs/adr/0004-a-settings-file-that-is-edited-never-rewritten.md`.

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::cli::{LogLevel, ThemeName};

/// The file name, under the XDG config directory.
const FILE_NAME: &str = "settings.conf";

/// What priel puts at the top of a file it had to create.
///
/// A file a user is invited to edit should explain itself the first time it is
/// opened, which is half the reason the format allows comments at all.
const HEADER: &str = "\
# priel settings. One `key = value` per line; a line starting with `#` is a
# comment. A flag on the command line overrides anything here for that run.
#
#   theme         a palette --theme accepts, e.g. nord or gruvbox-dark
#   device        an output device --device accepts (`priel --list-devices`)
#   exclusive     true or false; --exclusive and --shared override it
#   log_level     off, error, warn, info, debug or trace
#   update_check  true or false; whether to check the forge for a newer release
#   cache_size    album-art cache ceiling in MiB (default 256)
";

/// `$XDG_CONFIG_HOME/priel/settings.conf`, falling back to `~/.config`.
///
/// Resolved through `auth::config_dir` rather than by reading the variable
/// again: the specification's rule that an empty or relative value is invalid is
/// already implemented there, once.
///
/// **Only `main` may call this.** Everything else takes the path as an argument.
#[must_use]
pub fn default_path() -> String {
    format!("{}/{FILE_NAME}", priel_core::auth::config_dir())
}

/// The settings a file can carry, each absent unless it was set.
///
/// `None` means "the file said nothing", which is what lets a flag and a default
/// take over in that order. The same type carries what a picker chose this
/// session, where `None` means "not changed, so do not write a line for it".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub theme: Option<ThemeName>,
    pub device: Option<String>,
    pub exclusive: Option<bool>,
    pub log_level: Option<LogLevel>,
    /// Whether to check the forge for a newer release at startup. Unset means
    /// the default (on). Only ever read from the file - no picker sets it, so it
    /// is never written back automatically; a user turns it off by hand.
    pub update_check: Option<bool>,
    /// The album-art cache ceiling in MiB. Unset means the default (256). Like
    /// [`Self::update_check`], only ever read from the file - no picker sets it -
    /// so it is never written back automatically; a user sets it by hand.
    pub cache_size: Option<u64>,
}

/// Something worth saying about the file, held until there is a log to say it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub level: log::Level,
    pub text: String,
}

/// What a read produced: the settings, and what the reader would have logged.
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub settings: Settings,
    pub notes: Vec<Note>,
}

impl Settings {
    /// Is there nothing here to write?
    ///
    /// `save` asks first, so a session in which no picker was used never opens
    /// the file at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The lines this would write, in a fixed order, absent settings omitted.
    ///
    /// The values are spelled exactly as the flag spells them, because both go
    /// through the same clap `ValueEnum`: what `--theme` accepts and what the
    /// file accepts cannot drift.
    fn assignments(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        if let Some(theme) = self.theme {
            out.push(("theme", value_name(&theme)));
        }
        if let Some(device) = &self.device {
            out.push(("device", device.clone()));
        }
        if let Some(exclusive) = self.exclusive {
            out.push(("exclusive", exclusive.to_string()));
        }
        if let Some(level) = self.log_level {
            out.push(("log_level", value_name(&level)));
        }
        if let Some(check) = self.update_check {
            out.push(("update_check", check.to_string()));
        }
        if let Some(size) = self.cache_size {
            out.push(("cache_size", size.to_string()));
        }
        out
    }
}

/// How clap spells this variant on the command line.
fn value_name<T: ValueEnum>(v: &T) -> String {
    v.to_possible_value()
        .map_or_else(String::new, |p| p.get_name().to_string())
}

impl Note {
    fn warn(text: String) -> Self {
        Self {
            level: log::Level::Warn,
            text,
        }
    }
}

/// Read the settings at `path`.
///
/// Never fails. A file that is absent, unreadable or half-written yields the
/// defaults and a note saying so: it is the kind of thing set once in a launcher
/// and forgotten, and a typo in it must not cost the user their music player.
#[must_use]
pub fn load(path: &str) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut loaded = parse(&text);
            for note in &mut loaded.notes {
                note.text = format!("{path} {}", note.text);
            }
            loaded
        }
        // Absence is the ordinary state on a first run, so it is not a warning.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded {
            settings: Settings::default(),
            notes: vec![Note {
                level: log::Level::Info,
                text: format!("no settings file at {path}; using the defaults"),
            }],
        },
        Err(e) => Loaded {
            settings: Settings::default(),
            notes: vec![Note::warn(format!(
                "{path} could not be read ({e}); using the defaults"
            ))],
        },
    }
}

/// Parse the text of a settings file.
///
/// Pure, so the whole of the negative space - an unknown key, a value that does
/// not parse, a line that is not an assignment at all - is testable without a
/// filesystem. **A bad line is dropped and the rest of the file still applies:**
/// all-or-nothing would let one stale key throw away three good settings.
#[must_use]
pub fn parse(text: &str) -> Loaded {
    let mut settings = Settings::default();
    let mut notes = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let Some((key, value)) = assignment(raw) else {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                notes.push(Note::warn(format!(
                    "line {line_no} is not `key = value` and was ignored: {trimmed}"
                )));
            }
            continue;
        };
        match key {
            "theme" => set_once(
                &mut settings.theme,
                ThemeName::from_str(value, true).ok(),
                key,
                value,
                line_no,
                &mut notes,
            ),
            "device" => set_once(
                &mut settings.device,
                (!value.is_empty()).then(|| value.to_string()),
                key,
                value,
                line_no,
                &mut notes,
            ),
            "exclusive" => set_once(
                &mut settings.exclusive,
                parse_bool(value),
                key,
                value,
                line_no,
                &mut notes,
            ),
            "log_level" => set_once(
                &mut settings.log_level,
                LogLevel::from_str(value, true).ok(),
                key,
                value,
                line_no,
                &mut notes,
            ),
            "update_check" => set_once(
                &mut settings.update_check,
                parse_bool(value),
                key,
                value,
                line_no,
                &mut notes,
            ),
            "cache_size" => set_once(
                &mut settings.cache_size,
                parse_u64(value),
                key,
                value,
                line_no,
                &mut notes,
            ),
            _ => notes.push(Note::warn(format!(
                "line {line_no} sets `{key}`, which priel knows nothing about; ignoring it"
            ))),
        }
    }
    Loaded { settings, notes }
}

/// The key and value on this line, or `None` if it carries neither.
///
/// A `#` only starts a comment at the start of a line: a device identifier comes
/// from the sound server and is allowed to contain one, so there are no inline
/// comments to strip.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// Take the first value a key is given, and say so about any later one.
///
/// First-wins is also what the writer edits, so a read and a write agree about
/// which of two lines is the live one.
fn set_once<T>(
    field: &mut Option<T>,
    parsed: Option<T>,
    key: &str,
    value: &str,
    line_no: usize,
    notes: &mut Vec<Note>,
) {
    if field.is_some() {
        notes.push(Note::warn(format!(
            "line {line_no} sets `{key}` a second time; the first one stands"
        )));
        return;
    }
    match parsed {
        Some(v) => *field = Some(v),
        None => notes.push(Note::warn(format!(
            "line {line_no}: `{value}` is not a value `{key}` accepts; ignoring the line"
        ))),
    }
}

/// `true` or `false`, and nothing else.
///
/// Deliberately strict rather than accepting `yes`, `on` and `1` as well: the
/// file names the two words it wants, and a rejected line says which they are.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// A whole non-negative number, or `None` for anything else - a malformed
/// `cache_size` line is dropped and named rather than stopping priel starting.
fn parse_u64(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

/// The text of a settings file with `chosen` applied to it.
///
/// **A line edit, not a serialisation.** The line whose key matches is replaced
/// where it stands, a key that is not there is appended, and every other line -
/// comments, blank lines, spacing, and keys priel does not recognise - comes
/// back byte for byte. A user's file has to survive being written to.
///
/// Pure, so the promise above is a test rather than an intention.
#[must_use]
pub fn merge(existing: &str, chosen: &Settings) -> String {
    let wanted = chosen.assignments();
    let mut done: Vec<&str> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for raw in existing.lines() {
        let replacement = assignment(raw)
            .map(|(key, _)| key)
            .filter(|key| !done.contains(key))
            .and_then(|key| wanted.iter().find(|(k, _)| *k == key));
        match replacement {
            Some((key, value)) => {
                done.push(key);
                out.push(format!("{key} = {value}"));
            }
            None => out.push(raw.to_string()),
        }
    }
    for (key, value) in &wanted {
        if !done.contains(key) {
            out.push(format!("{key} = {value}"));
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Write `chosen` into the file at `path`, keeping everything else in it.
///
/// Called by `main` alone, once, after the event loop has ended: a `write(2)`
/// belongs nowhere near the render thread, which is the same rule that gives the
/// log a writer thread of its own.
///
/// # Errors
/// If the file exists but cannot be read, if its directory cannot be created, or
/// if the write itself fails. **A file that cannot be read is not written**:
/// overwriting what priel could not inspect is how a hand-written file would be
/// lost.
pub fn save(path: &str, chosen: &Settings) -> Result<()> {
    if chosen.is_empty() {
        return Ok(());
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HEADER.to_string(),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!("reading {path} before writing it")));
        }
    };
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, merge(&existing, chosen)).with_context(|| format!("writing {path}"))
}

#[cfg(test)]
mod tests {
    use super::{Loaded, Note, Settings, default_path, load, merge, parse, save};
    use crate::cli::{LogLevel, ThemeName};

    /// A directory under the system temporary directory, never under `$HOME`.
    ///
    /// Nothing a test constructs may write to the user's home directory, and the
    /// bug that rule was written for reached a real machine. Each test names its
    /// own directory so the suite can run in parallel.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("priel-settings-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a writable temporary directory");
            Self(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn warnings(loaded: &Loaded) -> Vec<&str> {
        loaded
            .notes
            .iter()
            .filter(|n| n.level == log::Level::Warn)
            .map(|n| n.text.as_str())
            .collect()
    }

    #[test]
    fn the_four_settings_a_flag_can_set_are_read_back_from_a_file() {
        // Goal: the whole point of the file. Each value is spelled exactly as
        // the flag spells it, because both sides go through the same clap
        // ValueEnum - `gruvbox-dark`, never `GruvboxDark`.
        let loaded = parse(
            "theme = gruvbox-dark\ndevice = pipewire/dac\nexclusive = true\nlog_level = debug\n",
        );
        assert_eq!(loaded.settings.theme, Some(ThemeName::GruvboxDark));
        assert_eq!(loaded.settings.device.as_deref(), Some("pipewire/dac"));
        assert_eq!(loaded.settings.exclusive, Some(true));
        assert_eq!(loaded.settings.log_level, Some(LogLevel::Debug));
        assert!(warnings(&loaded).is_empty(), "{:?}", loaded.notes);
    }

    #[test]
    fn comments_blank_lines_and_loose_spacing_are_all_ordinary() {
        // Goal: this is a file a human is invited to edit, so it has to tolerate
        // being written by hand. A comment is the reason the format is not JSON.
        let loaded = parse("# a comment\n\n   theme=dracula   \n\t# indented comment\n");
        assert_eq!(loaded.settings.theme, Some(ThemeName::Dracula));
        assert!(warnings(&loaded).is_empty(), "{:?}", loaded.notes);
    }

    #[test]
    fn a_device_identifier_may_contain_a_hash_and_an_equals_sign() {
        // Goal: the value is an opaque string from the sound server, not
        // something priel gets to have opinions about. `#` therefore only starts
        // a comment at the start of a line, and only the first `=` splits.
        let loaded = parse("device = alsa/hw:CARD=AUDIO,DEV=0 #2\n");
        assert_eq!(
            loaded.settings.device.as_deref(),
            Some("alsa/hw:CARD=AUDIO,DEV=0 #2")
        );
    }

    #[test]
    fn a_malformed_line_is_dropped_and_the_rest_of_the_file_still_applies() {
        // Goal: the rule that already governs $PRIEL_LOG. A typo gets set once
        // and forgotten, and it must not be able to cost someone their music
        // player - nor throw away the three settings either side of it.
        let loaded = parse(
            "theme = chartreuse\nnonsense\ndevice = pipewire/dac\nvolume = 11\nlog_level = info\n",
        );
        assert_eq!(loaded.settings.theme, None, "the bad value is not applied");
        assert_eq!(
            loaded.settings.device.as_deref(),
            Some("pipewire/dac"),
            "and the good lines around it still are"
        );
        assert_eq!(loaded.settings.log_level, Some(LogLevel::Info));
        let warned = warnings(&loaded);
        assert_eq!(warned.len(), 3, "each bad line is named: {warned:?}");
        assert!(warned[0].contains("line 1"), "{warned:?}");
        assert!(warned[0].contains("chartreuse"), "{warned:?}");
        assert!(warned[1].contains("line 2"), "{warned:?}");
        assert!(warned[2].contains("volume"), "{warned:?}");
    }

    #[test]
    fn what_moves_on_its_own_is_no_setting_and_the_file_says_so() {
        // Goal: the ADR's own membership test, applied rather than assumed.
        // "Can a flag already set it, and does it mean the same thing on the
        // next start?" - no flag says how the queue repeats, whether it is
        // shuffled or whether the radio follows it, and the exclusion list names
        // "anything that moves on its own" for exactly this. A file carrying one
        // of these is a stale line, so it is dropped and named rather than
        // silently obeyed.
        let loaded = parse("repeat = all\nshuffle = true\ntheme = nord\n");
        assert_eq!(
            loaded.settings,
            Settings {
                theme: Some(ThemeName::Nord),
                ..Settings::default()
            }
        );
        let warned = warnings(&loaded);
        assert_eq!(warned.len(), 2, "each is named: {warned:?}");
        assert!(warned[0].contains("repeat"), "{warned:?}");
        assert!(warned[1].contains("shuffle"), "{warned:?}");
    }

    #[test]
    fn an_empty_value_is_no_value_at_all() {
        // Goal: `device =` reads as "I have deleted this", not as a device
        // called the empty string, which mpv would refuse to open.
        let loaded = parse("device =\n");
        assert_eq!(loaded.settings.device, None);
        assert_eq!(warnings(&loaded).len(), 1, "{:?}", loaded.notes);
    }

    #[test]
    fn exclusive_takes_only_the_two_words_the_header_names() {
        // Goal: strictness here is kindness. `exclusive = yes` is rejected with
        // a line that says which words are accepted, rather than being guessed
        // at - taking a device from every other application is not a guess.
        assert_eq!(parse("exclusive = false\n").settings.exclusive, Some(false));
        let loaded = parse("exclusive = yes\n");
        assert_eq!(loaded.settings.exclusive, None);
        assert!(warnings(&loaded)[0].contains("yes"), "{:?}", loaded.notes);
    }

    #[test]
    fn the_update_check_key_reads_a_boolean_and_rejects_anything_else() {
        // Goal: the one setting priel reads that no picker writes - a user turns
        // the update check off by hand. It parses like `exclusive`, and a value
        // that is not a boolean is dropped with a warning rather than guessed.
        assert_eq!(
            parse("update_check = false\n").settings.update_check,
            Some(false)
        );
        assert_eq!(
            parse("update_check = true\n").settings.update_check,
            Some(true)
        );
        let loaded = parse("update_check = sometimes\n");
        assert_eq!(loaded.settings.update_check, None);
        assert!(
            warnings(&loaded)[0].contains("sometimes"),
            "{:?}",
            loaded.notes
        );
    }

    #[test]
    fn the_cache_size_key_reads_a_number_and_rejects_anything_else() {
        // Goal: the album-art cache ceiling in MiB - a whole number a user sets
        // by hand, and a non-number is dropped with a warning, not guessed.
        assert_eq!(parse("cache_size = 512\n").settings.cache_size, Some(512));
        assert_eq!(parse("cache_size = 0\n").settings.cache_size, Some(0));
        let loaded = parse("cache_size = lots\n");
        assert_eq!(loaded.settings.cache_size, None);
        assert!(warnings(&loaded)[0].contains("lots"), "{:?}", loaded.notes);
    }

    #[test]
    fn a_key_set_twice_keeps_the_first_and_says_so() {
        // Goal: first-wins is also what `merge` edits, so a read and a write
        // agree about which of the two lines is the live one. Silently taking
        // the last would make the writer change a line the reader ignores.
        let loaded = parse("theme = nord\ntheme = dracula\n");
        assert_eq!(loaded.settings.theme, Some(ThemeName::Nord));
        assert_eq!(warnings(&loaded).len(), 1, "{:?}", loaded.notes);
    }

    #[test]
    fn an_empty_file_is_not_a_complaint() {
        // Goal: a file someone commented out entirely is a valid answer, and
        // means exactly what no file means.
        let loaded = parse("# nothing to see\n\n");
        assert!(loaded.settings.is_empty());
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    }

    #[test]
    fn a_missing_file_is_normal_and_an_unreadable_one_is_a_warning() {
        // Goal: priel starts either way - that is the non-negotiable one. The
        // difference is only how loudly it is recorded: no file at all is the
        // ordinary first run, and a file that exists but will not open is worth
        // a line in the log.
        let dir = TempDir::new("missing");
        let loaded = load(&dir.path("nothing-here.conf"));
        assert!(loaded.settings.is_empty());
        assert_eq!(loaded.notes.len(), 1);
        assert_eq!(loaded.notes[0].level, log::Level::Info, "not a warning");

        // A directory where a file should be: the read fails with something
        // other than "not found", which is the unreadable case.
        let loaded = load(&dir.path(""));
        assert!(loaded.settings.is_empty());
        assert_eq!(warnings(&loaded).len(), 1, "{:?}", loaded.notes);
    }

    #[test]
    fn a_note_from_a_file_names_the_file() {
        // Goal: the log has to say which file to go and fix. A line number on
        // its own is useless when the path is the thing being got wrong.
        let dir = TempDir::new("named");
        let path = dir.path("settings.conf");
        std::fs::write(&path, "theme = chartreuse\n").expect("write");
        let loaded = load(&path);
        assert!(warnings(&loaded)[0].contains(&path), "{:?}", loaded.notes);
    }

    #[test]
    fn writing_replaces_the_line_it_owns_and_touches_nothing_else() {
        // Goal: the promise that makes writing safe at all. A hand-edited file
        // comes back as the user's file - their comments, their spacing, their
        // ordering, and any key priel has never heard of.
        let existing = "\
# my notes
log_level = debug

theme = nord
# a key from a future version
oversampling = 4
";
        let out = merge(
            existing,
            &Settings {
                theme: Some(ThemeName::Dracula),
                ..Settings::default()
            },
        );
        assert_eq!(
            out,
            "\
# my notes
log_level = debug

theme = dracula
# a key from a future version
oversampling = 4
",
            "only the theme line moved"
        );
    }

    #[test]
    fn a_setting_the_file_does_not_carry_is_appended() {
        // Goal: the first time a device is chosen there is no line to edit.
        let out = merge(
            "theme = nord\n",
            &Settings {
                device: Some("pipewire/dac".into()),
                exclusive: Some(true),
                ..Settings::default()
            },
        );
        assert_eq!(
            out,
            "theme = nord\ndevice = pipewire/dac\nexclusive = true\n"
        );
    }

    #[test]
    fn only_the_first_of_two_lines_for_a_key_is_edited() {
        // Goal: `parse` takes the first, so the writer edits the first. Editing
        // the second would write a value the next start would not read.
        let out = merge(
            "theme = nord\ntheme = dracula\n",
            &Settings {
                theme: Some(ThemeName::OneDark),
                ..Settings::default()
            },
        );
        assert_eq!(out, "theme = one-dark\ntheme = dracula\n");
    }

    #[test]
    fn what_was_written_is_what_is_read_back() {
        // Goal: the round trip, through a real file. Anything else is a pair of
        // functions that agree with each other and not with the disk.
        let dir = TempDir::new("roundtrip");
        let path = dir.path("nested/settings.conf");
        let chosen = Settings {
            theme: Some(ThemeName::TokyoNight),
            device: Some("alsa/hw:CARD=AUDIO,DEV=0".into()),
            exclusive: Some(true),
            log_level: None,
            update_check: None,
            cache_size: None,
        };
        save(&path, &chosen).expect("the directory is created on the way");
        let back = load(&path);
        assert_eq!(back.settings, chosen);
        assert!(warnings(&back).is_empty(), "{:?}", back.notes);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.starts_with("# priel settings"),
            "a file priel created explains itself: {text}"
        );
    }

    #[test]
    fn a_session_that_chose_nothing_never_opens_the_file() {
        // Goal: a run in which no picker was used must leave the file alone -
        // including not creating one. Writing on every exit would touch a
        // hand-edited file for no reason at all.
        let dir = TempDir::new("untouched");
        let path = dir.path("settings.conf");
        save(&path, &Settings::default()).expect("nothing to do is not a failure");
        assert!(
            !std::path::Path::new(&path).exists(),
            "no choice was made, so there is nothing to write"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_written_over() {
        // Goal: overwriting what priel could not inspect is how a hand-written
        // file would be lost. A directory in the file's place is the reachable
        // spelling of "this cannot be read".
        let dir = TempDir::new("unreadable");
        let path = dir.path("settings.conf");
        std::fs::create_dir_all(&path).expect("a directory where the file should be");
        let err = save(
            &path,
            &Settings {
                theme: Some(ThemeName::Nord),
                ..Settings::default()
            },
        )
        .expect_err("the read fails, so the write must not happen");
        assert!(format!("{err:#}").contains(&path), "{err:#}");
        assert!(
            std::path::Path::new(&path).is_dir(),
            "and what was there is still there"
        );
    }

    #[test]
    fn the_default_path_is_the_xdg_config_directory() {
        // Goal: settings are a preference, so they live under XDG_CONFIG_HOME -
        // unlike the session and the client key, which are runtime state and
        // stay under XDG_STATE_HOME. This is the one function that resolves a
        // path, and only `main` calls it.
        let path = default_path();
        assert!(path.ends_with("/priel/settings.conf"), "{path}");
        assert!(
            !path.starts_with(&priel_core::auth::state_dir()),
            "a preference is not state: {path}"
        );
    }

    #[test]
    fn a_note_is_only_a_note() {
        // Goal: the module reads the file before the logger exists, so it hands
        // back what it would have said instead of saying it. Levels are carried
        // because "no file yet" and "this file is broken" are not the same news.
        let note = Note::warn("something".into());
        assert_eq!(note.level, log::Level::Warn);
    }
}
