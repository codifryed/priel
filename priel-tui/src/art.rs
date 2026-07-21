//! Drawing a picture out of text.
//!
//! A terminal cell is one pixel wide and two tall if you paint `▀` (U+2580,
//! upper half block) with a foreground for the top pixel and a background for
//! the bottom one. Nothing but coloured text is involved, so this survives
//! `ssh`, `tmux`, `script`, and a plain text dump of a frame - which is also
//! what makes it testable here without a terminal.
//!
//! Kitty's graphics protocol and sixel both look better and were both rejected:
//! they draw outside ratatui's cell grid, so a redraw of the cells underneath
//! does not repaint them and the image tears. See `docs/cover-art.md`.
//!
//! `▀` is single width in every font that has it. That matters: this crate
//! already avoids glyphs with emoji presentation, because a terminal that paints
//! one two cells wide while `unicode-width` calls it one puts every hit box
//! after it a cell out.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// The glyph the whole module rests on: top half foreground, bottom half
/// background.
const HALF: &str = "\u{2580}";

/// A decoded picture: 8-bit RGB, row-major, no padding between rows.
///
/// Held as bytes rather than as a pixel type so that whatever decodes it can
/// hand it over without a conversion, and so this module needs no decoder in
/// its own dependency tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes. Anything shorter is treated as black, which
    /// is what makes a truncated download draw a dark square rather than panic.
    pub rgb: Vec<u8>,
}

impl Image {
    /// The pixel at `(x, y)`, or black where the buffer does not reach.
    ///
    /// Never indexes past the end: a decode that stopped early, or a `width`
    /// and `height` that disagree with the buffer, must not be able to panic the
    /// render thread. Black is the honest answer - it is what an absent pixel
    /// looks like - and the alternative, refusing to draw at all, would throw
    /// away a picture that is mostly there.
    fn pixel(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let at = (y.saturating_mul(self.width).saturating_add(x)).saturating_mul(3);
        match self.rgb.get(at..at.saturating_add(3)) {
            Some([red, green, blue]) => (*red, *green, *blue),
            _ => (0, 0, 0),
        }
    }

    /// The average colour of the source rectangle a target pixel covers.
    ///
    /// A box filter, deliberately: the cover is fetched at close to the size it
    /// is drawn at, so this averages a handful of pixels rather than resampling
    /// a large image, and a filter with a kernel would be a dependency and a
    /// decision for no visible gain at these sizes.
    fn box_average(&self, x0: usize, x1: usize, y0: usize, y1: usize) -> Color {
        let (mut red, mut green, mut blue, mut seen) = (0u32, 0u32, 0u32, 0u32);
        for y in y0..y1.max(y0.saturating_add(1)) {
            for x in x0..x1.max(x0.saturating_add(1)) {
                let (pr, pg, pb) = self.pixel(x, y);
                red += u32::from(pr);
                green += u32::from(pg);
                blue += u32::from(pb);
                seen += 1;
            }
        }
        if seen == 0 {
            return Color::Rgb(0, 0, 0);
        }
        // Truncation is the point: an average of bytes is a byte.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "each channel sum divided by its count is back within a byte"
        )]
        Color::Rgb(
            (red / seen) as u8,
            (green / seen) as u8,
            (blue / seen) as u8,
        )
    }
}

/// Draw `image` into a block `rows` tall and `cols` wide.
///
/// The result is `rows` lines of `cols` `▀` glyphs, each carrying two pixels, so
/// the picture is `cols × 2*rows` pixels. Terminal cells are about twice as tall
/// as they are wide, which makes those pixels roughly square - so a square
/// cover wants `cols == 2 * rows`. The caller decides that; this function fills
/// exactly what it is given and does not letterbox, because the one caller is
/// laying the block out itself and a second opinion here would fight it.
///
/// An empty rect, or an image with no pixels, draws nothing at all rather than
/// a black rectangle: absent and black are different statements, and a cover
/// that failed to arrive should leave the space it would have taken.
pub fn draw(image: &Image, rows: u16, cols: u16) -> Vec<Line<'static>> {
    if rows == 0 || cols == 0 || image.width == 0 || image.height == 0 {
        return Vec::new();
    }
    let cols = usize::from(cols);
    let pixel_rows = usize::from(rows).saturating_mul(2);

    (0..usize::from(rows))
        .map(|row| {
            let spans = (0..cols)
                .map(|col| {
                    let top = row.saturating_mul(2);
                    let (x0, x1) = span_of(col, cols, image.width);
                    let (ty0, ty1) = span_of(top, pixel_rows, image.height);
                    let (by0, by1) = span_of(top.saturating_add(1), pixel_rows, image.height);
                    Span::styled(
                        HALF,
                        Style::default()
                            .fg(image.box_average(x0, x1, ty0, ty1))
                            .bg(image.box_average(x0, x1, by0, by1)),
                    )
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

/// Which source pixels index `i` of `n` covers, given `len` of them.
///
/// Rounded so the last target pixel reaches the last source pixel: dividing and
/// truncating leaves the right and bottom edges of the picture unsampled, which
/// crops a cover by a row or a column at small sizes.
fn span_of(i: usize, n: usize, len: usize) -> (usize, usize) {
    let start = i.saturating_mul(len) / n;
    let end = (i.saturating_add(1)).saturating_mul(len) / n;
    (start.min(len.saturating_sub(1)), end.min(len))
}

#[cfg(test)]
mod tests {
    use super::{HALF, Image, draw, span_of};
    use ratatui::style::Color;

    /// A picture of one flat colour.
    fn flat(w: usize, h: usize, rgb: (u8, u8, u8)) -> Image {
        let mut buf = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            buf.extend_from_slice(&[rgb.0, rgb.1, rgb.2]);
        }
        Image {
            width: w,
            height: h,
            rgb: buf,
        }
    }

    /// The colours of one line, top and bottom halves separated.
    fn halves(line: &ratatui::text::Line<'_>) -> Vec<(Color, Color)> {
        line.spans
            .iter()
            .map(|s| {
                (
                    s.style.fg.unwrap_or(Color::Reset),
                    s.style.bg.unwrap_or(Color::Reset),
                )
            })
            .collect()
    }

    #[test]
    fn one_cell_carries_two_pixels() {
        // Goal: the whole technique in one assertion. A cell painted `▀` shows
        // the foreground on its top half and the background on its bottom, so a
        // block N rows tall is 2N pixels tall. Method: a two-pixel-tall picture
        // with a different colour in each row, drawn into a single row.
        let image = Image {
            width: 1,
            height: 2,
            rgb: vec![255, 0, 0, 0, 0, 255],
        };
        let out = draw(&image, 1, 1);

        assert_eq!(out.len(), 1, "one row was asked for");
        assert_eq!(out[0].spans.len(), 1, "one column was asked for");
        assert_eq!(out[0].spans[0].content, HALF, "the glyph is the half block");
        assert_eq!(
            halves(&out[0]),
            vec![(Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255))],
            "the top pixel is the foreground and the bottom one the background"
        );
    }

    #[test]
    fn the_block_is_exactly_the_size_it_was_given() {
        // Goal: the caller lays out the box and this fills what it is handed, so
        // a block that came out a row short or a column long would push the
        // text beside it out of place. Method: several shapes, including the
        // non-square ones a fold or a narrow terminal produces.
        for (rows, cols) in [(1u16, 2u16), (8, 16), (5, 10), (12, 7), (3, 40)] {
            let out = draw(&flat(64, 64, (10, 20, 30)), rows, cols);
            assert_eq!(out.len(), usize::from(rows), "rows at {rows}x{cols}");
            for line in &out {
                assert_eq!(line.spans.len(), usize::from(cols), "cols at {rows}x{cols}");
            }
        }
    }

    #[test]
    fn nothing_is_drawn_where_there_is_nothing_to_draw() {
        // Goal: absent and black are different statements. A cover that failed
        // to arrive must leave the space it would have taken rather than paint a
        // black rectangle that reads as a picture of the dark.
        assert!(draw(&flat(8, 8, (1, 2, 3)), 0, 10).is_empty(), "no rows");
        assert!(draw(&flat(8, 8, (1, 2, 3)), 10, 0).is_empty(), "no columns");
        assert!(
            draw(&flat(0, 0, (1, 2, 3)), 8, 16).is_empty(),
            "no picture either"
        );
    }

    #[test]
    fn a_truncated_download_draws_what_arrived_rather_than_panicking() {
        // Goal: the render thread may not panic over a short buffer, and a
        // decode that stopped early is the ordinary way to get one. The bytes
        // that arrived are drawn; what did not is black. Method: claim a size
        // the buffer cannot back.
        let image = Image {
            width: 4,
            height: 4,
            rgb: vec![255, 255, 255, 255, 255, 255],
        };
        let out = draw(&image, 2, 4);
        assert_eq!(out.len(), 2);
        assert_eq!(
            halves(&out[0])[0].0,
            Color::Rgb(255, 255, 255),
            "the pixels that arrived are drawn"
        );
        assert_eq!(
            halves(&out[1])[3].0,
            Color::Rgb(0, 0, 0),
            "and the rest is black rather than a panic"
        );
    }

    #[test]
    fn a_flat_picture_comes_out_flat() {
        // Goal: the box filter must not invent detail. Every cell of a single
        // colour drawn at any size is that colour, top and bottom.
        let out = draw(&flat(37, 41, (200, 100, 50)), 6, 12);
        for line in &out {
            for (fg, bg) in halves(line) {
                assert_eq!(fg, Color::Rgb(200, 100, 50));
                assert_eq!(bg, Color::Rgb(200, 100, 50));
            }
        }
    }

    #[test]
    fn the_picture_is_averaged_rather_than_sampled_at_a_point() {
        // Goal: a cover scaled down by picking one pixel per cell drops half the
        // picture and aliases hard edges into noise. The box filter is what
        // makes a shrunk cover still look like the cover. Method: a checkerboard
        // of black and white, which averages to grey and point-samples to one or
        // the other.
        let (w, h) = (16, 16);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                rgb.extend_from_slice(&[v, v, v]);
            }
        }
        let image = Image {
            width: w,
            height: h,
            rgb,
        };
        let out = draw(&image, 2, 4);
        for line in &out {
            for (fg, bg) in halves(line) {
                assert_eq!(fg, Color::Rgb(127, 127, 127), "top half is the average");
                assert_eq!(bg, Color::Rgb(127, 127, 127), "bottom half too");
            }
        }
    }

    #[test]
    fn the_last_row_and_column_of_the_source_are_reached() {
        // Goal: dividing and truncating leaves the right and bottom edges
        // unsampled, which crops a cover by a row or a column - invisible on a
        // photograph and obvious on artwork with a border. Method: ask what
        // source pixels the last target index covers.
        let (start, end) = span_of(7, 8, 100);
        assert_eq!(
            end, 100,
            "the last target pixel reaches the last source one"
        );
        assert!(start < end, "and covers something");

        // And an upscale, where several target pixels share one source pixel.
        for i in 0..10 {
            let (s, e) = span_of(i, 10, 3);
            assert!(s < 3, "index {i} stays inside the picture");
            assert!(e <= 3, "index {i} does not read past it");
        }
    }
}
