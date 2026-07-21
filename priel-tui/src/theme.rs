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
//! Not every palette needs it. Both gruvbox halves and `catppuccin` draw their
//! comment grey above the floor to begin with, and ship here unmodified.
//!
//! ## The stripe is a second surface, and it is measured like one
//!
//! `stripe_bg` backs every other row of a list, so it is the only role here
//! that is judged twice: it has to differ from the surface enough to be seen
//! and little enough not to be read as a state, and *everything painted on it*
//! has to clear the same floor it clears on the surface. Three foregrounds land
//! on it - `text` on an ordinary row, `active` on the row in the speakers,
//! `faint` on the tabs that are not the one you are on - and each is measured
//! against the stripe rather than against the background, because that is the
//! backing it is actually read on.
//!
//! Which direction a stripe goes is a palette's own answer, not a house rule.
//! Most step up from the surface; `gruvbox-light`, `catppuccin` and
//! `tokyo-night` step down, and each says why on its own constant - the step
//! above may already be spoken for by the control chip, or the surface may be
//! light enough that darkening it takes a foreground under its floor.
//! `terminal` takes no stripe at all, for the reason recorded there.

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
    /// The highlighted row of the list the keyboard is **not** driving.
    ///
    /// Two lists are focusable - the browse list and the queue in its own
    /// column - so two cursors are on screen at once and each has to
    /// say which it is. This is the quiet one: the selection backing stepped
    /// most of the way back toward the surface, so focusing a list can only
    /// make its cursor louder and never dimmer.
    ///
    /// It carries a foreground of its own rather than borrowing
    /// [`Theme::selection_fg`], and that is measured rather than tidy: on the
    /// eight palettes whose selected row is the *background* colour on a bright
    /// backing, that same foreground lands at about 1.6:1 on a backing this
    /// close to the surface. What reads there is the ordinary [`Theme::text`],
    /// which is also the honest answer - the row the listener is not driving is
    /// not disabled, so its words are the words every other row wears.
    pub selection_idle_fg: Color,
    /// The highlighted row of the list the keyboard is not driving.
    pub selection_idle_bg: Color,
    /// The backing of every other row in a list, and of the tabs that are not
    /// the one you are on. A whisper away from [`Theme::background`]: enough to
    /// carry the eye along a row two hundred cells wide, never enough to be
    /// read as a state. Equal to the background means no stripe at all, which
    /// is the only honest answer for a surface priel cannot see.
    pub stripe_bg: Color,
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
    ThemeName::Catppuccin,
    ThemeName::TokyoNight,
    ThemeName::TokyoNightDay,
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
            ThemeName::Catppuccin => CATPPUCCIN,
            ThemeName::TokyoNight => TOKYO_NIGHT,
            ThemeName::TokyoNightDay => TOKYO_NIGHT_DAY,
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

    /// The cursor of a list, loud or quiet by whether it holds the keyboard.
    ///
    /// A full pair either way rather than a backing the caller adds a
    /// foreground to, because a cursor owns the row it is on: the row in the
    /// speakers already gives up `active` under the focused cursor, and a
    /// second rule for the idle one would be two answers to one question.
    #[must_use]
    pub const fn cursor(&self, focused: bool) -> Style {
        if focused {
            self.selection()
        } else {
            Style::new()
                .fg(self.selection_idle_fg)
                .bg(self.selection_idle_bg)
        }
    }

    /// The backing a row sits on: the surface, or the stripe on every other
    /// one. Foregroundless on purpose - a stripe changes what a row is drawn
    /// on and never what it says, so the caller adds its own `fg` and the row
    /// keeps whatever the surface gave it when it does not.
    #[must_use]
    pub const fn stripe(&self, striped: bool) -> Style {
        if striped {
            Style::new().bg(self.stripe_bg)
        } else {
            Style::new()
        }
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
///
/// **The one palette with no stripe, and that is the decision rather than an
/// omission.** Every other palette knows its own surface to the byte and can
/// put a stripe a measured whisker away from it. This one does not know the
/// surface at all - that is the whole point of it - so any stripe would be a
/// guess at what the user's own background is, and a guess that lands wrong is
/// a band of the wrong colour down every other row. There is no ANSI index for
/// "the background, only slightly", and inventing one out of `black` or
/// `dark gray` would pin exactly the colour this palette exists to defer to.
/// So `stripe_bg` is `Reset`, the same as the surface, and the rows alternate
/// with nothing. `--theme`'s own help says so, which is what puts it in the
/// picker beside the palette rather than leaving it to be noticed as a bug.
const TERMINAL: Theme = Theme {
    background: Color::Reset,
    text: Color::Reset,
    muted: Color::Gray,
    faint: Color::DarkGray,
    accent: Color::Cyan,
    selection_fg: Color::Black,
    selection_bg: Color::Cyan,
    // A neutral rather than a hue, which is this palette's own way of saying
    // quiet: the focused cursor is `Cyan` and both are pinned, because `text`
    // here is `Reset` and a foreground priel cannot see cannot be measured
    // against any backing at all.
    selection_idle_fg: Color::Black,
    selection_idle_bg: Color::Gray,
    stripe_bg: Color::Reset,
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
///
/// The stripe is **half a step**, and the half is the considered part. Nord
/// publishes nord1 as the elevated surface, but nord1 is 1.24:1 above nord0 -
/// the loudest step any palette here offers - and at that distance `faint`
/// falls to 2.82:1, under the floor a mark owes, on the very tabs the stripe
/// would be backing. Halfway from nord0 to nord1 is 1.11:1, which is a stripe
/// an eye follows and a `faint` at 3.15:1.
const NORD: Theme = Theme {
    background: Color::Rgb(0x2e, 0x34, 0x40),
    text: Color::Rgb(0xec, 0xef, 0xf4),
    muted: Color::Rgb(0xd8, 0xde, 0xe9),
    faint: Color::Rgb(0x7b, 0x88, 0xa1),
    accent: Color::Rgb(0x88, 0xc0, 0xd0),
    selection_fg: Color::Rgb(0xec, 0xef, 0xf4),
    selection_bg: Color::Rgb(0x5e, 0x81, 0xac),
    selection_idle_fg: Color::Rgb(0xec, 0xef, 0xf4),
    selection_idle_bg: Color::Rgb(0x41, 0x53, 0x6b),
    stripe_bg: Color::Rgb(0x34, 0x3b, 0x49),
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
///
/// The stripe is `bg0_s`, and it needed no adjusting at all. Gruvbox is the one
/// palette here that publishes a background *between* its surface and its first
/// raised step - `bg0_s`, the soft background, is exactly the half-step nord
/// had to be blended into existence - and it lands at 1.12:1 with `faint` still
/// at 3.58:1. `bg1` stays where it was, as the chip a control is drawn on.
const GRUVBOX_DARK: Theme = Theme {
    background: Color::Rgb(0x28, 0x28, 0x28),
    text: Color::Rgb(0xeb, 0xdb, 0xb2),
    muted: Color::Rgb(0xd5, 0xc4, 0xa1),
    faint: Color::Rgb(0x92, 0x83, 0x74),
    accent: Color::Rgb(0x83, 0xa5, 0x98),
    selection_fg: Color::Rgb(0x28, 0x28, 0x28),
    selection_bg: Color::Rgb(0x83, 0xa5, 0x98),
    selection_idle_fg: Color::Rgb(0xeb, 0xdb, 0xb2),
    selection_idle_bg: Color::Rgb(0x43, 0x4e, 0x4a),
    stripe_bg: Color::Rgb(0x32, 0x30, 0x2f),
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
///
/// The stripe is this half's own `bg0_s`, the same published soft background
/// the dark half takes, at 1.11:1. It is the only light palette here whose
/// stripe goes *down*: darkening a light surface costs every foreground on it
/// some contrast, and this is the one light palette with enough in hand to pay.
/// Its `active` olive drops from 4.29:1 to 3.87:1 and is still clear of the
/// floor, where `one-light`'s green would go under.
const GRUVBOX_LIGHT: Theme = Theme {
    background: Color::Rgb(0xfb, 0xf1, 0xc7),
    text: Color::Rgb(0x3c, 0x38, 0x36),
    muted: Color::Rgb(0x50, 0x49, 0x45),
    faint: Color::Rgb(0x7c, 0x6f, 0x64),
    accent: Color::Rgb(0x07, 0x66, 0x78),
    selection_fg: Color::Rgb(0xfb, 0xf1, 0xc7),
    selection_bg: Color::Rgb(0x07, 0x66, 0x78),
    selection_idle_fg: Color::Rgb(0x3c, 0x38, 0x36),
    selection_idle_bg: Color::Rgb(0xb2, 0xc7, 0xaf),
    stripe_bg: Color::Rgb(0xf2, 0xe5, 0xbc),
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
///
/// **The one stripe in the set that goes up rather than down, and the palette's
/// own green is why.** One Light's `active` measures 3.07:1 on `#fafafa` -
/// seven hundredths above the mark floor - so a stripe darkened out of the
/// surface, the obvious move on a light theme, takes the row in the speakers
/// under the floor before it is dark enough to see: `darken(bg, 4%)` reads
/// 2.86:1. The surface is off-white rather than white, which leaves a step in
/// the other direction, and a stripe of paper white *lifts* every foreground
/// instead of charging it - `active` to 3.21:1, `faint` to 3.35:1. It is the
/// quietest stripe here at 1.04:1, which is the price of the direction and is
/// still the alternation a printed table uses.
const ONE_LIGHT: Theme = Theme {
    background: Color::Rgb(0xfa, 0xfa, 0xfa),
    text: Color::Rgb(0x38, 0x3a, 0x42),
    muted: Color::Rgb(0x69, 0x6c, 0x77),
    faint: Color::Rgb(0x8a, 0x8c, 0x94),
    accent: Color::Rgb(0x40, 0x78, 0xf2),
    selection_fg: Color::Rgb(0xfa, 0xfa, 0xfa),
    selection_bg: Color::Rgb(0x40, 0x78, 0xf2),
    selection_idle_fg: Color::Rgb(0x38, 0x3a, 0x42),
    selection_idle_bg: Color::Rgb(0xb9, 0xcc, 0xf7),
    stripe_bg: Color::Rgb(0xff, 0xff, 0xff),
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
///
/// The stripe is a quarter of the way from Background to Current Line, at
/// 1.11:1. Dracula publishes one background and one line highlight and nothing
/// between them - the same gap that made `muted` and `faint` a derivation here
/// rather than a lookup - and Current Line itself is already the chip a control
/// is drawn on. Taking a quarter of that one step is the smallest claim this
/// palette allows: the whole step is 1.56:1, half again past the point an
/// alternating background stops being a stripe and starts being a state.
const DRACULA: Theme = Theme {
    background: Color::Rgb(0x28, 0x2a, 0x36),
    text: Color::Rgb(0xf8, 0xf8, 0xf2),
    muted: Color::Rgb(0x8c, 0x98, 0xbc),
    faint: Color::Rgb(0x6f, 0x7e, 0xab),
    accent: Color::Rgb(0xbd, 0x93, 0xf9),
    selection_fg: Color::Rgb(0x28, 0x2a, 0x36),
    selection_bg: Color::Rgb(0xbd, 0x93, 0xf9),
    selection_idle_fg: Color::Rgb(0xf8, 0xf8, 0xf2),
    selection_idle_bg: Color::Rgb(0x55, 0x4a, 0x70),
    stripe_bg: Color::Rgb(0x2f, 0x31, 0x3f),
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
///
/// The stripe is the same construction one rung lower. One Dark builds its
/// raised surface by lightening the background, so the stripe lightens it by a
/// quarter as much and lands at 1.09:1, with `faint` at 3.28:1 and `text` at
/// 6.02:1. The light sibling could not take the same move in the same
/// direction, which is the one place the pair parts company.
const ONE_DARK: Theme = Theme {
    background: Color::Rgb(0x28, 0x2c, 0x34),
    text: Color::Rgb(0xab, 0xb2, 0xbf),
    muted: Color::Rgb(0x90, 0x96, 0xa3),
    faint: Color::Rgb(0x79, 0x81, 0x91),
    accent: Color::Rgb(0x61, 0xaf, 0xef),
    selection_fg: Color::Rgb(0x28, 0x2c, 0x34),
    selection_bg: Color::Rgb(0x61, 0xaf, 0xef),
    selection_idle_fg: Color::Rgb(0xab, 0xb2, 0xbf),
    selection_idle_bg: Color::Rgb(0x36, 0x4d, 0x63),
    stripe_bg: Color::Rgb(0x2e, 0x32, 0x3b),
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
///
/// **The stripe costs light, which is the one thing this palette is spending
/// carefully.** Half its rows stay at `#000000` and unlit; the other half sit
/// at `#101010`, which is about as little light as a stripe can be made of and
/// still be seen - 1.10:1. Modus publishes `bg-dim` at `#1e1e1e` and that would
/// be the obvious lookup, but it is 1.26:1 and takes `border`, this palette's
/// `faint`, to 2.82:1 on the tabs it backs. Half of `bg-dim` keeps `faint` at
/// 3.22:1 and lights half as many pixels.
const TRUE_BLACK: Theme = Theme {
    background: Color::Rgb(0x00, 0x00, 0x00),
    text: Color::Rgb(0xe6, 0xe6, 0xe6),
    muted: Color::Rgb(0x98, 0x98, 0x98),
    faint: Color::Rgb(0x64, 0x64, 0x64),
    accent: Color::Rgb(0x00, 0xbc, 0xff),
    selection_fg: Color::Rgb(0xe6, 0xe6, 0xe6),
    selection_bg: Color::Rgb(0x5a, 0x5a, 0x5a),
    selection_idle_fg: Color::Rgb(0xe6, 0xe6, 0xe6),
    selection_idle_bg: Color::Rgb(0x2d, 0x2d, 0x2d),
    stripe_bg: Color::Rgb(0x10, 0x10, 0x10),
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

/// Catppuccin Mocha, the darkest of the four flavours the Catppuccin project
/// publishes and the one it leads with. Pastel where dracula is neon, on much
/// the same charcoal.
///
/// Named for the project rather than the flavour, because the four flavours are
/// one palette at four depths: Latte, Frappé, Macchiato and Mocha share every
/// hue and differ only in how far down the surface sits. Four rows that a reader
/// would have to try one at a time to tell apart would pad the picker rather
/// than widen it, so Mocha stands for the family: the flavour the project leads
/// with, and the one that answers a question the set had not already answered.
///
/// The role map is the project's own tiers rather than a reading of them: Text,
/// Subtext 0 and Overlay 1 are what its style guide calls the primary, secondary
/// and subtle foregrounds, and Surface 0 is the raised surface it names for a
/// chip. Mauve is the accent its editor ports take by default.
///
/// **Nothing here needed correcting, and the comment grey is why.** Most of
/// these palettes put theirs under the floor, which is what earns each of them a
/// paragraph like this one; Catppuccin calls comments Overlay 2 and that
/// measures 5.81:1 on Base. `faint` sits one tier below it at Overlay 1 and
/// measures 4.44:1, the widest margin any `faint` here has, and still a
/// published step rather than a value invented beside one.
///
/// **The stripe goes down, not up, and the palette chose that for priel.**
/// Catppuccin publishes surfaces on both sides of Base - Mantle and Crust
/// below, Surface 0 and 1 above - and Surface 0 is already the chip a control
/// is drawn on here. So the stripe takes the step the palette still has spare:
/// Mantle, at 1.07:1 below Base, which every foreground in the set gains
/// contrast against rather than losing it. `faint` reads 4.75:1 on it.
const CATPPUCCIN: Theme = Theme {
    background: Color::Rgb(0x1e, 0x1e, 0x2e),
    text: Color::Rgb(0xcd, 0xd6, 0xf4),
    muted: Color::Rgb(0xa6, 0xad, 0xc8),
    faint: Color::Rgb(0x7f, 0x84, 0x9c),
    accent: Color::Rgb(0xcb, 0xa6, 0xf7),
    selection_fg: Color::Rgb(0x1e, 0x1e, 0x2e),
    selection_bg: Color::Rgb(0xcb, 0xa6, 0xf7),
    selection_idle_fg: Color::Rgb(0xcd, 0xd6, 0xf4),
    selection_idle_bg: Color::Rgb(0x52, 0x47, 0x6a),
    stripe_bg: Color::Rgb(0x18, 0x18, 0x25),
    control_fg: Color::Rgb(0xcb, 0xa6, 0xf7),
    control_bg: Color::Rgb(0x31, 0x32, 0x44),
    toggle_on_fg: Color::Rgb(0x1e, 0x1e, 0x2e),
    toggle_on_bg: Color::Rgb(0xa6, 0xe3, 0xa1),
    active: Color::Rgb(0xa6, 0xe3, 0xa1),
    notice: Color::Rgb(0xf9, 0xe2, 0xaf),
    error: Color::Rgb(0xf3, 0x8b, 0xa8),
    favorite: Color::Rgb(0xf5, 0xc2, 0xe7),
    queue: Color::Rgb(0x89, 0xb4, 0xfa),
    verdict_clean: Color::Rgb(0xa6, 0xe3, 0xa1),
    verdict_near: Color::Rgb(0xf9, 0xe2, 0xaf),
    verdict_altered: Color::Rgb(0xf3, 0x8b, 0xa8),
    verdict_unknown: Color::Rgb(0x7f, 0x84, 0x9c),
};

/// Tokyo Night, dark: the `night` style of `folke/tokyonight.nvim`, which is
/// its `storm` palette with the surface taken down to `#1a1b26`.
///
/// Taken from the plugin rather than from the `enkia/tokyo-night-vscode-theme`
/// it grew out of, for a reason that is about the source and not about the
/// look: the plugin publishes the palette as data with one name per colour, so
/// a role can be mapped onto a name and checked, where a VS Code theme
/// publishes scopes and leaves the palette to be read back out of them.
///
/// One deviation, and it is the usual one. `comment` measures 2.76:1 against
/// `bg`, under the floor a mark owes. Nothing here is lightened by hand,
/// though: Tokyo Night publishes a whole grey ramp - `comment`, `dark3`,
/// `dark5` - so `faint` is `dark5` at 4.10:1, a step the palette already
/// drew. `bg_highlight` is what the theme raises a highlighted line with, and
/// it carries the control chip here.
///
/// The stripe is `bg_dark`, the surface the theme paints its side panels with,
/// and it goes down for the reason `catppuccin`'s does: the step above is
/// `bg_highlight` and that is spoken for. At 1.05:1 it is the closest stripe in
/// the set to its own surface - `bg_dark` is a small step, and the palette's
/// own - and every foreground gains against it, `faint` reaching 4.31:1.
const TOKYO_NIGHT: Theme = Theme {
    background: Color::Rgb(0x1a, 0x1b, 0x26),
    text: Color::Rgb(0xc0, 0xca, 0xf5),
    muted: Color::Rgb(0xa9, 0xb1, 0xd6),
    faint: Color::Rgb(0x73, 0x7a, 0xa2),
    accent: Color::Rgb(0x7a, 0xa2, 0xf7),
    selection_fg: Color::Rgb(0x1a, 0x1b, 0x26),
    selection_bg: Color::Rgb(0x7a, 0xa2, 0xf7),
    selection_idle_fg: Color::Rgb(0xc0, 0xca, 0xf5),
    selection_idle_bg: Color::Rgb(0x37, 0x44, 0x65),
    stripe_bg: Color::Rgb(0x16, 0x16, 0x1e),
    control_fg: Color::Rgb(0x7a, 0xa2, 0xf7),
    control_bg: Color::Rgb(0x29, 0x2e, 0x42),
    toggle_on_fg: Color::Rgb(0x1a, 0x1b, 0x26),
    toggle_on_bg: Color::Rgb(0x9e, 0xce, 0x6a),
    active: Color::Rgb(0x9e, 0xce, 0x6a),
    notice: Color::Rgb(0xe0, 0xaf, 0x68),
    error: Color::Rgb(0xf7, 0x76, 0x8e),
    favorite: Color::Rgb(0xbb, 0x9a, 0xf7),
    queue: Color::Rgb(0x7d, 0xcf, 0xff),
    verdict_clean: Color::Rgb(0x9e, 0xce, 0x6a),
    verdict_near: Color::Rgb(0xe0, 0xaf, 0x68),
    verdict_altered: Color::Rgb(0xf7, 0x76, 0x8e),
    verdict_unknown: Color::Rgb(0x73, 0x7a, 0xa2),
};

/// Tokyo Night Day, the light sibling: the same plugin's `day` style, which it
/// derives by inverting the night palette and ships ready-made under `extras/`.
/// A third light option, and the only one whose foreground is a colour rather
/// than a near-black.
///
/// **The tightest palette in the set, and the only one whose whole grey ladder
/// had to be redrawn.** `fg` measures 4.52:1 on `bg` - two hundredths above what
/// prose owes - and two roles have to fit underneath it, one of which is prose
/// too and owes the same 4.5. The published greys do not reach it: `fg_dark` is
/// 3.57:1 and `comment` 2.54:1. There is no ladder to be had under a 4.52 top
/// step, so the top step moves. `text` is `fg` darkened to 7.28:1 and `muted` is
/// `fg_dark` darkened to 5.07:1, each along its own hue; `faint` is `dark5`
/// unchanged at 3.71:1, the one rung that already cleared.
///
/// `accent` is `blue` darkened for the same kind of reason. Published it is
/// 3.11:1 on the background, which passes, and 2.41:1 on `bg_highlight` - the
/// chip a control is drawn on - which does not, and a control is the one thing
/// that must be legible before it can be pointed at. Darkened it is 4.50:1 and
/// 3.50:1.
///
/// `error` is `red1` rather than `red`, which is the mapping the theme makes
/// itself: it paints its own error group with `red1`. `red` measures 3.01:1, and
/// a hundredth above the floor is not a margin.
///
/// Nothing else moved. The three grades are the palette's own `green`, `yellow`
/// and `red1` - `yellow` is literally what the theme names its warning colour -
/// and the olive and the ochre sit about as far apart here as gruvbox-light's
/// pair do, which is the closest any shipped palette puts them.
///
/// The stripe steps up, mirroring the night palette's step down - this is that
/// palette inverted, so its second surface is above `bg` rather than below it -
/// and up is also what the tightest palette in the set can afford. At 1.07:1
/// nothing on it loses: `faint` goes from 3.71:1 to 3.96:1 and `active` from
/// 4.04:1 to 4.31:1. A stripe darkened out of this surface would have taken
/// both back towards a floor they were already sitting near.
const TOKYO_NIGHT_DAY: Theme = Theme {
    background: Color::Rgb(0xe1, 0xe2, 0xe7),
    text: Color::Rgb(0x26, 0x43, 0x86),
    muted: Color::Rgb(0x4e, 0x5b, 0x8d),
    faint: Color::Rgb(0x68, 0x70, 0x9a),
    accent: Color::Rgb(0x25, 0x64, 0xba),
    selection_fg: Color::Rgb(0xe1, 0xe2, 0xe7),
    selection_bg: Color::Rgb(0x25, 0x64, 0xba),
    selection_idle_fg: Color::Rgb(0x26, 0x43, 0x86),
    selection_idle_bg: Color::Rgb(0xa9, 0xbc, 0xda),
    stripe_bg: Color::Rgb(0xe9, 0xe9, 0xec),
    control_fg: Color::Rgb(0x25, 0x64, 0xba),
    control_bg: Color::Rgb(0xc4, 0xc8, 0xda),
    toggle_on_fg: Color::Rgb(0xe1, 0xe2, 0xe7),
    toggle_on_bg: Color::Rgb(0x58, 0x75, 0x39),
    active: Color::Rgb(0x58, 0x75, 0x39),
    notice: Color::Rgb(0x8c, 0x6c, 0x3e),
    error: Color::Rgb(0xc6, 0x43, 0x43),
    favorite: Color::Rgb(0x98, 0x54, 0xf1),
    queue: Color::Rgb(0x00, 0x71, 0x97),
    verdict_clean: Color::Rgb(0x58, 0x75, 0x39),
    verdict_near: Color::Rgb(0x8c, 0x6c, 0x3e),
    verdict_altered: Color::Rgb(0xc6, 0x43, 0x43),
    verdict_unknown: Color::Rgb(0x68, 0x70, 0x9a),
};

#[cfg(test)]
mod tests {
    use super::{Theme, ThemeName};
    use clap::ValueEnum;
    use ratatui::style::Color;
    use ratatui::text::Span;

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

    /// Goal: the pairs that carry their own background - the selected row, a
    /// control, a toggle that is on, and now the stripe - are read against that
    /// background and not against the surface, so measuring them against the
    /// surface would prove nothing.
    ///
    /// The stripe is three pairs rather than one because a striped backing is a
    /// whole row: an ordinary track row wears `text`, the row in the speakers
    /// wears `active`, and the tabs that are not the one you are on wear
    /// `faint`. A stripe that only cleared against `text` would take the other
    /// two under the floor on exactly the rows that say the most.
    #[test]
    fn every_pair_is_legible_against_its_own_backing() {
        for (name, t) in all() {
            let pairs = [
                ("selection", t.selection_fg, t.selection_bg, MARK_FLOOR),
                // The second cursor. It repaints the row exactly as the first
                // one does - a cursor owns the row it is on, focused or not -
                // so it is one pair here and not one per foreground.
                (
                    "selection_idle",
                    t.selection_idle_fg,
                    t.selection_idle_bg,
                    MARK_FLOOR,
                ),
                ("control", t.control_fg, t.control_bg, MARK_FLOOR),
                ("toggle_on", t.toggle_on_fg, t.toggle_on_bg, MARK_FLOOR),
                ("stripe/text", t.text, t.stripe_bg, PROSE_FLOOR),
                ("stripe/active", t.active, t.stripe_bg, MARK_FLOOR),
                ("stripe/faint", t.faint, t.stripe_bg, MARK_FLOOR),
            ];
            for (role, fg, bg, floor) in pairs {
                let Some(ratio) = contrast(fg, bg) else {
                    continue;
                };
                assert!(
                    ratio >= floor,
                    "{name:?}: {role} is {ratio:.2}:1 against its own backing, under {floor}"
                );
            }
        }
    }

    /// The least the idle cursor may differ from the surface. Above
    /// [`STRIPE_MAX`] on purpose: a cursor inside the range a stripe lives in
    /// is a cursor a striped row can be mistaken for.
    const CURSOR_FROM_SURFACE: f64 = 1.35;

    /// The least the two cursors may differ from each other. Below this,
    /// "which of the two lists am I driving?" is answered by a shade nobody
    /// can name.
    const CURSOR_FROM_CURSOR: f64 = 1.5;

    /// The least the idle cursor may differ from the stripe it lands on.
    const CURSOR_FROM_STRIPE: f64 = 1.25;

    /// Goal: two focusable lists put two cursors on screen at once, and the
    /// pair has to answer three questions at a glance - which row the cursor is
    /// on, which of the two lists is being driven, and neither of those
    /// confused with a striped row. Method: measure the idle cursor's backing
    /// against all three of the backings it has to be told apart from.
    ///
    /// The last assertion is the one that says which of them is the quiet one:
    /// the idle cursor sits *between* the surface and the focused cursor, so
    /// focusing a list can only make its cursor louder. A palette that inverted
    /// that would be saying the list nobody is driving is the important one.
    #[test]
    fn the_idle_cursor_is_the_quiet_one_and_is_never_a_stripe() {
        for (name, t) in all() {
            // `None` is the terminal palette, whose backings are the user's.
            let Some(surface) = contrast(t.selection_idle_bg, t.background) else {
                continue;
            };
            assert!(
                surface >= CURSOR_FROM_SURFACE,
                "{name:?}: the idle cursor is {surface:.2}:1 on the surface, inside the \
                 range a stripe lives in"
            );
            let Some(apart) = contrast(t.selection_idle_bg, t.selection_bg) else {
                continue;
            };
            assert!(
                apart >= CURSOR_FROM_CURSOR,
                "{name:?}: the two cursors are {apart:.2}:1 apart, under {CURSOR_FROM_CURSOR}"
            );
            let Some(striped) = contrast(t.selection_idle_bg, t.stripe_bg) else {
                continue;
            };
            assert!(
                striped >= CURSOR_FROM_STRIPE,
                "{name:?}: the idle cursor is {striped:.2}:1 from the stripe, under \
                 {CURSOR_FROM_STRIPE}"
            );
            let Some(focused) = contrast(t.selection_bg, t.background) else {
                continue;
            };
            assert!(
                surface < focused,
                "{name:?}: the idle cursor is {surface:.2}:1 off the surface and the focused \
                 one is {focused:.2}:1, so the quiet cursor is the loud one"
            );
        }
    }

    /// The least a stripe may differ from the surface and still be seen at all.
    const STRIPE_MIN: f64 = 1.03;

    /// The most it may differ before it stops being a stripe. Past this the
    /// alternate rows read as two lists rather than one, and the row a stripe
    /// is meant to help you follow is the one it starts to fight.
    const STRIPE_MAX: f64 = 1.30;

    /// The least a stripe may differ from the row the cursor is on. A striped
    /// row that reads as a selected one is worse than no stripe at all.
    const STRIPE_FROM_SELECTION: f64 = 1.5;

    /// Goal: a zebra stripe is a whisper. It has to be visible enough to carry
    /// the eye across a two-hundred-cell row and quiet enough that it is never
    /// read as a second selection. Method: measure it against both of the
    /// backings it sits between - the surface it alternates with, and the
    /// selection it must not be confused for.
    #[test]
    fn the_stripe_is_a_whisper_and_never_a_second_selection() {
        for (name, t) in all() {
            let Some(ratio) = contrast(t.stripe_bg, t.background) else {
                continue;
            };
            assert!(
                ratio >= STRIPE_MIN,
                "{name:?}: the stripe is {ratio:.3}:1 on the surface, too faint to see"
            );
            assert!(
                ratio <= STRIPE_MAX,
                "{name:?}: the stripe is {ratio:.3}:1 on the surface, loud enough to read \
                 as a state of its own"
            );
            let Some(apart) = contrast(t.stripe_bg, t.selection_bg) else {
                continue;
            };
            assert!(
                apart >= STRIPE_FROM_SELECTION,
                "{name:?}: the stripe is {apart:.2}:1 from the selected row's backing"
            );
        }
    }

    /// Goal: one palette declines the stripe, and that is a decision rather
    /// than a gap. `terminal` paints in ANSI indices and [`Color::Reset`]
    /// precisely so it inherits a palette priel cannot read; guessing a stripe
    /// against an unknown background is the overstatement that theme exists to
    /// avoid. Every other palette knows its own surface and owes a stripe.
    ///
    /// Method: assert the shape of the decision on both sides, so neither a
    /// palette that quietly forgot its stripe nor a `terminal` that quietly
    /// grew one can pass.
    #[test]
    fn only_the_terminal_palette_declines_a_stripe() {
        for (name, t) in all() {
            if name == ThemeName::Terminal {
                assert_eq!(
                    t.stripe_bg, t.background,
                    "{name:?} cannot know the surface, so it must not stripe against it"
                );
            } else {
                assert_ne!(
                    t.stripe_bg, t.background,
                    "{name:?} knows its own surface and owes it a stripe"
                );
            }
        }
    }

    /// Goal: the palette that declines the stripe says so where a reader
    /// chooses one, rather than leaving a missing stripe to be noticed as a
    /// bug. The note is the flag's own help, so the picker, `--help`, the man
    /// page and all three completions carry the same sentence.
    #[test]
    fn the_palette_that_declines_a_stripe_says_so_where_it_is_chosen() {
        let note = super::note(ThemeName::Terminal);
        assert!(
            note.contains("stripe"),
            "the palette with no stripe says nothing about it: {note:?}"
        );
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
            t.selection_idle_fg,
            t.selection_idle_bg,
            t.stripe_bg,
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

    /// Goal: the picker lays every palette's name into one fixed-width field and
    /// draws that palette's three grade marks immediately after it, so a name
    /// wider than the field shunts the swatch out of line for every row below
    /// it. `tokyo-night-day` fills the field exactly, which is close enough to
    /// the edge to be worth a guard rather than a look.
    ///
    /// Method: read the width out of the renderer's own constant rather than
    /// writing 15 down a second time here, and measure each name the way
    /// ratatui measures what it paints.
    #[test]
    fn every_name_fits_the_field_the_picker_lays_it_into() {
        let field: usize = include_str!("ui.rs")
            .split_once("const THEME_NAME_FIELD: usize = ")
            .and_then(|(_, rest)| rest.split_once(';'))
            .and_then(|(width, _)| width.trim().parse().ok())
            .expect("ui.rs declares how wide the name field is");
        for name in super::OFFERED {
            let label = super::label(*name);
            let width = Span::raw(&label).width();
            assert!(
                width <= field,
                "{name:?} is {width} cells wide, over the {field} the picker lays out"
            );
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
