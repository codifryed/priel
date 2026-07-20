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

//! The command line, defined once.
//!
//! Both the binary and the asset generator include this module, so the man page
//! and the shell completions cannot drift from what the binary actually accepts.

use clap::{Parser, ValueEnum};
use log::LevelFilter;

/// Shown at the foot of `--help` and in the man page. This is a distribution
/// requirement, not decoration - see the packaging notes in the README.
pub const DISCLAIMER: &str = "priel is unofficial software. It is not affiliated with, endorsed by, \
or sponsored by TIDAL or Aspiro AB. TIDAL is a trademark of its respective owner. A subscription \
is required; priel neither circumvents access controls nor exports content for offline use.";

#[derive(Parser, Debug)]
#[command(
    name = "priel",
    version,
    about = "Hi-res terminal client for TIDAL, with VIM keys and full mouse support",
    after_help = DISCLAIMER,
    after_long_help = DISCLAIMER
)]
pub struct Cli {
    /// Audio output device, passed through to mpv
    ///
    /// For example `pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0`.
    /// Omit to use the system default sink.
    #[arg(long, value_name = "MPV_DEVICE")]
    pub device: Option<String>,

    /// Detail recorded in the diagnostic log
    ///
    /// Defaults to `warn`. `$PRIEL_LOG` sets it too, for launching from a
    /// desktop entry; the flag wins when both are given.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Diagnostic log file
    ///
    /// Defaults to `$XDG_STATE_HOME/priel/priel.log`, falling back to
    /// `~/.local/state` when `XDG_STATE_HOME` is unset. Truncated at startup.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<String>,
}

/// How much detail the diagnostic log carries.
///
/// A parallel to `log::LevelFilter` because that type is not a clap `ValueEnum`,
/// and deriving the enum is what gets the six values into the man page and all
/// three completions.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => Self::Off,
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

// The asset generator includes this module for the command definition alone and
// never resolves anything, so these are dead code in that binary only.
#[cfg_attr(not(test), allow(dead_code, reason = "unused by the asset generator"))]
impl Cli {
    /// The level to log at, from the flag, the environment or the default.
    #[must_use]
    pub fn log_level(&self) -> LevelFilter {
        Self::resolve_level(self.log_level, std::env::var("PRIEL_LOG").ok().as_deref())
    }

    /// The flag wins over the environment, and an unrecognised `$PRIEL_LOG`
    /// falls back to the default rather than refusing to start: it is the kind
    /// of thing that gets set once in a launcher and forgotten, and a typo there
    /// must not cost the user their music player.
    ///
    /// Takes the environment as a parameter so it can be tested without
    /// mutating the process's own.
    pub(crate) fn resolve_level(flag: Option<LogLevel>, env: Option<&str>) -> LevelFilter {
        flag.or_else(|| env.and_then(|v| LogLevel::from_str(v, true).ok()))
            .unwrap_or(LogLevel::Warn)
            .into()
    }

    /// Where mpv should write its own log, if anywhere.
    ///
    /// mpv's log answers a different question from priel's - what the decoder
    /// and the audio output made of a track - and it is verbose enough that it
    /// is only kept when someone is deliberately looking, so it follows
    /// `--log-level debug`. It is a separate file because mpv writes it itself,
    /// in its own format; two writers cannot share one file. It sits beside
    /// priel's own log so that attaching one to a bug report picks up both.
    #[must_use]
    pub fn mpv_log_file(&self) -> Option<String> {
        if self.log_level() < LevelFilter::Debug {
            return None;
        }
        let own = self.log_path();
        let beside = std::path::Path::new(&own).with_file_name("priel-mpv.log");
        Some(beside.to_string_lossy().into_owned())
    }

    /// The log path to use, resolving the default only when none was given.
    ///
    /// Deliberately *not* a clap `default_value_t`: that is evaluated when the
    /// command is built, so the generated man page would bake in the home
    /// directory of whichever machine ran the build.
    #[must_use]
    pub fn log_path(&self) -> String {
        self.log_file
            .clone()
            .unwrap_or_else(|| format!("{}/priel.log", priel_core::auth::state_dir()))
    }
}
