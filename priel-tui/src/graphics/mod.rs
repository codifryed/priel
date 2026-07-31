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

//! Drawing the album cover as actual pixels, where the terminal can take them.
//!
//! The fallback in [`crate::art`] draws half blocks and works everywhere. This
//! is the other path: three escape-sequence protocols that hand the terminal a
//! real image. Which one a terminal speaks is [`detect`]; what to send it is one
//! module each.
//!
//! **Everything here is a pure function of what it is given**, the same shape
//! `priel_player::setup`'s file contents have, so what would go out can be read
//! in a test without a terminal to send it to - the workspace rule that nothing
//! needs a TTY.
//!
//! The division of labour is the point of the types. A [`Payload`] is the
//! picture, built once when the cover arrives and **off the render thread** -
//! base64 of a megabyte, or sixel's palette quantisation, is not work for the
//! thread that draws frames. Putting it on screen afterwards is [`place`], which
//! is a handful of bytes for two of the three protocols. That split is what lets
//! the cover cost nothing on the frames - almost all of them - where it has not
//! changed.

pub mod iterm2;
pub mod kitty;
pub mod sixel;

use std::ffi::OsString;

/// A way of putting a picture on a terminal.
///
/// Ordered by preference where a terminal speaks more than one: `WezTerm` and
/// Konsole do all three, and kitty's is the one with an image id, so it is the
/// only one that can be re-placed without resending the picture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    /// kitty's graphics protocol: kitty, Ghostty, `WezTerm`, Konsole, wayst.
    Kitty,
    /// iTerm2's inline images: iTerm2, mintty, Rio, Warp, Tabby, `WezTerm`.
    ITerm2,
    /// Sixel: foot, xterm, mlterm, contour, Windows Terminal, Konsole.
    Sixel,
}

/// Which protocol this terminal speaks, from the environment it was started in.
///
/// Read from variables rather than by asking the terminal. A query means writing
/// an escape and *reading the reply*, which is a blocking read on startup, a
/// timeout to pick, and a reply that arrives interleaved with whatever the user
/// typed. The variables are what every other terminal-graphics implementation
/// leans on in practice, and being wrong here costs a listener a picture rather
/// than anything worse - the half blocks are still there.
///
/// **Inside tmux or screen this is deliberately `None`.** Passing graphics
/// through a multiplexer needs every escape wrapped and `allow-passthrough on`
/// set, and even then the multiplexer's own redraws move and corrupt what was
/// placed. Half blocks are what works there, so half blocks are what it gets.
#[must_use]
pub fn detect(env: &Env) -> Option<Protocol> {
    if env.get("TMUX").is_some() || env.get("STY").is_some() {
        return None;
    }
    let term = env.get("TERM").unwrap_or_default().to_lowercase();
    let program = env.get("TERM_PROGRAM").unwrap_or_default().to_lowercase();

    // kitty first: it is the only one that can re-place a picture without
    // sending it again, so where a terminal speaks more than one it wins.
    if term.contains("kitty")
        || env.get("KITTY_WINDOW_ID").is_some()
        || program == "ghostty"
        || program == "wezterm"
        || env.get("KONSOLE_VERSION").is_some()
    {
        return Some(Protocol::Kitty);
    }
    if program == "iterm.app"
        || env.get("LC_TERMINAL").unwrap_or_default().to_lowercase() == "iterm2"
        || program == "rio"
        || program == "warpterminal"
        || program == "tabby"
        || term.contains("mintty")
    {
        return Some(Protocol::ITerm2);
    }
    if term.contains("foot") || term.contains("mlterm") || term.contains("contour") {
        return Some(Protocol::Sixel);
    }
    // xterm only with sixel compiled in and turned on, which it advertises by
    // being started as a VT340. A plain `xterm` gets half blocks: drawing sixels
    // at a terminal that does not take them prints the escape as text, which is
    // a screen full of rubbish rather than a missing picture.
    if term == "xterm-sixel" || term.starts_with("vt340") {
        return Some(Protocol::Sixel);
    }
    None
}

/// The environment [`detect`] reads, so it is a table of tests rather than
/// something only the real process can answer.
///
/// The same shape `priel_player::setup::conf_dir_from` uses for the same reason:
/// a lookup that reached for a global could not be tested, and a test that set
/// real environment variables would race every other test in the binary.
pub struct Env(Vec<(String, String)>);

impl Env {
    /// The variables of the running process.
    #[must_use]
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    /// A fixed set, for tests.
    #[cfg(test)]
    #[must_use]
    pub fn fixed(vars: &[(&str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

impl From<Vec<(OsString, OsString)>> for Env {
    fn from(vars: Vec<(OsString, OsString)>) -> Self {
        Self(
            vars.into_iter()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        )
    }
}

/// A picture, encoded for one protocol and ready to put on screen.
///
/// Built once, when the cover arrives, and never on the render thread: two of
/// these are a megabyte of base64 and the third is a palette quantisation.
///
/// What varies between them is what a *re-place* costs. kitty transmits the
/// picture against an id and can then put it anywhere for a few bytes. The other
/// two carry no id: the picture is written into the cell grid like text, so
/// moving it means writing it again. That is the whole reason [`place`] takes a
/// `Payload` rather than an id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Payload {
    /// Transmitted against `id` and displayed separately, so a move is cheap.
    Kitty { id: u32, transmit: Vec<u8> },
    /// The picture's bytes, already base64'd. The header naming the cell box is
    /// built at placement time, because that is the part that changes.
    ITerm2 { base64_jpeg: String },
    /// A pixel raster at the size it was encoded.
    ///
    /// **Sixel has no scaling parameter**, where the other two hand the terminal
    /// a cell box and let it fit the picture in. So this one lands at whatever
    /// cells its own pixels come to, and what makes that the right size is
    /// `COVER_FETCH_PX`: the cover is fetched at about the pixel size the box is
    /// drawn at, which is the same reason the half blocks want it that size.
    /// A terminal with unusually large cells will draw it a little small.
    Sixel { bytes: Vec<u8> },
}

/// Move the cursor to a cell, one-based as the terminal counts.
///
/// Every protocol here draws at the cursor, so this is the first thing written
/// in all three cases. ratatui leaves the cursor wherever its last write landed,
/// which is never where the picture goes.
#[must_use]
pub fn cursor_to(col: u16, row: u16) -> Vec<u8> {
    format!("\x1b[{};{}H", row.saturating_add(1), col.saturating_add(1)).into_bytes()
}

/// What to write to put `payload` on screen at a cell rect.
///
/// `transmitted` says whether the terminal already has this picture: only kitty
/// can use that, and for the other two it is ignored, because a picture with no
/// id cannot be referred to again.
#[must_use]
pub fn place(
    payload: &Payload,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    transmitted: bool,
) -> Vec<u8> {
    let mut out = cursor_to(col, row);
    match payload {
        Payload::Kitty { id, transmit } => {
            if !transmitted {
                out.extend_from_slice(transmit);
            }
            out.extend_from_slice(&kitty::show(*id, cols, rows));
        }
        Payload::ITerm2 { base64_jpeg } => {
            out.extend_from_slice(&iterm2::show(base64_jpeg, cols, rows));
        }
        Payload::Sixel { bytes, .. } => out.extend_from_slice(bytes),
    }
    out
}

/// What to write to take `payload` off the screen.
///
/// Only kitty has anything to say: its pictures live outside the cell grid and
/// stay until deleted. The other two wrote into the grid, so the renderer
/// painting those cells is already the whole of the erasure - and writing
/// something here would be a second, contradictory opinion about cells ratatui
/// owns.
#[must_use]
pub fn clear(payload: &Payload) -> Vec<u8> {
    match payload {
        Payload::Kitty { id, .. } => kitty::delete(*id),
        Payload::ITerm2 { .. } | Payload::Sixel { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Env, Payload, Protocol, clear, cursor_to, detect, place};

    #[test]
    fn each_terminal_gets_the_protocol_it_actually_speaks() {
        // Goal: the whole of the detection, as a table. Getting one wrong costs
        // a listener their picture - or, for a terminal that takes none of
        // these, a screen full of escape text, which is why the last two rows
        // matter as much as the first.
        type Case = (&'static [(&'static str, &'static str)], Option<Protocol>);
        let cases: [Case; 12] = [
            (&[("TERM", "xterm-kitty")], Some(Protocol::Kitty)),
            (&[("KITTY_WINDOW_ID", "1")], Some(Protocol::Kitty)),
            (&[("TERM_PROGRAM", "ghostty")], Some(Protocol::Kitty)),
            (&[("TERM_PROGRAM", "WezTerm")], Some(Protocol::Kitty)),
            (&[("KONSOLE_VERSION", "220400")], Some(Protocol::Kitty)),
            (&[("TERM_PROGRAM", "iTerm.app")], Some(Protocol::ITerm2)),
            (&[("LC_TERMINAL", "iTerm2")], Some(Protocol::ITerm2)),
            (&[("TERM", "foot")], Some(Protocol::Sixel)),
            (&[("TERM", "xterm-sixel")], Some(Protocol::Sixel)),
            // No image support at all: half blocks, and nothing written that a
            // terminal would print as text.
            (&[("TERM", "alacritty")], None),
            (&[("TERM", "xterm-256color")], None),
            (&[("TERM", "dumb")], None),
        ];
        for (vars, want) in cases {
            assert_eq!(detect(&Env::fixed(vars)), want, "for {vars:?}");
        }
    }

    #[test]
    fn a_multiplexer_gets_half_blocks_however_capable_the_terminal_under_it_is() {
        // Goal: tmux needs every escape wrapped and `allow-passthrough on`, and
        // even then its own redraws move and corrupt what was placed. A picture
        // that works until the pane is resized is worse than no picture, so the
        // multiplexer wins over the terminal it is running in.
        assert_eq!(
            detect(&Env::fixed(&[
                ("TERM", "xterm-kitty"),
                ("TMUX", "/tmp/tmux-1000/default,1,0")
            ])),
            None
        );
        assert_eq!(
            detect(&Env::fixed(&[
                ("TERM", "xterm-kitty"),
                ("STY", "1234.pts-0.host")
            ])),
            None
        );
    }

    #[test]
    fn the_cursor_is_moved_in_the_terminals_own_one_based_counting() {
        // Goal: ratatui counts cells from zero and the terminal counts from one,
        // and every protocol here draws wherever the cursor is. An off-by-one
        // puts the picture one cell up and left of the box drawn for it.
        assert_eq!(cursor_to(0, 0), b"\x1b[1;1H");
        assert_eq!(cursor_to(4, 9), b"\x1b[10;5H");
    }

    #[test]
    fn only_kitty_can_move_a_picture_without_sending_it_again() {
        // Goal: the reason `Payload` is an enum rather than a blob. kitty holds
        // the picture against an id, so a move is a few bytes; the other two
        // wrote into the cell grid and have to write it all again.
        let kitty = Payload::Kitty {
            id: 7,
            transmit: b"TRANSMIT".to_vec(),
        };
        let moved = place(&kitty, 0, 0, 10, 5, true);
        assert!(
            !moved.windows(8).any(|w| w == b"TRANSMIT"),
            "the picture is already there: {moved:?}"
        );
        let first = place(&kitty, 0, 0, 10, 5, false);
        assert!(
            first.windows(8).any(|w| w == b"TRANSMIT"),
            "but the first placement carries it"
        );

        let iterm = Payload::ITerm2 {
            base64_jpeg: "QUJD".into(),
        };
        assert!(
            place(&iterm, 0, 0, 10, 5, true)
                .windows(4)
                .any(|w| w == b"QUJD"),
            "a picture with no id is sent every time"
        );
    }

    #[test]
    fn taking_a_picture_off_the_screen_is_kitty_only() {
        // Goal: the other two drew into cells ratatui owns, so the renderer
        // painting over them is the erasure. Writing anything here as well would
        // be a second opinion about cells that already have one.
        assert!(
            !clear(&Payload::Kitty {
                id: 3,
                transmit: Vec::new()
            })
            .is_empty()
        );
        assert!(
            clear(&Payload::ITerm2 {
                base64_jpeg: String::new()
            })
            .is_empty()
        );
        assert!(clear(&Payload::Sixel { bytes: Vec::new() }).is_empty());
    }
}
