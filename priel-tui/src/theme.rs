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

//! Every colour priel paints, in one place.
//!
//! The renderer asks for a **role** - `accent`, `faint`, `verdict_altered` - and
//! never for a colour. That is the whole point of this module: a `Color::Green`
//! written into `ui.rs` is a colour no theme can change, and one of those is
//! enough to make a theme quietly incomplete. `theme::tests` fails the build if
//! one appears.
//!
//! [`Theme`] is a plain struct of roles, so **adding a role is a compile error
//! in every theme** rather than a silent gap in the four that were forgotten.
//!
//! ## What the colours are not allowed to be
//!
//! Several roles here *are* the indicator rather than decoration:
//! `verdict_clean` means nothing altered the samples, `verdict_near` means the
//! level changed, `verdict_altered` means the stream was rebuilt. Two rules
//! follow, and both are tested:
//!
//! - **Hue is never the only carrier.** `✓`, `≈`, `⚠` and the `✓?` partial mark
//!   say the same thing with no colour at all, which is what makes the grades
//!   survive a monochrome terminal and the red/green deficiency they would
//!   otherwise lean on.
//! - **Every role clears a contrast floor against its own background**, so the
//!   near/altered distinction is as readable on cream as it is on charcoal.
//!
//! ## Where the palettes come from
//!
//! Published palettes, not invented ones: they are cheaper to get right, and a
//! terminal user often already knows them. Every deviation from what a palette
//! publishes is deliberate, and each is recorded on the palette that made it
//! with the ratio that forced it. Nearly all of them are the same deviation:
//! an editor palette puts its comment grey below the contrast floor, because
//! there that grey marks text you are meant to skip. In priel `faint` carries
//! the keyboard reference along the bottom row, which is how bindings are
//! discovered, so it is lightened on a dark theme and darkened on a light one
//! until it clears - and where a palette publishes no second grey at all,
//! `muted` is lightened out of the same one rather than invented beside it.

use clap::ValueEnum as _;
use ratatui::style::{Color, Style};

use crate::cli::ThemeName;

/// The colours priel paints, one field per role.
///
/// Add a field rather than reaching for a literal: every palette below then
/// stops compiling until each has answered for the new role, which is the only
/// way a theme set stays complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// The surface everything is drawn on. [`Color::Reset`] hands the question
    /// back to the terminal, which is the only honest answer for a background
    /// priel cannot know.
    pub background: Color,
    /// Primary foreground: a title, a value, the thing being read.
    pub text: Color,
    /// Secondary foreground: prose in an overlay, an ordinary log line.
    pub muted: Color,
    /// Tertiary foreground: the keyboard reference, footers, an inactive tab.
    /// De-emphasised, never illegible - these are still content.
    pub faint: Color,
    /// Structure: borders, section headings, key glyphs, the progress bar.
    pub accent: Color,
    /// The highlighted row of a list or a picker.
    pub selection_fg: Color,
    /// The highlighted row of a list or a picker.
    pub selection_bg: Color,
    /// A control drawn as a glyph on a raised background.
    pub control_fg: Color,
    /// A control drawn as a glyph on a raised background.
    pub control_bg: Color,
    /// A toggle that is on. Off borrows the control colours.
    pub toggle_on_fg: Color,
    /// A toggle that is on. Off borrows the control colours.
    pub toggle_on_bg: Color,
    /// In use, playing, current: the device being played into, the track in the
    /// speakers, a buffer with room to spare.
    pub active: Color,
    /// Under way or worth a glance: resolving, buffering, a filter in force, a
    /// warning in the log.
    pub notice: Color,
    /// Something failed and was recorded.
    pub error: Color,
    /// A kept track.
    pub favorite: Color,
    /// How much of the queue is left.
    pub queue: Color,
    /// Nothing altered the samples.
    pub verdict_clean: Color,
    /// Only the level changed.
    pub verdict_near: Color,
    /// The sample stream was rebuilt: resampled, or truncated.
    pub verdict_altered: Color,
    /// There is not yet anything to grade.
    pub verdict_unknown: Color,
}

/// The palettes on offer, in the order both the flag and the picker list them.
///
/// One list, so a palette the picker shows is always one `--theme` accepts.
pub const OFFERED: &[ThemeName] = &[
    ThemeName::Nord,
    ThemeName::GruvboxDark,
    ThemeName::GruvboxLight,
    ThemeName::OneLight,
    ThemeName::Dracula,
    ThemeName::OneDark,
    ThemeName::TrueBlack,
    ThemeName::Terminal,
];

/// A palette's name, spelled the way `--theme` takes it.
///
/// Read back off the command definition rather than written out again here:
/// a second spelling is a picker that can offer something the flag refuses.
#[must_use]
pub fn label(name: ThemeName) -> String {
    name.to_possible_value()
        .map_or_else(String::new, |v| v.get_name().to_string())
}

/// What the flag's own help says about a palette, so the picker and `--help`
/// cannot describe the same theme two different ways.
#[must_use]
pub fn note(name: ThemeName) -> String {
    name.to_possible_value()
        .and_then(|v| v.get_help().map(ToString::to_string))
        .unwrap_or_default()
}

impl Default for Theme {
    fn default() -> Self {
        Self::of(ThemeName::default())
    }
}

impl Theme {
    /// The palette a name stands for.
    #[must_use]
    pub const fn of(name: ThemeName) -> Self {
        match name {
            ThemeName::Terminal => TERMINAL,
            ThemeName::Nord => NORD,
            ThemeName::GruvboxDark => GRUVBOX_DARK,
            ThemeName::GruvboxLight => GRUVBOX_LIGHT,
            ThemeName::OneLight => ONE_LIGHT,
            ThemeName::Dracula => DRACULA,
            ThemeName::OneDark => ONE_DARK,
            ThemeName::TrueBlack => TRUE_BLACK,
        }
    }

    /// The surface: what an empty cell looks like.
    ///
    /// Painted over the whole frame before anything else, and over each
    /// overlay's own block, so a theme's background is the background rather
    /// than a suggestion the terminal is free to ignore.
    #[must_use]
    pub const fn surface(&self) -> Style {
        Style::new().fg(self.text).bg(self.background)
    }

    /// A foreground role on the surface.
    #[must_use]
    pub const fn on(&self, role: Color) -> Style {
        Style::new().fg(role).bg(self.background)
    }

    /// The highlighted row.
    #[must_use]
    pub const fn selection(&self) -> Style {
        Style::new().fg(self.selection_fg).bg(self.selection_bg)
    }

    /// A clickable glyph on a raised background.
    #[must_use]
    pub const fn control(&self) -> Style {
        Style::new().fg(self.control_fg).bg(self.control_bg)
    }

    /// A toggle, in whichever state it is in.
    #[must_use]
    pub const fn toggle(&self, on: bool) -> Style {
        if on {
            Style::new().fg(self.toggle_on_fg).bg(self.toggle_on_bg)
        } else {
            self.control()
        }
    }
}

/// The terminal's own sixteen colours, by index rather than by value.
///
/// The only palette that can be right on a terminal whose background priel
/// cannot read, and the one to pick when the terminal is already themed:
/// fighting a palette the user chose is its own bug. [`Color::Reset`] for the
/// surface and for `text` means "whatever you set", so this follows a light
/// terminal into light and a dark one into dark.
const TERMINAL: Theme = Theme {
    background: Color::Reset,
    text: Color::Reset,
    muted: Color::Gray,
    faint: Color::DarkGray,
    accent: Color::Cyan,
    selection_fg: Color::Black,
    selection_bg: Color::Cyan,
    control_fg: Color::Cyan,
    control_bg: Color::DarkGray,
    toggle_on_fg: Color::Black,
    toggle_on_bg: Color::Green,
    active: Color::Green,
    notice: Color::Yellow,
    error: Color::Red,
    favorite: Color::Magenta,
    queue: Color::Blue,
    verdict_clean: Color::Green,
    verdict_near: Color::Yellow,
    verdict_altered: Color::Red,
    verdict_unknown: Color::DarkGray,
};

/// Nord, by Arctic Ice Studio. Dark, and priel's default.
///
/// `faint` is nord3 lightened: nord3 itself sits at 2.4 against nord0, and the
/// bottom row is not decoration.
const NORD: Theme = Theme {
    background: Color::Rgb(0x2e, 0x34, 0x40),
    text: Color::Rgb(0xec, 0xef, 0xf4),
    muted: Color::Rgb(0xd8, 0xde, 0xe9),
    faint: Color::Rgb(0x7b, 0x88, 0xa1),
    accent: Color::Rgb(0x88, 0xc0, 0xd0),
    selection_fg: Color::Rgb(0xec, 0xef, 0xf4),
    selection_bg: Color::Rgb(0x5e, 0x81, 0xac),
    control_fg: Color::Rgb(0x88, 0xc0, 0xd0),
    control_bg: Color::Rgb(0x43, 0x4c, 0x5e),
    toggle_on_fg: Color::Rgb(0x2e, 0x34, 0x40),
    toggle_on_bg: Color::Rgb(0xa3, 0xbe, 0x8c),
    active: Color::Rgb(0xa3, 0xbe, 0x8c),
    notice: Color::Rgb(0xeb, 0xcb, 0x8b),
    error: Color::Rgb(0xbf, 0x61, 0x6a),
    favorite: Color::Rgb(0xb4, 0x8e, 0xad),
    queue: Color::Rgb(0x81, 0xa1, 0xc1),
    verdict_clean: Color::Rgb(0xa3, 0xbe, 0x8c),
    verdict_near: Color::Rgb(0xeb, 0xcb, 0x8b),
    verdict_altered: Color::Rgb(0xbf, 0x61, 0x6a),
    verdict_unknown: Color::Rgb(0x7b, 0x88, 0xa1),
};

/// Gruvbox dark, by Pavel Pertsev. Warm, and half of a matched pair - switching
/// to the light one changes the background without changing which hue means
/// what.
const GRUVBOX_DARK: Theme = Theme {
    background: Color::Rgb(0x28, 0x28, 0x28),
    text: Color::Rgb(0xeb, 0xdb, 0xb2),
    muted: Color::Rgb(0xd5, 0xc4, 0xa1),
    faint: Color::Rgb(0x92, 0x83, 0x74),
    accent: Color::Rgb(0x83, 0xa5, 0x98),
    selection_fg: Color::Rgb(0x28, 0x28, 0x28),
    selection_bg: Color::Rgb(0x83, 0xa5, 0x98),
    control_fg: Color::Rgb(0x83, 0xa5, 0x98),
    control_bg: Color::Rgb(0x3c, 0x38, 0x36),
    toggle_on_fg: Color::Rgb(0x28, 0x28, 0x28),
    toggle_on_bg: Color::Rgb(0xb8, 0xbb, 0x26),
    active: Color::Rgb(0xb8, 0xbb, 0x26),
    notice: Color::Rgb(0xfa, 0xbd, 0x2f),
    error: Color::Rgb(0xfb, 0x49, 0x34),
    favorite: Color::Rgb(0xd3, 0x86, 0x9b),
    queue: Color::Rgb(0x8e, 0xc0, 0x7c),
    verdict_clean: Color::Rgb(0xb8, 0xbb, 0x26),
    verdict_near: Color::Rgb(0xfa, 0xbd, 0x2f),
    verdict_altered: Color::Rgb(0xfb, 0x49, 0x34),
    verdict_unknown: Color::Rgb(0x92, 0x83, 0x74),
};

/// Gruvbox light, the same palette's own light half. Its author redrew the
/// accents for a cream background rather than reusing the dark ones, which is
/// exactly why the near/altered distinction survives the switch.
const GRUVBOX_LIGHT: Theme = Theme {
    background: Color::Rgb(0xfb, 0xf1, 0xc7),
    text: Color::Rgb(0x3c, 0x38, 0x36),
    muted: Color::Rgb(0x50, 0x49, 0x45),
    faint: Color::Rgb(0x7c, 0x6f, 0x64),
    accent: Color::Rgb(0x07, 0x66, 0x78),
    selection_fg: Color::Rgb(0xfb, 0xf1, 0xc7),
    selection_bg: Color::Rgb(0x07, 0x66, 0x78),
    control_fg: Color::Rgb(0x07, 0x66, 0x78),
    control_bg: Color::Rgb(0xeb, 0xdb, 0xb2),
    toggle_on_fg: Color::Rgb(0xfb, 0xf1, 0xc7),
    toggle_on_bg: Color::Rgb(0x79, 0x74, 0x0e),
    active: Color::Rgb(0x79, 0x74, 0x0e),
    notice: Color::Rgb(0xb5, 0x76, 0x14),
    error: Color::Rgb(0x9d, 0x00, 0x06),
    favorite: Color::Rgb(0x8f, 0x3f, 0x71),
    queue: Color::Rgb(0x42, 0x7b, 0x58),
    verdict_clean: Color::Rgb(0x79, 0x74, 0x0e),
    verdict_near: Color::Rgb(0xb5, 0x76, 0x14),
    verdict_altered: Color::Rgb(0x9d, 0x00, 0x06),
    verdict_unknown: Color::Rgb(0x7c, 0x6f, 0x64),
};

/// One Light, from the Atom editor. A cool light palette next to Gruvbox's warm
/// one, so a light terminal has a choice rather than a single answer.
///
/// `faint` is mono-3 darkened, for the reason the module docs give.
const ONE_LIGHT: Theme = Theme {
    background: Color::Rgb(0xfa, 0xfa, 0xfa),
    text: Color::Rgb(0x38, 0x3a, 0x42),
    muted: Color::Rgb(0x69, 0x6c, 0x77),
    faint: Color::Rgb(0x8a, 0x8c, 0x94),
    accent: Color::Rgb(0x40, 0x78, 0xf2),
    selection_fg: Color::Rgb(0xfa, 0xfa, 0xfa),
    selection_bg: Color::Rgb(0x40, 0x78, 0xf2),
    control_fg: Color::Rgb(0x40, 0x78, 0xf2),
    control_bg: Color::Rgb(0xe5, 0xe5, 0xe6),
    toggle_on_fg: Color::Rgb(0xfa, 0xfa, 0xfa),
    toggle_on_bg: Color::Rgb(0x50, 0xa1, 0x4f),
    active: Color::Rgb(0x50, 0xa1, 0x4f),
    notice: Color::Rgb(0x98, 0x68, 0x01),
    error: Color::Rgb(0xca, 0x12, 0x43),
    favorite: Color::Rgb(0xa6, 0x26, 0xa4),
    queue: Color::Rgb(0x01, 0x84, 0xbc),
    verdict_clean: Color::Rgb(0x50, 0xa1, 0x4f),
    verdict_near: Color::Rgb(0x98, 0x68, 0x01),
    verdict_altered: Color::Rgb(0xca, 0x12, 0x43),
    verdict_unknown: Color::Rgb(0x8a, 0x8c, 0x94),
};

/// Dracula, by Zeno Rocha. Dark, and the most saturated palette on offer.
///
/// Published as one background and eleven colours, with **one foreground and
/// one comment grey and nothing in between** - so the two steps priel wants
/// below `text` are the comment lightened along its own hue rather than a
/// second grey invented from nothing. Comment `#6272a4` measures 3.03:1, which
/// clears the mark floor by three hundredths and is the thinnest `faint` in the
/// set; `muted` is prose and needs 4.5. So `muted` is that comment at 4.97:1
/// and `faint` at 3.56:1, which is where nord's sits.
///
/// Dracula gives Selection and Current Line the same value, so the highlighted
/// row takes the accent as gruvbox-dark's does and `#44475a` becomes the raised
/// chip a control is drawn on. `notice` is Orange rather than Yellow: Dracula's
/// Yellow is a lime at hue 65 and would be read next to its Green at hue 135,
/// which is the one distinction the fidelity grades cannot afford to lose.
const DRACULA: Theme = Theme {
    background: Color::Rgb(0x28, 0x2a, 0x36),
    text: Color::Rgb(0xf8, 0xf8, 0xf2),
    muted: Color::Rgb(0x8c, 0x98, 0xbc),
    faint: Color::Rgb(0x6f, 0x7e, 0xab),
    accent: Color::Rgb(0xbd, 0x93, 0xf9),
    selection_fg: Color::Rgb(0x28, 0x2a, 0x36),
    selection_bg: Color::Rgb(0xbd, 0x93, 0xf9),
    control_fg: Color::Rgb(0xbd, 0x93, 0xf9),
    control_bg: Color::Rgb(0x44, 0x47, 0x5a),
    toggle_on_fg: Color::Rgb(0x28, 0x2a, 0x36),
    toggle_on_bg: Color::Rgb(0x50, 0xfa, 0x7b),
    active: Color::Rgb(0x50, 0xfa, 0x7b),
    notice: Color::Rgb(0xff, 0xb8, 0x6c),
    error: Color::Rgb(0xff, 0x55, 0x55),
    favorite: Color::Rgb(0xff, 0x79, 0xc6),
    queue: Color::Rgb(0x8b, 0xe9, 0xfd),
    verdict_clean: Color::Rgb(0x50, 0xfa, 0x7b),
    verdict_near: Color::Rgb(0xff, 0xb8, 0x6c),
    verdict_altered: Color::Rgb(0xff, 0x55, 0x55),
    verdict_unknown: Color::Rgb(0x6f, 0x7e, 0xab),
};

/// One Dark, from the Atom editor: the dark half of the pair `one-light` is the
/// light half of, and the same roles read off the same variables.
///
/// Two deviations, both the lightening the module docs describe. mono-3, the
/// comment grey, measures 2.32:1 and carries `faint` at 3.57:1; mono-2 measures
/// 3.98:1, which is fine for a comment and short of the 4.5 `muted` owes a body
/// of prose, and carries it at 4.72:1.
///
/// Where One Dark publishes two of a hue, the light sibling takes the darker
/// and this one takes the lighter, which is the same judgement made from
/// opposite ends: `error` is hue-5 rather than hue-5-2 (2.95:1 here, under the
/// floor), and `notice` is hue-6-2 rather than the hue-6 `one-light` uses.
/// `control_bg` is the theme's own raised surface - `lighten(@syntax-bg, 10%)`,
/// what it paints a selected line with.
const ONE_DARK: Theme = Theme {
    background: Color::Rgb(0x28, 0x2c, 0x34),
    text: Color::Rgb(0xab, 0xb2, 0xbf),
    muted: Color::Rgb(0x90, 0x96, 0xa3),
    faint: Color::Rgb(0x79, 0x81, 0x91),
    accent: Color::Rgb(0x61, 0xaf, 0xef),
    selection_fg: Color::Rgb(0x28, 0x2c, 0x34),
    selection_bg: Color::Rgb(0x61, 0xaf, 0xef),
    control_fg: Color::Rgb(0x61, 0xaf, 0xef),
    control_bg: Color::Rgb(0x3e, 0x44, 0x51),
    toggle_on_fg: Color::Rgb(0x28, 0x2c, 0x34),
    toggle_on_bg: Color::Rgb(0x98, 0xc3, 0x79),
    active: Color::Rgb(0x98, 0xc3, 0x79),
    notice: Color::Rgb(0xe5, 0xc0, 0x7b),
    error: Color::Rgb(0xe0, 0x6c, 0x75),
    favorite: Color::Rgb(0xc6, 0x78, 0xdd),
    queue: Color::Rgb(0x56, 0xb6, 0xc2),
    verdict_clean: Color::Rgb(0x98, 0xc3, 0x79),
    verdict_near: Color::Rgb(0xe5, 0xc0, 0x7b),
    verdict_altered: Color::Rgb(0xe0, 0x6c, 0x75),
    verdict_unknown: Color::Rgb(0x79, 0x81, 0x91),
};

/// True black: Modus Vivendi's colours on the `#000000` this theme is named
/// for. The one to pick on an OLED panel, where a black pixel is an unlit pixel
/// and the surface costs no light at all.
///
/// Named for the surface rather than the panel. `oled` would name the hardware,
/// and the reason to want an unlit background outlives the display technology -
/// a dark room, a projector, a photophobic reader; `black` on its own would sit
/// in the list next to `terminal` and read as one of the sixteen ANSI names.
/// `true-black` says the one thing that distinguishes it: not dark, `#000000`.
///
/// The palette is the only one here designed to a contrast standard rather than
/// to a look. Protesilaos Stavrou's Modus themes ship with GNU Emacs and hold
/// every foreground to 7:1 (WCAG AAA) against a `bg-main` that is already pure
/// black, which is what makes them the honest source for this: nothing had to
/// be moved to survive the drop to `#000000`, because that is where they were
/// drawn.
///
/// **One deviation, and it is the only one in the set that goes down.**
/// `fg-main` is `#ffffff`, which scores 21:1 - the highest ratio there is, and
/// a known readability problem: at that luminance small glyphs bloom into the
/// black around them, which is tiring over an album rather than a line. `text`
/// is white at 90% instead, 16.83:1, still more than twice the AAA floor the
/// palette was built to. No measurement can make that call - 21:1 is a perfect
/// score - which is why the test guarding it asserts what the colour is *not*.
///
/// Nothing else needed a lift, which is what a palette built to 7:1 buys:
/// `muted` is `fg-dim` unmodified at 7.28:1, and `faint` is `border` at 3.55:1,
/// which is where nord's and gruvbox's sit. Modus publishes exactly one dim
/// foreground, and the border is the step below it. The row backgrounds are its
/// own too: `bg-region` is what it selects with, `bg-inactive` what it raises a
/// surface with.
const TRUE_BLACK: Theme = Theme {
    background: Color::Rgb(0x00, 0x00, 0x00),
    text: Color::Rgb(0xe6, 0xe6, 0xe6),
    muted: Color::Rgb(0x98, 0x98, 0x98),
    faint: Color::Rgb(0x64, 0x64, 0x64),
    accent: Color::Rgb(0x00, 0xbc, 0xff),
    selection_fg: Color::Rgb(0xe6, 0xe6, 0xe6),
    selection_bg: Color::Rgb(0x5a, 0x5a, 0x5a),
    control_fg: Color::Rgb(0x00, 0xbc, 0xff),
    control_bg: Color::Rgb(0x30, 0x30, 0x30),
    toggle_on_fg: Color::Rgb(0x00, 0x00, 0x00),
    toggle_on_bg: Color::Rgb(0x44, 0xbc, 0x44),
    active: Color::Rgb(0x44, 0xbc, 0x44),
    notice: Color::Rgb(0xfe, 0xc4, 0x3f),
    error: Color::Rgb(0xff, 0x5f, 0x59),
    favorite: Color::Rgb(0xf7, 0x8f, 0xe7),
    queue: Color::Rgb(0x00, 0xd3, 0xd0),
    verdict_clean: Color::Rgb(0x44, 0xbc, 0x44),
    verdict_near: Color::Rgb(0xfe, 0xc4, 0x3f),
    verdict_altered: Color::Rgb(0xff, 0x5f, 0x59),
    verdict_unknown: Color::Rgb(0x64, 0x64, 0x64),
};

#[cfg(test)]
mod tests {
    use super::{Theme, ThemeName};
    use clap::ValueEnum;
    use ratatui::style::Color;

    /// The contrast a body of prose has to clear: WCAG AA for normal text.
    const PROSE_FLOOR: f64 = 4.5;

    /// The contrast an indicator, an accent or a de-emphasised line has to
    /// clear: WCAG's floor for large text and for graphical objects. These are
    /// short, bold, and never the only carrier of their meaning.
    const MARK_FLOOR: f64 = 3.0;

    /// One channel of sRGB, linearised, per WCAG 2.
    fn linear(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Relative luminance, or `None` for a colour whose value priel does not
    /// know - every ANSI index, which is the terminal's to decide.
    fn luminance(c: Color) -> Option<f64> {
        let Color::Rgb(r, g, b) = c else {
            return None;
        };
        Some(0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b))
    }

    /// The WCAG contrast ratio between two known colours.
    fn contrast(a: Color, b: Color) -> Option<f64> {
        let (a, b) = (luminance(a)?, luminance(b)?);
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        Some((hi + 0.05) / (lo + 0.05))
    }

    /// Every palette, with the name it is chosen by.
    fn all() -> Vec<(ThemeName, Theme)> {
        super::OFFERED.iter().map(|n| (*n, Theme::of(*n))).collect()
    }

    /// Goal: a role that cannot be read is a role that carries nothing, and the
    /// fidelity grades are exactly the roles that must survive a light
    /// background as well as a dark one. Method: measure every role against the
    /// background it is painted on and hold it to the WCAG floor for what it is
    /// - prose, or a short bold mark.
    #[test]
    fn every_role_is_legible_on_its_own_background() {
        for (name, t) in all() {
            let bg = t.background;
            let prose = [("text", t.text), ("muted", t.muted)];
            let marks = [
                ("faint", t.faint),
                ("accent", t.accent),
                ("active", t.active),
                ("notice", t.notice),
                ("error", t.error),
                ("favorite", t.favorite),
                ("queue", t.queue),
                ("verdict_clean", t.verdict_clean),
                ("verdict_near", t.verdict_near),
                ("verdict_altered", t.verdict_altered),
                ("verdict_unknown", t.verdict_unknown),
            ];
            for (floor, roles) in [(PROSE_FLOOR, &prose[..]), (MARK_FLOOR, &marks[..])] {
                for (role, colour) in roles {
                    // `None` is the terminal palette, whose values are the
                    // user's; there is nothing here priel could measure.
                    let Some(ratio) = contrast(*colour, bg) else {
                        continue;
                    };
                    assert!(
                        ratio >= floor,
                        "{name:?}: {role} is {ratio:.2}:1 on the background, under {floor}"
                    );
                }
            }
        }
    }

    /// Goal: the three pairs that carry their own background - the selected
    /// row, a control, a toggle that is on - are read against that background
    /// and not against the surface, so measuring them against the surface would
    /// prove nothing.
    #[test]
    fn every_pair_is_legible_against_its_own_backing() {
        for (name, t) in all() {
            let pairs = [
                ("selection", t.selection_fg, t.selection_bg),
                ("control", t.control_fg, t.control_bg),
                ("toggle_on", t.toggle_on_fg, t.toggle_on_bg),
            ];
            for (role, fg, bg) in pairs {
                let Some(ratio) = contrast(fg, bg) else {
                    continue;
                };
                assert!(
                    ratio >= MARK_FLOOR,
                    "{name:?}: {role} is {ratio:.2}:1 against its own backing"
                );
            }
        }
    }

    /// Goal: the three fidelity grades have to be told apart, and a palette
    /// that gave two of them the same colour would read as one finding. The
    /// glyphs carry the meaning on their own, so this is the second line of
    /// defence rather than the first - but a duplicate here is always a
    /// mistake.
    #[test]
    fn the_three_grades_never_share_a_colour() {
        for (name, t) in all() {
            assert_ne!(t.verdict_clean, t.verdict_near, "{name:?}");
            assert_ne!(t.verdict_near, t.verdict_altered, "{name:?}");
            assert_ne!(t.verdict_clean, t.verdict_altered, "{name:?}");
        }
    }

    /// Goal: the terminal palette exists to defer to a palette the user already
    /// chose, and a single `Rgb` in it would override exactly the colour it was
    /// meant to respect. Method: every role must be an ANSI name or `Reset`.
    #[test]
    fn the_terminal_palette_names_no_value_of_its_own() {
        let t = Theme::of(ThemeName::Terminal);
        let roles = [
            t.background,
            t.text,
            t.muted,
            t.faint,
            t.accent,
            t.selection_fg,
            t.selection_bg,
            t.control_fg,
            t.control_bg,
            t.toggle_on_fg,
            t.toggle_on_bg,
            t.active,
            t.notice,
            t.error,
            t.favorite,
            t.queue,
            t.verdict_clean,
            t.verdict_near,
            t.verdict_altered,
            t.verdict_unknown,
        ];
        for role in roles {
            assert!(
                !matches!(role, Color::Rgb(..) | Color::Indexed(_)),
                "the terminal palette pins {role:?} instead of deferring"
            );
        }
    }

    /// Goal: the picker walks `OFFERED` and the flag walks its own variants,
    /// and a palette missing from either is one the two disagree about. Method:
    /// compare the sets rather than trusting that both were edited.
    #[test]
    fn what_the_picker_offers_is_what_the_flag_accepts() {
        let flagged = ThemeName::value_variants();
        assert_eq!(super::OFFERED.len(), flagged.len());
        for name in flagged {
            assert!(super::OFFERED.contains(name), "the picker hides {name:?}");
            assert!(!super::label(*name).is_empty(), "{name:?} has no name");
            assert!(!super::note(*name).is_empty(), "{name:?} says nothing");
        }
    }

    /// Goal: at least one theme for a dark terminal and one for a light one,
    /// and the default is a dark one. Method: read the backgrounds rather than
    /// trusting the names.
    #[test]
    fn the_set_covers_light_and_dark_and_defaults_to_dark() {
        let bright = |t: &Theme| luminance(t.background).is_some_and(|l| l > 0.5);
        let dark = |t: &Theme| luminance(t.background).is_some_and(|l| l < 0.1);
        assert!(all().iter().any(|(_, t)| bright(t)), "no light theme");
        assert!(all().iter().any(|(_, t)| dark(t)), "no dark theme");
        assert!(dark(&Theme::default()), "the default is not a dark theme");
    }

    /// Goal: an OLED panel spends no light on a black pixel, so one palette
    /// takes the surface all the way down - and that surface is the one pure
    /// white must not be painted on. White on black blooms into the space
    /// around each glyph, which is tiring over an album rather than a line, and
    /// no contrast test can catch it: `#ffffff` on `#000000` scores 21:1, the
    /// highest ratio there is. So this is the judgement the measurements cannot
    /// make, written down as the one thing the text is not.
    ///
    /// Method: find the palette that takes the background to `#000000`, and
    /// hold it to both halves - the text is not pure white, and it still clears
    /// the 7:1 its own source palette was built to.
    #[test]
    fn the_true_black_palette_does_not_paint_pure_white_on_pure_black() {
        let (name, t) = all()
            .into_iter()
            .find(|(_, t)| t.background == Color::Rgb(0, 0, 0))
            .expect("one palette should take the surface to #000000");
        assert_ne!(
            t.text,
            Color::Rgb(0xff, 0xff, 0xff),
            "{name:?}: pure white on pure black is the readability problem \
             true black exists to avoid, not a score to max out"
        );
        let ratio = contrast(t.text, t.background).expect("both are known values");
        assert!(
            ratio >= 7.0,
            "{name:?}: text is {ratio:.2}:1, under WCAG AAA"
        );
    }

    /// Goal: the renderer must not name a colour of its own. One
    /// `Color::Green` written into a rendering path is a colour no theme can
    /// reach, and this is the kind of thing that rots one call site at a time -
    /// so the guard is a test rather than a habit.
    ///
    /// Method: read the two files that draw, up to their own test modules,
    /// and refuse any `Color::` literal in them.
    #[test]
    fn nothing_outside_this_module_names_a_colour() {
        let sources = [
            ("ui.rs", include_str!("ui.rs")),
            ("app.rs", include_str!("app.rs")),
            ("main.rs", include_str!("main.rs")),
        ];
        for (file, src) in sources {
            // The tests below a `#[cfg(test)]` may name colours: asserting on
            // what was painted means naming what was expected.
            let code = src.split("#[cfg(test)]").next().unwrap_or(src);
            let offenders = code.matches("Color::").count();
            assert_eq!(
                offenders, 0,
                "{file} names {offenders} colour(s) of its own; \
                 add a role to `theme::Theme` and use that instead"
            );
        }
    }
}
