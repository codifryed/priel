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
#[allow(
    clippy::struct_excessive_bools,
    reason = "these are command-line switches, which clap models as bools; they are flags, not state"
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
    /// did not get. `x` in the device picker toggles the same thing, and what it
    /// settles on is remembered.
    #[arg(long)]
    pub exclusive: bool,

    /// Share the output device, whatever the settings file remembers
    ///
    /// The answer to `--exclusive` in the other direction, and the reason it
    /// exists: a bare boolean flag cannot say *false*, so a remembered
    /// `exclusive = true` would otherwise be unanswerable without editing the
    /// file. Every setting priel remembers has to be overridable from the
    /// command line in both directions.
    ///
    /// Given both, the last one on the line wins.
    #[arg(long, overrides_with = "exclusive")]
    pub shared: bool,

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

    /// Update priel to the latest release and exit
    ///
    /// Runs the install script, which fetches the newest release and replaces
    /// this binary. Nothing else happens: like `--list-devices`, priel does the
    /// one thing and stops rather than going on to play.
    #[arg(long)]
    pub update: bool,

    /// Do not check the forge for a newer release this run
    ///
    /// priel checks once at startup and, if a newer release exists, says so on
    /// the notice line - a single unauthenticated read of a public version
    /// number, never an automatic download. This turns that check off for the
    /// run; `update_check = false` in the settings file turns it off for good,
    /// and `$PRIEL_NO_UPDATE_CHECK` turns it off from a launcher.
    #[arg(long)]
    pub no_update_check: bool,

    /// Colour theme
    ///
    /// Defaults to `nord`, a dark theme. `terminal` uses your terminal's own
    /// sixteen colours instead of priel's, which is the one to pick if you have
    /// already themed your terminal or if its background is light and you would
    /// rather priel followed it. It is also the one palette that draws no row
    /// stripe: it cannot see the background it would be striping against.
    ///
    /// `t` opens the same list at runtime, and what is chosen there is
    /// remembered in `$XDG_CONFIG_HOME/priel/settings.conf`. This flag
    /// overrides it for one run.
    #[arg(long, value_name = "THEME")]
    pub theme: Option<ThemeName>,

    /// Log everything, for a diagnostic report (same as `--log-level debug`)
    ///
    /// The quick way to gather a log when something misbehaves: the queue, the
    /// downloader and mpv all record what they do. `-V` is `--version`, so
    /// verbose takes the conventional lowercase `-v`. An explicit `--log-level`
    /// wins over it.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Detail recorded in the diagnostic log
    ///
    /// Defaults to `warn`. `$PRIEL_LOG` sets it too, for launching from a
    /// desktop entry, and `log_level` in the settings file sets it for good.
    /// The flag wins over the environment, and both win over the file.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<LogLevel>,

    /// Diagnostic log file
    ///
    /// Defaults to `$XDG_STATE_HOME/priel/priel.log`, falling back to
    /// `~/.local/state` when `XDG_STATE_HOME` is unset. Truncated at startup.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<String>,

    /// Album-art cache ceiling, in MiB
    ///
    /// priel caches cover art under `$XDG_CACHE_HOME/priel` (falling back to
    /// `~/.cache`) so reopening a listing re-shows covers rather than
    /// re-downloading them. The cache is bounded: once it passes this size it
    /// evicts the least-recently-seen covers. Defaults to 256 MiB, plenty for a
    /// large library; `cache_size` in the settings file sets it for good.
    #[arg(long, value_name = "MIB")]
    pub cache_size: Option<u64>,
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
    /// Dark on pure black, for an OLED panel.
    TrueBlack,
    /// Dark, pastel.
    Catppuccin,
    /// Dark, indigo.
    TokyoNight,
    /// Light, indigo.
    TokyoNightDay,
    /// Your terminal's own colours; no row stripe
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
    /// The palette to paint with: the flag, then the settings file, then `nord`.
    ///
    /// `from_file` is passed in rather than read here for the same reason the
    /// environment is passed to [`Self::resolve_level`]: precedence is then a
    /// pure function, and a test needs neither a home directory nor a file.
    #[must_use]
    pub fn theme(&self, from_file: Option<ThemeName>) -> ThemeName {
        self.theme.or(from_file).unwrap_or_default()
    }

    /// The output device: the flag, then the settings file, then `None`, which
    /// is the sound server's default sink.
    ///
    /// `--device auto` is the way back to the default sink from a remembered
    /// device, so there is no third flag for it.
    #[must_use]
    pub fn device(&self, from_file: Option<String>) -> Option<String> {
        self.device.clone().or(from_file)
    }

    /// The album-art cache ceiling in bytes: the flag, else the settings file,
    /// else the default. Taken in MiB (what the flag and the file speak) and
    /// returned in bytes (what the cache is measured in).
    #[must_use]
    pub fn cache_size(&self, from_file: Option<u64>) -> u64 {
        self.cache_size
            .or(from_file)
            .unwrap_or(crate::cache::DEFAULT_CAP_MIB)
            .saturating_mul(1024 * 1024)
    }

    /// Whether to ask for the device exclusively: either flag, then the settings
    /// file, then shared.
    ///
    /// Two flags rather than one because a bare boolean cannot say *false*; clap
    /// makes them override each other, so at most one is ever set.
    #[must_use]
    pub fn exclusive(&self, from_file: Option<bool>) -> bool {
        if self.shared {
            false
        } else if self.exclusive {
            true
        } else {
            from_file.unwrap_or(false)
        }
    }

    /// Whether to check the forge for a newer release at startup.
    ///
    /// Off if `--no-update-check` is given or `$PRIEL_NO_UPDATE_CHECK` is set to
    /// anything, off if the file says `update_check = false`, on otherwise. The
    /// flag and the environment can only turn it *off* - both are per-invocation
    /// ways to say "not this time" - so there is nothing that forces it back on
    /// over a file that disabled it.
    #[must_use]
    pub fn update_check(&self, from_file: Option<bool>) -> bool {
        Self::resolve_update_check(
            self.no_update_check,
            std::env::var_os("PRIEL_NO_UPDATE_CHECK").is_some(),
            from_file,
        )
    }

    /// Pure so it is testable without the process environment or a home
    /// directory. Either per-run switch being set wins and disables the check;
    /// otherwise the file decides, defaulting to on.
    pub(crate) fn resolve_update_check(
        flag_off: bool,
        env_off: bool,
        from_file: Option<bool>,
    ) -> bool {
        if flag_off || env_off {
            return false;
        }
        from_file.unwrap_or(true)
    }

    /// The level to log at, from the flag, the environment, the file or the
    /// default.
    #[must_use]
    pub fn log_level(&self, from_file: Option<LogLevel>) -> LevelFilter {
        // `--verbose` is sugar for `--log-level debug`; an explicit `--log-level`
        // is more specific and wins over it, and both win over the environment
        // and the file.
        let flag = self.log_level.or(self.verbose.then_some(LogLevel::Debug));
        Self::resolve_level(flag, std::env::var("PRIEL_LOG").ok().as_deref(), from_file)
    }

    /// The flag wins over the environment, the environment over the file, and an
    /// unrecognised value in either falls through rather than refusing to start:
    /// they are the kind of thing that gets set once in a launcher and
    /// forgotten, and a typo there must not cost the user their music player.
    ///
    /// `$PRIEL_LOG` sits above the file because it is per-invocation, like a
    /// flag: it is how a level is raised for one run started from a desktop
    /// entry. The file is what the machine is normally set to.
    ///
    /// Takes both sources as parameters so it can be tested without mutating the
    /// process's own environment or touching a real home directory.
    pub(crate) fn resolve_level(
        flag: Option<LogLevel>,
        env: Option<&str>,
        from_file: Option<LogLevel>,
    ) -> LevelFilter {
        flag.or_else(|| env.and_then(|v| LogLevel::from_str(v, true).ok()))
            .or(from_file)
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
    ///
    /// Takes the level already resolved rather than resolving it again: there is
    /// one answer to "how much detail was asked for", and two callers deriving
    /// it separately is how the two halves of the log come to disagree.
    #[must_use]
    pub fn mpv_log_level(level: LevelFilter) -> Option<String> {
        let level = match level {
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
