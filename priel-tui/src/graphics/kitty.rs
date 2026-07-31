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

//! kitty's graphics protocol: transmit once against an id, then place cheaply.
//!
//! The one protocol here that separates the picture from where it goes, which is
//! what lets a resize cost a dozen bytes instead of a megabyte. Spoken by kitty,
//! Ghostty, `WezTerm`, Konsole and wayst.
//!
//! The picture is sent as **raw RGB** (`f=24`), which is exactly what
//! [`crate::art::Image`] already holds - `zune-jpeg` is asked for
//! `ColorSpace::RGB` and the buffer is `width * height * 3`, row-major, no
//! padding. No re-encoding and no image crate: the bytes go out as they are.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::art::Image;

/// How many base64 bytes go in one escape sequence.
///
/// The protocol's own limit: a chunk may not exceed 4096 bytes of payload, and
/// every chunk but the last says `m=1` to mean "more follows". A cover is
/// hundreds of these.
const CHUNK: usize = 4096;

/// Ask the terminal for no reply. **Load-bearing.**
///
/// Without it kitty writes `\x1b_G...;OK\x1b\\` back on *stdin*, where priel's
/// event loop is reading keys - crossterm hands it over as a burst of junk key
/// events, which at best does nothing and at worst presses whatever they happen
/// to spell. There is no answer priel wants to any of these, so nothing is asked
/// for.
const QUIET: &str = "q=2";

/// The bytes that hand the terminal a picture and nothing else.
///
/// Transmit only (`a=t`), so the picture is held against `id` without being
/// drawn. Where it goes is [`show`], separately, because that is the part that
/// changes when a box moves and this is the part that costs a megabyte.
///
/// An empty picture transmits nothing: a zero-by-zero image is an absence, and
/// the protocol has no spelling for one.
#[must_use]
pub fn transmit(id: u32, image: &Image) -> Vec<u8> {
    if image.width == 0 || image.height == 0 || image.rgb.is_empty() {
        return Vec::new();
    }
    let encoded = STANDARD.encode(&image.rgb);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / CHUNK * 64 + 64);
    let mut chunks = encoded.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        // The whole description goes on the first chunk and the continuations
        // carry only `m`, which is what the protocol asks for.
        if first {
            out.extend_from_slice(
                format!(
                    "\x1b_Ga=t,f=24,s={},v={},i={id},{QUIET},m={more};",
                    image.width, image.height
                )
                .as_bytes(),
            );
            first = false;
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// The bytes that draw an already-transmitted picture at the cursor.
///
/// `c` and `r` are the cell box to fit it into; the terminal does the scaling,
/// which is why the transmitted picture needs no size of its own and a resize
/// costs only this.
#[must_use]
pub fn show(id: u32, cols: u16, rows: u16) -> Vec<u8> {
    format!("\x1b_Ga=p,i={id},c={cols},r={rows},{QUIET}\x1b\\").into_bytes()
}

/// The bytes that take a picture off the screen and free it.
///
/// `d=I` rather than `d=i`: the capital deletes the data as well as the
/// placement, so a session that plays a thousand tracks does not leave a
/// thousand covers in the terminal's memory.
#[must_use]
pub fn delete(id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={id},{QUIET}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{CHUNK, delete, show, transmit};
    use crate::art::Image;

    fn image(width: usize, height: usize) -> Image {
        Image {
            width,
            height,
            rgb: vec![7; width * height * 3],
        }
    }

    #[test]
    fn a_small_picture_goes_out_in_one_escape_that_describes_it() {
        // Goal: the shape of the thing. Raw RGB, the pixel size the terminal
        // needs to make sense of the bytes, an id to refer to it by later, and
        // `m=0` because there is no more coming.
        let out = String::from_utf8(transmit(1, &image(2, 1))).expect("utf-8");
        assert!(
            out.starts_with("\x1b_Ga=t,f=24,s=2,v=1,i=1,q=2,m=0;"),
            "{out}"
        );
        assert!(out.ends_with("\x1b\\"), "{out}");
        // Six bytes of RGB, base64'd, and nothing else between the header and
        // the terminator.
        let payload = out
            .trim_start_matches("\x1b_Ga=t,f=24,s=2,v=1,i=1,q=2,m=0;")
            .trim_end_matches("\x1b\\");
        assert_eq!(payload, "BwcHBwcH", "{out}");
    }

    #[test]
    fn nothing_is_asked_of_the_terminal_that_it_would_answer_on_stdin() {
        // Goal: the event loop reads keys from stdin. Without `q=2` kitty writes
        // its `OK` back there and crossterm delivers it as a burst of key
        // events - which is a bug that only shows up on a real terminal, so it
        // is held here instead.
        for bytes in [transmit(1, &image(2, 2)), show(1, 4, 2), delete(1)] {
            let out = String::from_utf8(bytes).expect("utf-8");
            assert!(out.contains("q=2"), "no reply may be asked for: {out}");
        }
    }

    #[test]
    fn a_cover_sized_picture_is_carried_in_chunks_that_say_more_is_coming() {
        // Goal: the protocol caps a chunk at 4096 bytes of payload, and a real
        // cover is hundreds of them. Every chunk but the last says `m=1`; the
        // last says `m=0`, which is the only thing that tells the terminal to
        // draw rather than keep waiting.
        let out = String::from_utf8(transmit(9, &image(64, 64))).expect("utf-8");
        let payload_len = (64 * 64 * 3usize).div_ceil(3) * 4;
        let expected_chunks = payload_len.div_ceil(CHUNK);
        assert!(expected_chunks > 1, "the fixture has to actually split");
        assert_eq!(
            out.matches("\x1b_G").count(),
            expected_chunks,
            "one escape per chunk: {}",
            out.len()
        );
        assert_eq!(
            out.matches("m=1").count(),
            expected_chunks - 1,
            "every chunk but the last says more is coming"
        );
        assert_eq!(
            out.matches("m=0").count(),
            1,
            "and exactly one says it is done"
        );
        assert!(
            out.ends_with("m=0;") || out.contains("m=0;"),
            "the last chunk is the one that closes it"
        );
    }

    #[test]
    fn the_continuation_chunks_carry_nothing_but_the_flag() {
        // Goal: the protocol wants the description once. Repeating it on every
        // chunk is a few hundred wasted bytes per cover and, on some terminals,
        // a rejected transmission.
        let out = String::from_utf8(transmit(2, &image(64, 64))).expect("utf-8");
        assert_eq!(
            out.matches("f=24").count(),
            1,
            "the description goes on the first chunk only"
        );
        assert_eq!(out.matches("i=2").count(), 1);
    }

    #[test]
    fn a_picture_with_no_pixels_is_not_transmitted_at_all() {
        // Goal: absent and blank are different statements, and the protocol has
        // no spelling for a zero-by-zero picture - sending one is a parse error
        // at the far end rather than an empty box.
        assert!(transmit(1, &image(0, 0)).is_empty());
        assert!(
            transmit(
                1,
                &Image {
                    width: 4,
                    height: 4,
                    rgb: Vec::new()
                }
            )
            .is_empty(),
            "a decode that produced nothing is an absence too"
        );
    }

    #[test]
    fn placing_and_deleting_name_the_id_and_the_box() {
        // Goal: the two cheap halves. `show` carries the cell box the terminal
        // scales into; `delete` uses the capital `I` so the picture's data goes
        // with its placement rather than accumulating for the session.
        assert_eq!(
            String::from_utf8(show(5, 40, 20)).expect("utf-8"),
            "\x1b_Ga=p,i=5,c=40,r=20,q=2\x1b\\"
        );
        assert_eq!(
            String::from_utf8(delete(5)).expect("utf-8"),
            "\x1b_Ga=d,d=I,i=5,q=2\x1b\\"
        );
    }
}
