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

use clap::Parser;

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

    /// hiresTI PKCE token file
    ///
    /// Defaults to `$XDG_CONFIG_HOME/hiresti/hiresti_token.json`, falling back to
    /// `~/.config` when `XDG_CONFIG_HOME` is unset.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<String>,
}

impl Cli {
    /// The token path to use, resolving the default only when none was given.
    ///
    /// Deliberately *not* a clap `default_value_t`: that is evaluated when the
    /// command is built, so the generated man page would bake in the home
    /// directory of whichever machine ran the build.
    #[must_use]
    // The asset generator includes this module for the command definition alone
    // and never resolves a path, so this is dead code in that binary only.
    #[cfg_attr(not(test), allow(dead_code, reason = "unused by the asset generator"))]
    pub fn token_path(&self) -> String {
        self.token_file
            .clone()
            .unwrap_or_else(priel_core::Client::default_token_path)
    }
}
