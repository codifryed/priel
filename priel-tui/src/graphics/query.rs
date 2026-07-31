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

//! Asking the terminal what it can do, rather than guessing from its name.
//!
//! **Environment variables cannot answer this question.** They describe the
//! terminal a session was started under, not what is between priel and the
//! screen. Measured on a real machine: kitty running a multiplexer, with
//! `KITTY_WINDOW_ID` inherited straight through into the pane. Every variable
//! said kitty, the pictures were written, the multiplexer swallowed them, and
//! the cover was a blank box - worse than the half blocks it replaced, because
//! those at least drew something.
//!
//! A query is the only thing that answers the question actually being asked,
//! which is not "what terminal is this" but "will a picture I write arrive". A
//! multiplexer that does not forward the escape does not forward the reply
//! either, so silence is the right answer and priel falls back to half blocks.
//!
//! The two halves are kept apart so the awkward one is small: [`parse_reply`] is
//! pure and holds all the knowledge, and the reading of bytes from a terminal is
//! the caller's problem.

use super::Protocol;

/// What priel writes to ask the terminal what it can do.
///
/// Two questions in one write, and the order matters:
///
/// 1. A kitty graphics query - a one-pixel image transmitted with `a=q`, which
///    asks the terminal to say whether it could take it without drawing
///    anything. `i=31` is an arbitrary id the reply quotes back so a stray `OK`
///    from something else cannot be mistaken for this one.
/// 2. A primary device attributes request, which **every** terminal answers.
///
/// The second is a fence. Without it a terminal that does not speak the first
/// would leave priel waiting for the timeout on every start, and a terminal that
/// does would still have to be waited out in case a slow reply was coming. With
/// it there is always exactly one thing to wait for, and it arrives at once.
///
/// The device attributes reply is worth having for itself: a `4` among its
/// parameters is how a terminal says it does sixel, which is a far better answer
/// than matching names against `TERM`.
#[must_use]
pub fn request() -> Vec<u8> {
    let mut out = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAAAAAAAAAA\x1b\\".to_vec();
    out.extend_from_slice(b"\x1b[c");
    out
}

/// What the terminal said it can do.
///
/// kitty's protocol wins where both are offered, for the reason
/// [`Protocol`] gives: it is the only one that can move a picture without
/// sending it again.
///
/// `None` is every terminal that answered neither - including, and this is the
/// case that matters, one whose reply never came back because something in
/// between ate the question.
#[must_use]
pub fn parse_reply(reply: &[u8]) -> Option<Protocol> {
    let text = String::from_utf8_lossy(reply);
    // The id is quoted back, so this cannot be confused with any other `OK`.
    if text.contains("_Gi=31;OK") {
        return Some(Protocol::Kitty);
    }
    // Primary device attributes: `ESC [ ? 62 ; 4 ; ... c`. A `4` among the
    // parameters is sixel. Matched as a whole parameter rather than a substring,
    // or `14` and `64` would both read as a yes.
    let da = text.split("\x1b[?").nth(1)?;
    let params = da.split('c').next()?;
    params
        .split(';')
        .any(|p| p.trim() == "4")
        .then_some(Protocol::Sixel)
}

/// Has the terminal finished answering?
///
/// The device attributes reply ends with `c` and is always last, because it was
/// asked last and terminals answer in order. This is what lets the read stop as
/// soon as the answer is complete rather than waiting out a timeout on every
/// single start.
#[must_use]
pub fn is_complete(reply: &[u8]) -> bool {
    reply.contains(&b'c')
}

#[cfg(test)]
mod tests {
    use super::{is_complete, parse_reply, request};
    use crate::graphics::Protocol;

    #[test]
    fn the_question_asks_for_a_picture_and_then_for_something_every_terminal_answers() {
        // Goal: the fence. Asking only the graphics question means a terminal
        // that does not speak it says nothing, and priel waits out the whole
        // timeout on every start. The device attributes request is answered by
        // everything, so there is always exactly one arrival to wait for.
        let out = String::from_utf8(request()).expect("utf-8");
        assert!(out.contains("a=q"), "a query, drawing nothing: {out:?}");
        assert!(out.contains("i=31"), "with an id to quote back: {out:?}");
        assert!(out.ends_with("\x1b[c"), "and the fence last: {out:?}");
    }

    #[test]
    fn a_terminal_that_says_ok_to_the_picture_gets_pictures() {
        // Goal: the reply kitty, Ghostty, WezTerm and Konsole send.
        assert_eq!(
            parse_reply(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c"),
            Some(Protocol::Kitty)
        );
    }

    #[test]
    fn a_terminal_that_only_claims_sixel_gets_sixel() {
        // Goal: `4` among the device attributes is how a terminal says sixel,
        // and it is a far better answer than matching names against TERM - it
        // is the terminal's own statement rather than a guess from its label.
        assert_eq!(
            parse_reply(b"\x1b[?62;4;22c"),
            Some(Protocol::Sixel),
            "foot and xterm built for it answer like this"
        );
    }

    #[test]
    fn a_four_inside_a_larger_number_is_not_a_claim_of_sixel() {
        // Goal: `14` and `64` are ordinary attributes and neither says sixel.
        // A substring test reads both as a yes and writes sixels at a terminal
        // that prints them as a screenful of text.
        assert_eq!(parse_reply(b"\x1b[?64;14;21c"), None);
    }

    #[test]
    fn silence_is_no_pictures_which_is_the_case_that_matters() {
        // Goal: the whole reason this exists. A multiplexer that swallows the
        // question swallows the picture too, and its own device attributes come
        // back without a graphics answer. Measured on a real machine, where
        // every environment variable said kitty and nothing was ever drawn.
        assert_eq!(parse_reply(b"\x1b[?62;22c"), None);
        assert_eq!(parse_reply(b""), None, "and nothing at all is nothing");
    }

    #[test]
    fn the_read_stops_as_soon_as_the_fence_has_arrived() {
        // Goal: a start that waited out a timeout every time would be a start
        // that felt broken. The attributes reply is asked last and ends with
        // `c`, so its arrival is the end of the answer.
        assert!(!is_complete(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(is_complete(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c"));
    }
}
