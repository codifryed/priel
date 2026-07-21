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
    /// For example `pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0`
    /// for the sound server's entry for a card, or `alsa/hw:CARD=AUDIO,DEV=0`
    /// for the card itself. Omit to use the system default sink.
    #[arg(long, value_name = "MPV_DEVICE")]
    pub device: Option<String>,

    /// Open the output device exclusively, for priel alone
    ///
    /// Takes the device out of the sound server's graph, so nothing else on the
    /// machine can play through it and no other application can reshape the
    /// chain underneath you. Use it alongside `--device`; it is orthogonal to
    /// which device is chosen, and priel never selects it on its own.
    ///
    /// If the device will not open exclusively - usually because something else
    /// already holds it - priel says so, records it in the log, and keeps
    /// playing: a hardware device falls back to the sound server's own entry
    /// for the same card, which is the same DAC shared, and to the default sink
    /// when there is no such entry. The track restarts from the beginning. The
    /// output badge then reports shared output; it never claims exclusivity it
    /// did not get. `x` in the device picker toggles the same thing for one
    /// session.
    #[arg(long)]
    pub exclusive: bool,

    /// List the audio output devices and exit
    ///
    /// Prints each device's identifier and description, one per line. The
    /// identifier is exactly what `--device` takes.
    ///
    /// The direct hardware devices are in the listing too, marked `(direct
    /// hardware access)`. ALSA advertises only the plugin spellings of a card,
    /// so nothing enumerates those; priel builds them from the kernel's own
    /// card listing instead. They are the ones `--exclusive` is worth asking
    /// for, and the picker (`d`) offers exactly the same set.
    #[arg(long)]
    pub list_devices: bool,

    /// Colour theme
    ///
    /// Defaults to `nord`, a dark theme. `terminal` uses your terminal's own
    /// sixteen colours instead of priel's, which is the one to pick if you have
    /// already themed your terminal or if its background is light and you would
    /// rather priel followed it.
    ///
    /// `t` opens the same list at runtime, but priel reads no configuration
    /// file, so a choice made there lasts for that session only. This flag is
    /// how one is kept.
    #[arg(long, value_name = "THEME")]
    pub theme: Option<ThemeName>,

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

/// Which built-in palette priel paints with.
///
/// Lives here rather than beside the palettes themselves for the same reason
/// [`LogLevel`] does: deriving the enum on the command definition is what puts
/// the choices into `--help`, into the man page and into all three completions,
/// and the asset generator compiles this module alone. `priel_tui::theme` maps
/// a name onto the colours.
///
/// The order is the order the runtime picker lists them in, so the flag and the
/// picker cannot disagree about what is on offer.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeName {
    /// Dark. The default.
    #[default]
    Nord,
    /// Dark, warm.
    GruvboxDark,
    /// Light, warm.
    GruvboxLight,
    /// Light, cool.
    OneLight,
    /// Dark, vivid.
    Dracula,
    /// Dark, cool.
    OneDark,
    /// Your terminal's own sixteen colours
    Terminal,
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

    /// How much of its own log to ask mpv for, in mpv's level names.
    ///
    /// mpv answers the other half of a playback question - what the decoder and
    /// the audio output made of a track - and its messages are recorded in the
    /// same file, in order, rather than in one of their own. One `--log-level`
    /// therefore sets both: asking mpv for more than priel will record is work
    /// mpv does for nothing, and asking for less loses the half that explains a
    /// failure.
    ///
    /// The names are not quite priel's. mpv's `v` (verbose) sits between info
    /// and debug and is what priel's `debug` maps onto; mpv's own `debug` and
    /// `trace` go down to individual packets, which would bury everything else.
    #[must_use]
    pub fn mpv_log_level(&self) -> Option<String> {
        let level = match self.log_level() {
            LevelFilter::Off => return None,
            LevelFilter::Error => "error",
            LevelFilter::Warn => "warn",
            LevelFilter::Info => "info",
            LevelFilter::Debug => "v",
            LevelFilter::Trace => "debug",
        };
        Some(level.to_string())
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
