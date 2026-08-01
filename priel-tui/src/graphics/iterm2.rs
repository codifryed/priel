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

//! iTerm2's inline images: hand over a picture *file*, in the cell grid.
//!
//! Spoken by iTerm2, mintty, Rio, Warp, Tabby and `WezTerm`. The picture is a
//! file rather than pixels - and priel already has one, because the cover
//! arrives from the service as a JPEG and is kept alongside the decoded pixels
//! the half-block fallback needs. So this costs a base64 and nothing else: no
//! encoder, no image crate, no re-compression.
//!
//! Unlike kitty's, these pictures have no id. They are written into the cell
//! grid like text, which has two consequences worth keeping in mind: the
//! renderer painting those cells is what erases one, and moving one means
//! writing the whole thing again.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The cover's own bytes, base64'd once so a re-place costs only the header.
///
/// Separate from [`show`] because this is the expensive half and it does not
/// depend on where the picture goes - the same split kitty's transmit-and-place
/// makes, for the same reason.
#[must_use]
pub fn encode(jpeg: &[u8]) -> String {
    STANDARD.encode(jpeg)
}

/// The escape that draws a base64'd picture in a cell box at the cursor.
///
/// `preserveAspectRatio=1` because the box is laid out square by the caller and
/// a cover that is not quite square should letterbox rather than stretch.
///
/// The payload is terminated with `BEL` rather than `ESC \`: both are legal, and
/// the terminals that speak this dialect are more consistent about the former.
#[must_use]
pub fn show(base64_jpeg: &str, cols: u16, rows: u16) -> Vec<u8> {
    if base64_jpeg.is_empty() {
        return Vec::new();
    }
    format!(
        "\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1:{base64_jpeg}\x07"
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{encode, show};

    #[test]
    fn the_covers_own_jpeg_goes_out_with_no_re_encoding() {
        // Goal: the reason this protocol is free. The service sends a JPEG, the
        // cache keeps a JPEG, and this dialect wants a picture file - so the
        // bytes that arrived are the bytes that go out, base64'd and otherwise
        // untouched.
        assert_eq!(encode(b"ABC"), "QUJD");
    }

    #[test]
    fn the_box_is_named_in_cells_and_the_shape_is_kept() {
        // Goal: the picture is drawn into the cell grid, so the size is given in
        // cells and the terminal fits the picture to them. Without
        // `preserveAspectRatio` a cover that is not exactly square is stretched
        // to the box rather than letterboxed inside it.
        let out = String::from_utf8(show("QUJD", 40, 20)).expect("utf-8");
        assert_eq!(
            out,
            "\x1b]1337;File=inline=1;width=40;height=20;preserveAspectRatio=1:QUJD\x07"
        );
    }

    #[test]
    fn a_picture_with_no_bytes_is_not_drawn_at_all() {
        // Goal: the same rule the other two keep. An absent cover leaves the
        // space it would have taken rather than drawing an empty box, and an
        // escape with no payload is a parse error at the far end.
        assert!(show("", 40, 20).is_empty());
    }
}
