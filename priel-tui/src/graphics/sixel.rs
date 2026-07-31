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

//! Sixel: a palette-quantised raster, for the terminals that speak nothing else.
//!
//! foot, xterm built for it, mlterm, contour, Windows Terminal. foot is the
//! reason this is here at all - it is a common Wayland terminal and sixel is the
//! only picture it takes.
//!
//! The one protocol here that needs an encoder, because sixel is at most 256
//! colours and the picture has to be quantised down to them. `icy_sixel` does
//! that in one crate with no dependencies of its own, which is what made it
//! worth having at all - the alternative was a median-cut quantiser of our own,
//! or `image`, which costs the best part of eighty crates.
//!
//! **Sixel has no scaling parameter.** The picture is a raster at a fixed pixel
//! size and occupies whatever cells that comes to, where the other two protocols
//! take a cell box and scale into it. So the caller has to scale *before*
//! encoding, and a box of a different pixel size needs the whole thing encoded
//! again - which is what [`Payload::fits`](super::Payload::fits) exists to say.

use icy_sixel::{DiffusionMethod, MethodForLargest, MethodForRep, PixelFormat, Quality};

use crate::art::Image;

/// The picture as a sixel raster, at its own pixel size.
///
/// `RGB888` is exactly what [`Image`] holds, so the pixels go straight in.
///
/// The knobs are chosen for a small picture on a dark terminal, once per track:
/// `Atkinson` diffusion because it keeps a cover's gradients from banding
/// without the noise Floyd-Steinberg leaves at this size, and `HIGH` quality
/// because this runs on the worker, off the render thread, and a cover is worth
/// the extra pass over a palette that is going to be looked at for minutes.
///
/// `None` when the picture is empty, or when the encoder cannot make sense of
/// it. A failure here is not worth a banner - the half blocks are still there
/// and the listener gets a mosaic instead of a photograph.
#[must_use]
pub fn encode(image: &Image) -> Option<Vec<u8>> {
    if image.width == 0 || image.height == 0 {
        return None;
    }
    let width = i32::try_from(image.width).ok()?;
    let height = i32::try_from(image.height).ok()?;
    // The encoder indexes the buffer from the dimensions it is given, so a
    // buffer that does not reach is a panic rather than a short picture. `Image`
    // documents that a truncated decode is possible, so it is checked here.
    if image.rgb.len() < image.width.saturating_mul(image.height).saturating_mul(3) {
        return None;
    }
    icy_sixel::sixel_string(
        &image.rgb,
        width,
        height,
        PixelFormat::RGB888,
        DiffusionMethod::Atkinson,
        MethodForLargest::Auto,
        MethodForRep::Auto,
        Quality::HIGH,
    )
    .ok()
    .map(String::into_bytes)
}

#[cfg(test)]
mod tests {
    use super::encode;
    use crate::art::Image;

    fn image(width: usize, height: usize) -> Image {
        // A gradient rather than a flat fill: a single-colour picture quantises
        // to one palette entry and would pass a test that a real cover fails.
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let v = u8::try_from((x * 8 + y * 4) % 256).unwrap_or(0);
                rgb.extend_from_slice(&[v, 255 - v, v / 2]);
            }
        }
        Image { width, height, rgb }
    }

    #[test]
    fn a_picture_comes_back_as_a_sixel_raster() {
        // Goal: the encoder is reached and produces the thing the terminal
        // parses - the DCS introducer, and the string terminator that ends it.
        // A sixel missing either is printed as text rather than drawn.
        let out = encode(&image(24, 24)).expect("a picture encodes");
        let text = String::from_utf8(out).expect("sixel is ascii");
        assert!(
            text.starts_with("\x1bP"),
            "the DCS introducer: {:?}",
            &text[..8]
        );
        assert!(text.ends_with("\x1b\\"), "and the terminator");
        assert!(text.contains('#'), "with a palette in it");
    }

    #[test]
    fn a_picture_with_no_pixels_encodes_to_nothing() {
        // Goal: absent is not blank, the rule the whole cover path keeps.
        assert!(encode(&image(0, 0)).is_none());
    }

    #[test]
    fn a_decode_that_stopped_early_does_not_reach_the_encoder() {
        // Goal: `Image` documents that its buffer may be shorter than its
        // dimensions claim - a truncated download draws a dark square rather
        // than panicking. The encoder indexes from the dimensions, so it would
        // panic on the render path instead. Checked here rather than trusted.
        let short = Image {
            width: 16,
            height: 16,
            rgb: vec![0; 8],
        };
        assert!(encode(&short).is_none());
    }
}
