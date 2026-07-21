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

//! Rendering. Also records list/progress rects into `App` for mouse hit-testing.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use std::fmt::Write as _;

use priel_player::graph::{SinkStage, SinkVolume};
use priel_player::{Alteration, Fidelity, OutputAccess, StreamVolume, Verdict};

use crate::app::{App, GraphRow, GraphRowKind, Hit, Mode, View};
use crate::cli::ThemeName;
use crate::theme::{self, Theme};

pub fn render(f: &mut Frame, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header / tabs
        Constraint::Min(1),    // list
        Constraint::Length(3), // now-playing
    ])
    .split(f.area());

    // The surface first, so a theme's background is the background rather than
    // a suggestion. `Clear` resets the cells it covers back to the terminal's
    // own colours, so every overlay repaints this on its own block.
    f.render_widget(Block::default().style(app.theme().surface()), f.area());

    // Hit boxes are geometry, so the renderer owns them. Rebuilt every frame.
    app.hits.clear();
    header(f, app, rows[0]);
    list(f, app, rows[1]);
    now_playing(f, app, rows[2]);

    // Drawn last so it sits over everything, and after the hit boxes above have
    // been registered - `App` ignores them while the overlay is up.
    if app.mode == Mode::Help {
        let area = f.area();
        help_overlay(f, app, area);
    }
    if app.mode == Mode::Log {
        log_overlay(f, f.area(), app);
    }
    if app.mode == Mode::Graph {
        graph_overlay(f, f.area(), app);
    }
    if app.mode == Mode::Devices {
        device_overlay(f, f.area(), app);
    }
    if app.mode == Mode::Themes {
        theme_overlay(f, f.area(), app);
    }
    if app.mode == Mode::Credentials {
        let area = f.area();
        credentials_overlay(f, app, area);
    }
    if app.mode == Mode::Login {
        let area = f.area();
        login_overlay(f, app, area);
    }
    if app.mode == Mode::AddTo {
        add_to_overlay(f, f.area(), app);
    }
    if app.mode == Mode::Prompt {
        let area = f.area();
        prompt_overlay(f, app, area);
    }
    if app.mode == Mode::Confirm {
        let area = f.area();
        confirm_overlay(f, app, area);
    }
}

/// The box a playlist name is typed into.
///
/// Modal like the consent screen, and for a milder version of the same reason:
/// the keys being pressed are text, so anything behind this that answered to a
/// letter would answer to the name being typed.
fn prompt_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let Some(question) = app.prompt_question() else {
        return;
    };
    // Whatever the header and the bottom row registered this frame is behind
    // this box; clearing before anything else means a terminal too short to
    // draw the controls leaves nothing clickable rather than last frame's.
    app.hits.clear();
    let width = area.width.saturating_sub(4).min(64);
    let height = 5u16.min(area.height);
    let rect = centred(area, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(format!(" {question} "));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    // The tail rather than the head, and the same cursor glyph the paste box
    // uses: a name longer than the box should show what is still being typed.
    let room = usize::from(inner.width).saturating_sub(3);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" ".to_string(), Style::default()),
            Span::styled(tail(&app.prompt_text, room), Style::default().fg(t.text)),
            Span::styled("▏".to_string(), Style::default().fg(t.accent)),
        ])),
        Rect { height: 1, ..inner },
    );

    if inner.height < 3 {
        return;
    }
    let line = Rect {
        y: inner.y.saturating_add(2),
        height: 1,
        ..inner
    };
    let dim = Style::default().fg(t.faint);
    let key = Style::default().fg(t.accent);
    let mut bar = ControlBar::new(line);
    bar.label(" [", dim);
    bar.button("Enter", Hit::SubmitPrompt, key);
    bar.label("] save   [", dim);
    bar.button("Esc", Hit::CancelPrompt, key);
    bar.label("] cancel", dim);
    app.hits.extend(bar.hits.iter().copied());
    f.render_widget(Paragraph::new(Line::from(bar.spans)), line);
}

/// The question asked before something that cannot be taken back.
///
/// **Nothing outside the two controls answers it.** `App::on_mouse` swallows
/// every other click while this is up, and the hit boxes are the only way in -
/// so a click that lands on the prose, on the border, or on the list behind
/// does nothing at all. The one control that destroys something is drawn in the
/// error colour and says what it will destroy, rather than reading "OK".
fn confirm_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let Some(lines) = app.confirm_question() else {
        return;
    };
    let verb = app.confirm_verb().unwrap_or("do it");
    app.hits.clear();
    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let width = area.width.saturating_sub(4).min(64);
    let height = rows.saturating_add(4).min(area.height);
    let rect = centred(area, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        // The error colour, not the accent: this box is the one place in priel
        // where saying yes cannot be walked back.
        .border_style(Style::default().fg(t.error))
        .title(" Are you sure? ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    let body: Vec<Line<'static>> = lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            // The first line names the thing; the rest say what it means.
            let style = if i == 0 {
                Style::default().fg(t.text)
            } else {
                Style::default().fg(t.muted)
            };
            Line::from(Span::styled(format!(" {text}"), style))
        })
        .collect();
    f.render_widget(Paragraph::new(body), inner);

    let choices = rows.saturating_add(1);
    if choices >= inner.height {
        return; // too short to draw them, so there is nothing to click either
    }
    let line = Rect {
        y: inner.y.saturating_add(choices),
        height: 1,
        ..inner
    };
    let dim = Style::default().fg(t.faint);
    let mut bar = ControlBar::new(line);
    bar.label(" [", dim);
    bar.button("y", Hit::ConfirmYes, Style::default().fg(t.error));
    bar.label(format!("] {verb}   ["), dim);
    // Backing out is the easy one to reach, and the one every other key here
    // already means.
    bar.button("n", Hit::ConfirmNo, Style::default().fg(t.accent));
    bar.label("] keep it   (Esc)", dim);
    app.hits.extend(bar.hits.iter().copied());
    f.render_widget(Paragraph::new(Line::from(bar.spans)), line);
}

/// The picker that chooses which playlist a track goes into.
///
/// The output picker's shape, windowed the same way and hit-boxed in the same
/// left-to-right walk, so the gesture a listener already knows works here.
fn add_to_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme();
    let width = area.width.saturating_sub(4).min(80);
    let height = area.height.saturating_sub(2);
    let rect = centred(area, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Add to playlist ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    // Cleared before the early return, so a terminal too short to draw a row
    // cannot leave the last frame's boxes behind to be clicked.
    app.add_rows.clear();
    if inner.height <= 1 {
        return;
    }

    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if app.playlists.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Fetching your playlists…",
                Style::default().fg(t.faint),
            ))),
            body,
        );
    } else {
        add_to_rows(f, app, body);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  j k move · g G ends · Enter add · click · Esc or q to close",
            Style::default().fg(t.faint),
        ))),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// The picker's rows, windowed onto the selection.
fn add_to_rows(f: &mut Frame, app: &mut App, body: Rect) {
    let t = app.theme();
    let total = app.playlists.len();
    let h = body.height as usize;
    let selected = app.add_selected_row();
    if selected < app.add_offset {
        app.add_offset = selected;
    } else if selected >= app.add_offset + h {
        app.add_offset = selected + 1 - h;
    }
    if app.add_offset >= total {
        app.add_offset = 0;
    }

    let name_width = usize::from(body.width).saturating_sub(14);
    let mut rows = Vec::new();
    for (i, index) in (app.add_offset..(app.add_offset + h).min(total)).enumerate() {
        let p = &app.playlists[index];
        let text = format!(
            "  {}  {:>5} tracks",
            field(&p.title, name_width),
            p.num_tracks
        );
        let style = if index == selected {
            t.selection()
        } else {
            Style::default()
        };
        let rect = Rect {
            x: body.x,
            y: body.y + u16::try_from(i).unwrap_or(u16::MAX),
            width: body.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(text).style(style), rect);
        rows.push((rect, index));
    }
    app.add_rows = rows;
}

/// How far an overlay's content sits from its own border.
///
/// Two cells, everywhere. Every footer already used it and most bodies did; the
/// two that did not gave their box a left edge of its own.
const OVERLAY_INDENT: u16 = 2;

/// `area` moved in from the left by [`OVERLAY_INDENT`], never past its own end.
fn indented(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(OVERLAY_INDENT),
        width: area.width.saturating_sub(OVERLAY_INDENT),
        ..area
    }
}

/// A box of the given size in the middle of `area`.
///
/// Extracted because five overlays did the same arithmetic, and one of them
/// getting it subtly wrong is a box that hangs off the edge of the terminal.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// The sign-in screen.
///
/// The redirect lands on the service's own site, which priel cannot listen on,
/// so the last step is unavoidably a paste. Everything around it is made as
/// short as possible: the browser is already open, the box is already focused,
/// and one paste plus Enter finishes the job.
fn login_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    // Read before the mutable borrow the hit boxes need, as the header does.
    let t = app.theme();
    let flow = app
        .login()
        .map(|f| (f.is_busy(), f.pasted.clone(), f.status.clone()));
    // Modal: whatever the header and the bottom row registered this frame is
    // behind this screen and must not be reachable through it. Cleared before
    // the guard below, so a mode with no flow to draw is not a mode in which the
    // header underneath quietly answers to clicks.
    app.hits.clear();
    let Some((busy, pasted, status)) = flow else {
        return;
    };
    let width = area.width.min(76);
    let height = 16u16.min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Sign in ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let dim = Style::default().fg(t.muted);
    let key = Style::default().fg(t.accent);
    let mut lines = vec![
        Line::from(Span::styled(
            "A browser should have opened. Sign in there.",
            dim,
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "You will land on a page that looks like an error. That is",
            dim,
        )),
        Line::from(Span::styled(
            "expected. Copy its address and paste it below.",
            dim,
        )),
        Line::raw(""),
    ];

    if busy {
        lines.push(Line::from(Span::styled(
            "    signing in…",
            Style::default().fg(t.notice),
        )));
    } else {
        // A pasted URL is far wider than the box; show the tail, which is where
        // the code sits, so the user can see something arrived.
        let shown = tail(&pasted, inner.width.saturating_sub(6) as usize);
        lines.push(Line::from(vec![
            Span::styled("    ", dim),
            Span::styled(
                if shown.is_empty() {
                    "paste here…".to_string()
                } else {
                    shown
                },
                if pasted.is_empty() {
                    Style::default().fg(t.faint)
                } else {
                    Style::default().fg(t.text)
                },
            ),
            Span::styled("▏", key),
        ]));
    }

    lines.push(Line::raw(""));
    if let Some(status) = &status {
        lines.push(Line::from(Span::styled(
            format!("    {status}"),
            Style::default().fg(t.notice),
        )));
        lines.push(Line::raw(""));
    }
    // The rows of actions are placed by hand rather than pushed onto the
    // paragraph, because a control needs a rect of its own to register the hit
    // box that makes it one. Everything above is prose.
    let used = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    f.render_widget(Paragraph::new(lines), inner);
    login_controls(f, app, inner, used);
}

/// The sign-in screen's four actions, as controls.
///
/// Laid out and hit-boxed in the same left-to-right walk the header's are, from
/// the row `used` lines down. A row the box was too short to draw registers
/// nothing: a control that was never painted must not answer to a click.
fn login_controls(f: &mut Frame, app: &mut App, inner: Rect, used: u16) {
    let t = app.theme();
    let dim = Style::default().fg(t.muted);
    let key = Style::default().fg(t.accent);
    let mut row = |y: u16, build: &dyn Fn(&mut ControlBar)| {
        if y >= inner.height {
            return;
        }
        let line = Rect {
            y: inner.y.saturating_add(y),
            height: 1,
            ..inner
        };
        let mut bar = ControlBar::new(line);
        build(&mut bar);
        app.hits.extend(bar.hits.iter().copied());
        f.render_widget(Paragraph::new(Line::from(bar.spans)), line);
    };

    row(used, &|bar: &mut ControlBar| {
        bar.label("    [", dim);
        bar.button("Enter", Hit::SubmitLogin, key);
        bar.label("] sign in   [", dim);
        bar.button("Ctrl-O", Hit::ReopenBrowser, key);
        bar.label("] reopen browser   [", dim);
        bar.button("Ctrl-U", Hit::ClearPaste, key);
        bar.label("] clear", dim);
    });
    row(used.saturating_add(1), &|bar: &mut ControlBar| {
        bar.label("    [", dim);
        bar.button("Esc", Hit::CancelLogin, key);
        bar.label("] cancel", dim);
    });
}

/// The last `width` characters, so a long URL shows its tail.
fn tail(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    s.chars().skip(count - width).collect()
}

/// Draw the consent screen.
///
/// Its three answers - download it, not now, quit - are controls, laid out and
/// hit-boxed in the same walk the header's are. A stray click is still not
/// consent: the app answers a click here only where it lands on one of these.
fn credentials_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let status = app.credential_status().map(ToString::to_string);
    // Modal: whatever the header and the bottom row registered this frame is
    // behind this screen and must not be reachable through it.
    app.hits.clear();
    let rows = u16::try_from(CREDENTIALS_PROMPT.len()).unwrap_or(u16::MAX);
    let width = area.width.min(78);
    let height = rows.saturating_add(6).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.notice))
        .title(" priel needs a client identity ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines: Vec<Line<'static>> = CREDENTIALS_PROMPT
        .iter()
        .map(|l| {
            // The address and the destination path are the two facts a reader
            // should be able to find without reading the prose.
            let emphasised = l.contains("github.com") || l.contains("credentials.json");
            let style = if emphasised {
                Style::default().fg(t.accent)
            } else {
                Style::default().fg(t.muted)
            };
            Line::from(Span::styled((*l).to_string(), style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);

    // A blank line after the prose, then the choices, then whatever the last
    // attempt had to say. Placed by hand rather than pushed onto the paragraph
    // because the controls need a rect of their own to register hit boxes in.
    let dim = Style::default().fg(t.muted);
    let key = Style::default().fg(t.accent);
    let choices = rows.saturating_add(1);
    if choices >= inner.height {
        return; // too short to draw them, so nothing to click either
    }
    let line = Rect {
        y: inner.y.saturating_add(choices),
        height: 1,
        ..inner
    };
    let mut bar = ControlBar::new(line);
    bar.label("    [", dim);
    bar.button("f", Hit::FetchCredentials, key);
    bar.label("] download it   [", dim);
    bar.button("Esc", Hit::DeclineCredentials, key);
    bar.label("] not now   [", dim);
    bar.button("q", Hit::Quit, key);
    bar.label("] quit", dim);
    app.hits.extend(bar.hits.iter().copied());
    f.render_widget(Paragraph::new(Line::from(bar.spans)), line);

    if let Some(status) = status
        && choices.saturating_add(1) < inner.height
    {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("    {status}"),
                Style::default().fg(t.notice),
            ))),
            Rect {
                y: line.y.saturating_add(1),
                height: 1,
                ..inner
            },
        );
    }
}

/// The first-run consent screen for obtaining a client identity.
///
/// Kept short enough to actually be read, but it still has to carry four facts:
/// what is downloaded, from where, that the key is not priel's, and where it
/// lands. A user who later learns whose key this is should find nothing here
/// they were not told - that is the whole point of asking rather than doing it
/// silently.
const CREDENTIALS_PROMPT: &[&str] = &[
    "priel needs a client key before it can sign you in, and it does not",
    "ship one of its own.",
    "",
    "The key is published in an open-source project that the other Linux",
    "players for this service all rely on:",
    "",
    "    github.com/EbbLabs/python-tidal",
    "",
    "It is not priel's key and priel is not an official client. This is",
    "how every native player on Linux works.",
    "",
    "If you continue, priel downloads it once and saves it to",
    "~/.local/state/priel/. Nothing of yours is sent anywhere, and you",
    "will not be asked again.",
    "",
    "Prefer to do it yourself? Put a client_id and client_secret in",
    "~/.config/priel/credentials.json and restart priel.",
];

/// One line of the reference: the keys it names, and what they do.
///
/// A key carries the action it runs, so the reference is not a description of
/// the interface but the interface itself - clicking `M` here opens the log the
/// same way pressing `M` does. `None` marks a line that names no action: the
/// vocabulary of the fidelity badges, and the mouse gestures that have no key.
struct HelpRow {
    keys: &'static [(&'static str, Option<Hit>)],
    what: &'static str,
}

const fn row(keys: &'static [(&'static str, Option<Hit>)], what: &'static str) -> HelpRow {
    HelpRow { keys, what }
}

/// The complete reference, in two columns.
///
/// **This is where the mouse reaches everything.** The bottom row carries only
/// what is used constantly and drops hints as the terminal narrows, and the
/// header runs out of width too; this overlay is sized to the terminal and is
/// opened by a `[?]` that is never dropped, so a key that is listed here can be
/// clicked at any width. That is what lets rarely-used actions stay out of the
/// competition for space down there without becoming keyboard-only.
const HELP_LEFT: &[(&str, &[HelpRow])] = &[
    (
        "Views",
        &[
            row(&[("Tab", Some(Hit::CycleView))], "cycle views"),
            row(
                &[
                    ("1", Some(Hit::View(View::Favorites))),
                    ("2", Some(Hit::View(View::Playlists))),
                    ("3", Some(Hit::View(View::Search))),
                    ("4", Some(Hit::View(View::Mixes))),
                ],
                "jump to a view",
            ),
            row(&[("Enter", Some(Hit::Enter))], "open playlist or mix"),
            // Named for what it does rather than for one of the two lists it
            // can land on: a control that promised the playlists would be wrong
            // half the time it was clicked.
            row(&[("Esc", Some(Hit::Back))], "back to the list"),
            row(&[("r", Some(Hit::Reload))], "reload this list"),
            row(&[("M", Some(Hit::Log))], "recent log messages"),
        ],
    ),
    (
        "Move",
        &[
            row(
                &[("j", Some(Hit::MoveDown)), ("k", Some(Hit::MoveUp))],
                "move the selection",
            ),
            row(
                &[("Down", Some(Hit::MoveDown)), ("Up", Some(Hit::MoveUp))],
                "move the selection",
            ),
            row(
                &[("J", Some(Hit::PageDown)), ("K", Some(Hit::PageUp))],
                "page down / up",
            ),
            row(
                &[
                    ("Ctrl-D", Some(Hit::HalfPageDown)),
                    ("Ctrl-U", Some(Hit::HalfPageUp)),
                ],
                "half page down / up",
            ),
            row(
                &[("g", Some(Hit::Top)), ("G", Some(Hit::Bottom))],
                "first / last row",
            ),
        ],
    ),
    (
        "Output",
        &[
            row(&[("D", Some(Hit::Graph))], "the output report"),
            row(&[("d", Some(Hit::Devices))], "choose the device"),
            // The picker carries its own toggle, and `x` outside the picker is
            // not an action at all, so this line names a key rather than
            // offering a control that would do nothing where it was clicked.
            row(&[("x", None)], "exclusive, in the picker"),
            // Last of the actions, and deliberately above the glossary below
            // it: a short terminal clips this column from the bottom, and what
            // it may take is a word being explained, never a control.
            row(&[("0", Some(Hit::VolUnity))], "restore unity gain"),
            row(&[("exclusive", None)], "the device is priel's"),
            row(&[("direct", None)], "the card itself, no mixer"),
            row(&[("DAC", None)], "live from the device"),
            row(&[("OUT", None)], "what the server took"),
            row(&[("bit-perfect", None)], "nothing altered"),
            row(&[("near", None)], "level changed only"),
            row(&[("resampled", None)], "rate changed"),
            row(&[("truncated", None)], "format too narrow"),
            // The mark decision nine turns on: a stage that cannot exist is
            // fully evidenced, so this appears only where one that could exist
            // went unread.
            row(&[("✓?", None)], "a stage went unread"),
            row(&[("sink", None)], "the shared output level"),
            row(&[("⚠ in D", None)], "the node that did it"),
            row(&[("permitted", None)], "rates the server may use"),
        ],
    ),
];

const HELP_RIGHT: &[(&str, &[HelpRow])] = &[
    (
        "Play",
        &[
            row(&[("Enter", Some(Hit::Enter))], "play selected track"),
            row(&[("space", Some(Hit::PlayPause))], "play / pause"),
            row(
                &[("h", Some(Hit::SeekBack)), ("l", Some(Hit::SeekFwd))],
                "seek -5s / +5s",
            ),
            row(
                &[("L", Some(Hit::Next)), ("H", Some(Hit::Prev))],
                "next / previous track",
            ),
            row(
                &[("n", Some(Hit::Next)), ("p", Some(Hit::Prev))],
                "next / previous track",
            ),
            row(&[("s", Some(Hit::Shuffle))], "shuffle this view"),
            row(
                &[("f", Some(Hit::FavoriteSelected))],
                "favorite selected track",
            ),
            row(
                &[("F", Some(Hit::FavoriteNowPlaying))],
                "favorite playing track",
            ),
            // The vocabulary, not an action: the glyphs are what the two keys
            // above put on a row, and hollow means "not, as far as priel has
            // been told" - it loads the favorites a page at a time.
            row(&[("♥ ♡", None)], "kept / not kept"),
            row(
                &[("+", Some(Hit::VolUp)), ("-", Some(Hit::VolDown))],
                "volume up / down",
            ),
        ],
    ),
    (
        // Every one of these is reachable only from here with the mouse: none
        // of them is common enough to earn width on the bottom row, and the
        // reference is where an action that is not constantly used belongs.
        "Playlists",
        &[
            row(&[("N", Some(Hit::NewPlaylist))], "new playlist"),
            row(&[("R", Some(Hit::RenamePlaylist))], "rename this playlist"),
            row(&[("a", Some(Hit::AddToPlaylist))], "add track to playlist"),
            row(
                &[("X", Some(Hit::RemoveSelected))],
                "delete playlist / track",
            ),
            // The vocabulary again: `X` asks first, and this says so where the
            // key is read rather than only where it is pressed.
            row(&[("y", None), ("n", None)], "answer that question"),
        ],
    ),
    (
        "Find",
        &[
            row(&[("/", Some(Hit::Filter))], "filter this list"),
            row(&[("i", Some(Hit::EditSearch))], "edit search query"),
            // Accepting or cancelling what has been typed belongs to the box
            // being typed in. There is nothing to point at here, and no control
            // that could stand for a keystroke aimed at a text field.
            row(&[("Enter", None), ("Esc", None)], "accept / cancel"),
        ],
    ),
    (
        "Session",
        &[
            row(&[("A", Some(Hit::SignIn))], "sign in again"),
            row(&[("t", Some(Hit::Themes))], "colour theme"),
            row(&[("q", Some(Hit::Quit))], "quit priel"),
        ],
    ),
    (
        "Mouse",
        &[
            // The gestures themselves: these have no key and need none, because
            // every action they reach is bound above.
            row(&[("wheel", None)], "move the selection"),
            row(&[("double-click", None)], "play a row"),
            row(&[("click drag", None)], "seek on progress bar"),
            row(&[("click", None)], "any key printed here"),
        ],
    ),
];

/// How many lines a column of the reference occupies, blank separators included.
fn help_height(sections: &[(&str, &[HelpRow])]) -> usize {
    sections
        .iter()
        .map(|(_, rows)| rows.len() + 1)
        .sum::<usize>()
        + sections.len().saturating_sub(1)
}

/// Where the description column starts, in cells from the left of the overlay.
const HELP_KEY_FIELD: u16 = 16;

/// Draw one column of the reference, registering a hit box for every key that
/// names an action.
///
/// The hit boxes are built in the same left-to-right walk that lays the spans
/// out, through the same `ControlBar` the header uses, so what was painted and
/// what is clickable are the same cells. Rows past the bottom of `area` are
/// neither drawn nor registered: a control clipped off the overlay must not
/// still answer to a click at the place it would have been.
fn help_column(
    f: &mut Frame,
    area: Rect,
    sections: &[(&str, &[HelpRow])],
    hits: &mut Vec<(Rect, Hit)>,
    t: &Theme,
    skip: usize,
) {
    let dim = Style::default().fg(t.faint);
    // Laid out in content lines, drawn in screen rows. Every line is counted
    // whether or not it is drawn - a header scrolled off the top still moves
    // its own rows down - and a line outside the window registers no hit box,
    // so a control that is not on screen does not answer to a click where it
    // used to be.
    let mut line = 0usize;
    let place = |line: &mut usize| {
        let at = *line;
        *line = line.saturating_add(1);
        let offset = at.checked_sub(skip)?;
        let y = area
            .y
            .checked_add(u16::try_from(offset).unwrap_or(u16::MAX))?;
        (y < area.y.saturating_add(area.height)).then_some(Rect {
            y,
            height: 1,
            ..area
        })
    };
    for (i, (title, rows)) in sections.iter().enumerate() {
        if i > 0 {
            let _ = place(&mut line); // a blank line between sections
        }
        if let Some(head) = place(&mut line) {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    (*title).to_string(),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ))),
                head,
            );
        }
        for r in *rows {
            let Some(row) = place(&mut line) else {
                continue;
            };
            let mut bar = ControlBar::new(row);
            bar.label("  ", Style::default());
            for (n, (key, hit)) in r.keys.iter().enumerate() {
                if n > 0 {
                    bar.label(" ", Style::default());
                }
                let style = Style::default().fg(t.text);
                match hit {
                    Some(h) => bar.button(*key, *h, style),
                    None => bar.label(*key, style),
                }
            }
            // Pad to the description column, and never to nothing: a key field
            // wider than the column must not run into the description.
            let used = bar.x.saturating_sub(area.x);
            let pad = usize::from(HELP_KEY_FIELD.saturating_sub(used)).max(1);
            bar.label(" ".repeat(pad), Style::default());
            bar.label(r.what, dim);
            hits.extend(bar.hits.iter().copied());
            f.render_widget(Paragraph::new(Line::from(bar.spans)), row);
        }
    }
}

/// The recent diagnostics, newest last.
///
/// Modal and deliberately plain. This is the answer to "something just went
/// wrong and I do not want to leave the player to find out what" - the same
/// lines that are in the log file, without going to look for it.
fn log_overlay(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let width = area.width.saturating_sub(4).min(120);
    let height = area.height.saturating_sub(2);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Recent log ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    // One row goes to the way out, as in the help overlay: an overlay that does
    // not say how to close it is a trap.
    //
    // Indented two cells to the same left edge the footer below and every other
    // overlay's body already use. Without it this box had two left edges of its
    // own, the lines against the border and the footer clear of it.
    let body = indented(Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    });
    let all = app.log_lines();
    let lines: Vec<Line> = if all.is_empty() {
        vec![Line::from(Span::styled(
            "Nothing recorded yet.",
            Style::default().fg(t.faint),
        ))]
    } else {
        // Windowed from the end: the newest line is the one that is always on
        // screen, and scrolling moves that window back through history.
        let end = all.len().saturating_sub(app.log_offset()).max(1);
        let start = end.saturating_sub(usize::from(body.height));
        all[start..end].iter().map(|l| log_line(l, &t)).collect()
    };
    f.render_widget(Paragraph::new(lines), body);

    if inner.height > 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  j k scroll · g G oldest / newest · M, Esc or q to close",
                Style::default().fg(t.faint),
            ))),
            Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }
}

/// The chain between priel and the output device.
///
/// The DAC badge on the bottom row says whether the chain is clean, because it
/// reads the device's own live parameters. It cannot say *what* made it that
/// way. This is that answer: every node on the path, in order, with the format
/// each one settled on, and a `⚠` on the node where the track's rate or width is
/// first lost.
///
/// Which node that is - or that no node explains it - is decided in the player
/// crate, by `AudioGraph::attribute`. Nothing here judges anything; it colours
/// what it was handed.
///
/// Modal and scrolled like the log overlay, and for the same reason - a second
/// idiom for the same gesture is its own bug.
fn graph_overlay(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let rows = app.graph_rows();
    let width = area.width.saturating_sub(4).min(76);
    // Two for the border, one for the way out. Sized to the content rather than
    // to the screen: this is a short list and a full-height box around three
    // lines reads as something failing to load.
    let wanted = u16::try_from(rows.len().saturating_add(3)).unwrap_or(u16::MAX);
    let height = wanted.min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Output ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    // One row goes to the way out, as in the other overlays: an overlay that
    // does not say how to close it is a trap.
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let start = app.graph_offset().min(rows.len().saturating_sub(1));
    let end = start
        .saturating_add(usize::from(body.height))
        .min(rows.len());
    let lines: Vec<Line> = rows[start..end]
        .iter()
        .map(|r| graph_line(r, body.width, &t))
        .collect();
    f.render_widget(Paragraph::new(lines), body);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  j k scroll · g G top / bottom · D, Esc or q to close",
            Style::default().fg(t.faint),
        ))),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// One row of the graph overlay: what the node is on the left, what it
/// negotiated on the right.
///
/// The gap between them is measured with `Span::width`, the same unicode-width
/// measurement ratatui draws with, so a description with wide glyphs in it does
/// not push the format column off the edge.
fn graph_line(row: &GraphRow, width: u16, t: &Theme) -> Line<'static> {
    let (label_style, detail_style) = match row.kind {
        GraphRowKind::Node => (
            Style::default().fg(t.text).add_modifier(Modifier::BOLD),
            Style::default().fg(t.accent),
        ),
        // The same role the fidelity badge grades an altered stream with: the
        // badge says the samples were rebuilt and this says which node did it,
        // and two colours for one answer would read as two opinions.
        GraphRowKind::Culprit => (
            Style::default()
                .fg(t.verdict_altered)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(t.verdict_altered),
        ),
        GraphRowKind::Link => (Style::default().fg(t.faint), Style::default().fg(t.faint)),
        GraphRowKind::Note => (Style::default().fg(t.muted), Style::default().fg(t.muted)),
    };
    let label = Span::styled(row.label.clone(), label_style);
    if row.detail.is_empty() {
        return Line::from(label);
    }
    let detail = Span::styled(row.detail.clone(), detail_style);
    // One cell short of the full width so the format column does not sit
    // against the border, and at least one space even when the two do not fit,
    // so they never run together into one unreadable word; the paragraph clips
    // whatever overflows.
    let gap = usize::from(width.saturating_sub(1))
        .saturating_sub(label.width() + detail.width())
        .max(1);
    Line::from(vec![label, Span::raw(" ".repeat(gap)), detail])
}

/// How much of the overlay the identifier column may take.
///
/// Device identifiers run to sixty characters and the description is what makes
/// one recognisable, so neither may crowd the other out entirely.
const DEVICE_NAME_SHARE: u16 = 2;

/// The output device picker.
///
/// Modal like the log overlay and scrolled with the same keys. Two things it
/// must always say: which device is in use, and that a choice made here lasts
/// for this session only - priel reads no configuration file, so `--device` is
/// the only way to make one permanent.
fn device_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme();
    let width = area.width.saturating_sub(4).min(110);
    let height = area.height.saturating_sub(2);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Output device ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    // Rebuilt every frame, exactly as the header's hit boxes are - and cleared
    // before the early return below, so a terminal too short to draw the
    // toggle does not leave the last frame's hit box behind to be clicked.
    app.device_rows.clear();
    app.device_exclusive_rect = Rect::default();
    if inner.height <= 2 {
        return;
    }

    // Two rows go to the footer: what choosing does, and how to leave. An
    // overlay that says neither is a trap.
    let body = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    match app.device_notice() {
        // Indented to where a device row's own two-cell mark starts, and to
        // where the footer starts, rather than against the border.
        Some(notice) => f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                notice,
                Style::default().fg(t.faint),
            ))),
            indented(body),
        ),
        None => device_rows(f, app, body),
    }

    let footer = Style::default().fg(t.faint);
    exclusive_toggle(
        f,
        app,
        Rect {
            y: inner.y + inner.height.saturating_sub(2),
            height: 1,
            ..inner
        },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  j k move · g G ends · Enter choose · x exclusive · click · d, Esc or q to close",
            footer,
        ))),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// The exclusivity toggle, and what choosing anything here is worth.
///
/// Laid out and hit-boxed in the same left-to-right walk the header controls
/// are, and measured with `Span::width` for the same reason: a hit box that
/// drifted from what was painted would take the device on a click meant for
/// something else. Exclusivity is *not* implied by picking a row, so it is its
/// own control rather than a spelling of one of them.
fn exclusive_toggle(f: &mut Frame, app: &mut App, row: Rect) {
    let t = app.theme();
    let on = app.exclusive();
    let mut bar = ControlBar::new(row);
    bar.label("  ", Style::default());
    let control = bar.push(Span::styled(
        if on {
            " x exclusive: on  ".to_string()
        } else {
            " x exclusive: off ".to_string()
        },
        t.toggle(on),
    ));
    bar.label(
        "  this session only — --device and --exclusive make a choice permanent",
        Style::default().fg(t.faint),
    );
    // Do not offer a control the row was too narrow to draw.
    app.device_exclusive_rect = if control.x < row.x.saturating_add(row.width) {
        control
    } else {
        Rect::default()
    };
    f.render_widget(Paragraph::new(Line::from(bar.spans)), row);
}

/// The device rows themselves, windowed onto the selection.
///
/// Each row's hit box is registered in the same walk that draws it, so a click
/// cannot land on a device other than the one under the pointer.
fn device_rows(f: &mut Frame, app: &mut App, body: Rect) {
    let t = app.theme();
    let devices = app.devices().len();
    let h = body.height as usize;
    let selected = app.device_selected();
    if selected < app.device_offset {
        app.device_offset = selected;
    } else if selected >= app.device_offset + h {
        app.device_offset = selected + 1 - h;
    }
    if app.device_offset >= devices {
        app.device_offset = 0;
    }

    let name_width = usize::from(body.width / DEVICE_NAME_SHARE);
    let in_use = app.status.audio_device.clone();
    let mut rows = Vec::new();
    for (i, index) in (app.device_offset..(app.device_offset + h).min(devices)).enumerate() {
        let d = &app.devices()[index];
        let here = d.name == in_use;
        let mark = if here { "* " } else { "  " };
        let text = format!(
            "{mark}{:name_width$}  {}",
            trunc(&d.name, name_width),
            d.description
        );
        let style = if index == selected {
            t.selection()
        } else if here {
            Style::default().fg(t.active)
        } else {
            Style::default()
        };
        let rect = Rect {
            x: body.x,
            y: body.y + u16::try_from(i).unwrap_or(u16::MAX),
            width: body.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(text).style(style), rect);
        rows.push((rect, index));
    }
    app.device_rows = rows;
}

/// How wide the name column is, in cells.
///
/// `gruvbox-light` is the longest name on offer and the descriptions read as a
/// column only if they all start in the same place.
const THEME_NAME_FIELD: usize = 15;

/// The colour theme picker.
///
/// Modal like the output picker and scrolled with the same keys. Two things it
/// must always say, for the same reason that one does: which palette is in use,
/// and that a choice made here lasts for this session only - priel reads no
/// configuration file, so `--theme` is the only way to keep one.
///
/// **Each row previews the palette it offers rather than the one in force.**
/// The three fidelity grades are the reason a palette is a decision and not a
/// preference, so every row carries `✓ ≈ ⚠` in that theme's own three colours,
/// on that theme's own background. Painting them in the current theme would
/// draw one identical row per palette and answer none of the question the
/// picker is for.
fn theme_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme();
    let current = app.theme_name();
    let selected = app.theme_selected();
    let width = area.width.saturating_sub(4).min(72);
    // Two for the border, two for the footer. Sized to the content, as the
    // graph overlay is: a full-height box around a handful of rows reads as a failure
    // to load.
    let wanted = u16::try_from(theme::OFFERED.len().saturating_add(4)).unwrap_or(u16::MAX);
    let height = wanted.min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Colour theme ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    // Rebuilt every frame, exactly as the device picker's are - and cleared
    // before the early return below, so a terminal too short to draw a row does
    // not leave the last frame's hit box behind to be clicked.
    app.theme_rows.clear();
    if inner.height <= 2 {
        return;
    }

    let body = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    theme_rows(f, app, body, current, selected);

    let footer = Style::default().fg(t.faint);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  this session only — --theme makes a choice permanent",
            footer,
        ))),
        Rect {
            y: inner.y + inner.height.saturating_sub(2),
            height: 1,
            ..inner
        },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  j k move · g G ends · Enter choose · click · t, Esc or q to close",
            footer,
        ))),
        Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        },
    );
}

/// The theme rows, each registering its hit box in the walk that draws it.
///
/// No windowing: the list is the built-in palettes and cannot grow past what a
/// usable
/// terminal shows, so a row that does not fit is simply not drawn - and not
/// registered either, since a control that was never painted must not answer to
/// a click.
fn theme_rows(f: &mut Frame, app: &mut App, body: Rect, current: ThemeName, selected: usize) {
    let t = app.theme();
    let mut rows = Vec::with_capacity(theme::OFFERED.len());
    for (i, name) in theme::OFFERED.iter().enumerate() {
        let Ok(offset) = u16::try_from(i) else { break };
        if offset >= body.height {
            break;
        }
        let line = Rect {
            y: body.y.saturating_add(offset),
            height: 1,
            ..body
        };
        let mut bar = ControlBar::new(line);
        let here = *name == current;
        let label = format!(
            "{}{:THEME_NAME_FIELD$}",
            if here { "* " } else { "  " },
            theme::label(*name)
        );
        bar.label(
            label,
            if i == selected {
                t.selection()
            } else if here {
                Style::default().fg(t.active)
            } else {
                Style::default()
            },
        );
        theme_swatch(&mut bar, *name);
        bar.label(
            format!("  {}", theme::note(*name)),
            Style::default().fg(t.faint),
        );
        f.render_widget(Paragraph::new(Line::from(bar.spans)), line);
        rows.push((line, *name));
    }
    app.theme_rows = rows;
}

/// The three grades as the palette on this row would paint them.
///
/// The glyphs are the ones the badge itself uses, so what is previewed here is
/// literally what will be on the bottom row - including the fact that each mark
/// says what it means with no colour at all.
fn theme_swatch(bar: &mut ControlBar, name: ThemeName) {
    let p = Theme::of(name);
    let marks = [
        ("✓", p.verdict_clean),
        ("≈", p.verdict_near),
        ("⚠", p.verdict_altered),
    ];
    for (glyph, colour) in marks {
        bar.label(" ", Style::default());
        bar.label(glyph, Style::default().fg(colour).bg(p.background));
    }
}

/// One log line, coloured by how much it wants to be noticed.
fn log_line<'a>(raw: &'a str, t: &Theme) -> Line<'a> {
    let text = raw.trim_end_matches('\n');
    let colour = if text.contains(" ERROR ") {
        t.error
    } else if text.contains(" WARN ") {
        t.notice
    } else {
        t.muted
    };
    Line::from(Span::styled(text, Style::default().fg(colour)))
}

/// The full reference, and the mouse's way to everything in it.
///
/// Modal, and the hit boxes it publishes replace the ones the header and the
/// bottom row registered for this frame - so a click cannot reach a control
/// underneath it, and every key it prints can be clicked instead of pressed.
fn help_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let left = help_height(HELP_LEFT);
    let right = help_height(HELP_RIGHT);
    let width = area.width.min(84);
    // Two columns need a 14-cell key field plus a description, twice over.
    // Below that, stacking beats clipping every description in half.
    let stacked = width.saturating_sub(2) < 76;
    let rows = if stacked {
        u16::try_from(left + right + 1).unwrap_or(u16::MAX)
    } else {
        u16::try_from(left.max(right)).unwrap_or(u16::MAX)
    };

    // Fit the terminal rather than assume it: a short window clips the overlay
    // otherwise, and the one thing it must always show is how to close it.
    let height = rows.saturating_add(4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .border_style(Style::default().fg(t.accent))
        .title(" Keyboard and mouse ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // The overlay owns the frame's hit boxes while it is up: the header and the
    // bottom row registered theirs before this ran, and a click reaching one of
    // them through a modal overlay is exactly the bug modality prevents.
    app.hits.clear();
    let body = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    // Clamped here because only the renderer knows how tall the box came out.
    // `G` sets the scroll to its maximum and lets this decide what that is,
    // which is the same trick `device_offset` uses.
    let shown = usize::from(body.height);
    let content = if stacked {
        left + right + 1
    } else {
        left.max(right)
    };
    let furthest = content.saturating_sub(shown);
    app.help_scroll = app.help_scroll.min(furthest);
    let skip = app.help_scroll;

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(body);
    let mut found = Vec::new();
    help_column(f, cols[0], HELP_LEFT, &mut found, &t, skip);
    help_column(f, cols[1], HELP_RIGHT, &mut found, &t, skip);
    app.hits = found;

    if inner.height > 0 {
        // The way out is always shown, and so is the way down when there is
        // more below: a reference that silently ended would be a reference that
        // silently lost bindings.
        let footer = if furthest > 0 {
            "  j k scroll · g G ends · ?, Esc or q to close"
        } else {
            "  ?, Esc or q to close"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(t.faint),
            ))),
            Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }
}

/// The favorite state of a track, as one glyph.
///
/// Filled means priel has been told the track is in the user's favorites;
/// hollow covers both "it is not" and "priel has not been told", which is the
/// only pair of answers there is - see `App::favorite_ids` for why nothing on
/// the wire offers a third. Suit glyphs rather than the emoji hearts: an emoji
/// font paints those two cells wide while unicode-width calls them one, and
/// every hit box after one would sit a cell off what was painted.
fn heart(favorite: bool) -> &'static str {
    if favorite { "\u{2665}" } else { "\u{2661}" }
}

fn heart_style(favorite: bool, t: &Theme) -> Style {
    if favorite {
        Style::default().fg(t.favorite)
    } else {
        Style::default().fg(t.faint)
    }
}

fn tab_style(active: bool, t: &Theme) -> Style {
    if active {
        t.selection().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.faint)
    }
}

// A transport control is a glyph on a raised background (`Theme::control`)
// rather than bracketed text: brackets read as punctuation to scan past, a
// filled block reads as something to click.
//
// The glyphs are deliberately the *white* triangles (U+25C1/U+25B7) and not the
// media-player codepoints (U+23EE..U+23EF). The latter have emoji presentation,
// so a terminal with an emoji font paints them two cells wide while
// unicode-width calls them one - and every control to their right would then sit
// one cell away from its own hit box.
/// Tabs on the left, transport controls next to them, then status. Controls live
/// up here because this is the row the eye already tracks; the bottom row stays
/// the keyboard reference.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display-only: volume percent is non-negative and rendered whole"
)]
fn header(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let in_playlists = matches!(app.view, View::Playlists | View::PlaylistTracks);
    let in_mixes = matches!(app.view, View::Mixes | View::MixTracks);
    let tabs = [
        ("1 Favorites", View::Favorites, app.view == View::Favorites),
        ("2 Playlists", View::Playlists, in_playlists),
        ("3 Search", View::Search, app.view == View::Search),
        ("4 Mixes", View::Mixes, in_mixes),
    ];
    // Read what the bar needs up front: it borrows `app` mutably to record hits.
    let playing = app.status.playing;
    let shuffle = app.shuffle;
    let volume = app.status.volume as u32;
    let queue = app.queue_indicator();
    let filtering = app.mode == Mode::Filter;
    let filter = app.filter.clone();
    let notice = app.notice.clone();
    let dim = Style::default().fg(t.faint);

    let mut bar = ControlBar::new(area);
    for (label, view, active) in tabs {
        bar.button(format!(" {label} "), Hit::View(view), tab_style(active, &t));
    }

    bar.label(" ", Style::default());
    // Next to the tabs because it acts on the list, not on the playback: it
    // fetches whichever list is on screen again, from its first page.
    bar.button(" ↻ ", Hit::Reload, t.control());

    bar.label("  ", Style::default());
    bar.button(" |◁ ", Hit::Prev, t.control());
    // A control shows the action it performs, not the state it is in.
    bar.button(
        if playing { " ‖ " } else { " ▷ " },
        Hit::PlayPause,
        t.control(),
    );
    bar.button(" ▷| ", Hit::Next, t.control());
    bar.label(" ", Style::default());
    bar.button(" ⇄ ", Hit::Shuffle, t.toggle(shuffle));
    bar.label(" ", Style::default());
    bar.button(" - ", Hit::VolDown, t.control());
    // Unity gain is the desirable state, so say so rather than leaving the
    // listener to infer it from a number.
    // Graded, not decorated: unity is a level nothing was multiplied by and
    // anything else is a level that changed, which is the same finding the
    // verdict badge reports, so it wears the same two roles.
    let (vol_text, vol_style) = if volume == 100 {
        (" 100% ".to_string(), Style::default().fg(t.verdict_clean))
    } else {
        (
            format!(" {volume}% "),
            Style::default()
                .fg(t.verdict_near)
                .add_modifier(Modifier::BOLD),
        )
    };
    bar.button(vol_text, Hit::VolUnity, vol_style);
    bar.button(" + ", Hit::VolUp, t.control());
    bar.label(" ", Style::default());
    // Last in the output cluster, because that is what it belongs to: it opens
    // the picker that decides where these samples go. A bullseye rather than a
    // speaker: every speaker codepoint is emoji, and an emoji font would paint
    // it two cells wide while unicode-width calls it one, moving every hit box
    // after it one cell off what was painted.
    bar.button(" ◎ ", Hit::Devices, t.control());
    // Beside the output control because both are session settings rather than
    // playback. A half-filled circle rather than any of the palette or paint
    // codepoints: those all have emoji presentation, and an emoji font paints
    // them two cells wide while unicode-width calls them one.
    bar.button(" ◐ ", Hit::Themes, t.control());
    bar.label("  ", Style::default());

    if let Some(q) = queue {
        bar.label(format!("queue {q}  "), Style::default().fg(t.queue));
    }
    if filtering {
        bar.label(format!("/{filter}"), Style::default().fg(t.notice));
    } else if let Some(n) = notice {
        bar.label(n, dim);
    }
    app.hits.extend(bar.hits);
    f.render_widget(Paragraph::new(Line::from(bar.spans)), area);
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "row index is bounded by the rect height, itself a u16"
)]
fn list(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    let vis = app.visible();
    let title = list_title(app, vis.len(), area.width as usize);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(t.surface())
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.list_inner = inner;

    let h = inner.height as usize;
    if h == 0 {
        return;
    }

    if app.selected < app.list_offset {
        app.list_offset = app.selected;
    } else if app.selected >= app.list_offset + h {
        app.list_offset = app.selected + 1 - h;
    }
    if app.list_offset >= vis.len() {
        app.list_offset = 0;
    }

    for (i, vi) in (app.list_offset..(app.list_offset + h).min(vis.len())).enumerate() {
        let y = inner.y + i as u16;
        let selected = vi == app.selected;
        let (text, is_now) = row_text(app, &vis, vi, inner.width as usize);
        let style = if selected {
            t.selection()
        } else if is_now {
            Style::default().fg(t.active)
        } else {
            Style::default()
        };
        f.render_widget(
            Paragraph::new(text).style(style),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// The line the list box wears: which list this is, and how much of it is here.
///
/// It used to carry per-view key hints as well, and they were about eighty-five
/// cells in a seventy-eight cell box, so an eighty-column terminal - the
/// commonest width there is - deleted a binding mid-word. That is the same
/// silent clip `push_hints` was hardened against. They were also five different
/// grammars for what the bottom row and the `?` reference already say, and since
/// the reference became a complete clickable menu, nothing became unreachable
/// when they went.
///
/// What is left is the one thing only this line can say: where you are, the
/// trail if you are a level down, and the figure the row below cannot repeat.
/// `width` is the whole box, so the name is cut to fit rather than the count
/// being clipped off the end of it.
fn list_title(app: &App, count: usize, width: usize) -> String {
    let (place, typing) = title_place(app);
    let tail = title_count(app, count);
    // The two corners the border draws, then one space either side of the text.
    let inside = width.saturating_sub(2);
    let room = inside.saturating_sub(2 + cells(&tail));
    if room == 0 {
        // No room for both. The count is the half that would be clipped, so on
        // a box this narrow the name goes and the line stays inside its border.
        return trunc(&place, inside);
    }
    // A query is cut from the front: the caret and the letters just typed are
    // what a cut from the back would take, and they are the whole point of it.
    let place = if typing {
        trunc_start(&place, room)
    } else {
        trunc(&place, room)
    };
    format!(" {place}{tail} ")
}

/// Where you are, and whether the user is typing into it.
fn title_place(app: &App) -> (String, bool) {
    match app.view {
        View::Favorites => ("Favorites".to_string(), false),
        View::Playlists => ("Playlists".to_string(), false),
        View::PlaylistTracks => {
            let name = app.open_playlist.as_ref().map_or("", |(_, t)| t.as_str());
            (format!("Playlists › {name}"), false)
        }
        View::Mixes => ("Mixes".to_string(), false),
        View::MixTracks => {
            let name = app.open_mix.as_ref().map_or("", |(_, t)| t.as_str());
            (format!("Mixes › {name}"), false)
        }
        View::Search if app.mode == Mode::Search => {
            (format!("Search › {}▏", app.search_query), true)
        }
        View::Search if app.search_query.is_empty() => ("Search".to_string(), false),
        View::Search => (format!("Search › {}", app.search_query), false),
    }
}

/// How much of the list is here, and how much of it there is.
///
/// The second figure only while the two differ: a list that has paged in its
/// first hundred must not read as the whole library, or there is no reason to
/// keep scrolling, and once it has all arrived the total only repeats the count
/// beside it.
fn title_count(app: &App, count: usize) -> String {
    if app.view == View::Search && (app.mode == Mode::Search || app.search_query.is_empty()) {
        return String::new();
    }
    let of_total = app
        .rows_available()
        .map_or(String::new(), |total| format!(" of {total}"));
    match app.view {
        // Neither noun adds anything the word before it has not just said.
        View::Playlists | View::Mixes => format!(" — {count}{of_total}"),
        View::Search => format!(" — {count}{of_total} results"),
        _ => format!(" — {count}{of_total} tracks"),
    }
}

/// Which columns a track row can afford at `width` cells, and how wide each is.
///
/// Zero means the column is not drawn at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrackColumns {
    title: usize,
    artist: usize,
    album: usize,
    quality: usize,
}

/// The two cells that separate one column from the next.
const GAP: usize = 2;
/// The now-playing mark plus the favourite heart plus a space.
const LEAD: usize = 4;
/// `LOSSLESS`, the longest tier `short_quality` produces.
const QUALITY_CELLS: usize = 8;
/// `999:59`, longer than any track the service carries.
const DURATION_CELLS: usize = 6;
/// Under this a title says nothing an eye can catch, so a column goes instead.
const TITLE_MIN: usize = 16;
/// The audit's figure: below about twelve cells an artist name is an ellipsis.
const ARTIST_MIN: usize = 12;
/// Wider than the artist's floor deliberately. The album is the column that is
/// there because there is room, so it has to earn more than a bare minimum.
const ALBUM_MIN: usize = 18;

/// What a track row shows, and the order it gives things up as the box narrows.
///
/// The row used to be a fixed 72-cell block whatever the terminal was: at 200
/// columns 116 cells of it were blank, and below 74 columns the quality and the
/// duration were clipped away with nothing to say they had ever been there. So
/// it is a budget now, and the order it spends it in is written down rather
/// than being whatever the format string happened to do.
///
/// **Kept at every width**: the title, and the duration pinned to the right
/// edge. `number-tabular`: a column of times is only scannable when the digits
/// line up, and it is the figure a listener actually compares between rows.
///
/// **Dropped in this order as the width falls**: the album first, then the
/// artist, then the quality tier.
///
/// - The **album** is the one that is only there because the width is otherwise
///   wasted, so it is the first thing the width stops paying for.
/// - The **tier** outlives the artist because grading the fidelity is what this
///   client is for (ADR-0002), and it costs eight fixed cells where an artist
///   column worth reading costs a dozen that grow with the box.
///
/// A column is dropped rather than shaved: below its floor it would be an
/// ellipsis with a letter in front of it, and four such columns say less than
/// two full ones.
///
/// **What is deliberately not a column.** `explicit` and `version` are false or
/// empty on the great majority of rows, so either would be a column of blanks
/// paid for by every row that has nothing to put in it - and `version` belongs
/// against the title rather than beside it. `isrc` and `copyright` are an
/// identifier and a rights line: neither is read while scanning for something to
/// play. `streamable` is true on nearly every row, so it belongs where a play
/// fails, not on every row that will not. Sample rate and bit depth are exact
/// and per-track and would be the best columns here, but they are only known
/// after `resolve_stream`, which happens for the track being played and no other
/// - a column that could be filled in for one row in a listing is not a column.
fn track_columns(width: usize) -> TrackColumns {
    // Title, artist, album, tier, duration.
    let flex = width.saturating_sub(LEAD + 4 * GAP + QUALITY_CELLS + DURATION_CELLS);
    let side = flex * 3 / 11;
    if flex.saturating_sub(2 * side) >= TITLE_MIN && side >= ARTIST_MIN && side >= ALBUM_MIN {
        return TrackColumns {
            title: flex - 2 * side,
            artist: side,
            album: side,
            quality: QUALITY_CELLS,
        };
    }
    // Title, artist, tier, duration.
    let flex = width.saturating_sub(LEAD + 3 * GAP + QUALITY_CELLS + DURATION_CELLS);
    let artist = flex * 2 / 5;
    if flex.saturating_sub(artist) >= TITLE_MIN && artist >= ARTIST_MIN {
        return TrackColumns {
            title: flex - artist,
            artist,
            album: 0,
            quality: QUALITY_CELLS,
        };
    }
    // Title, tier, duration.
    let flex = width.saturating_sub(LEAD + 2 * GAP + QUALITY_CELLS + DURATION_CELLS);
    if flex >= TITLE_MIN {
        return TrackColumns {
            title: flex,
            artist: 0,
            album: 0,
            quality: QUALITY_CELLS,
        };
    }
    // Title and duration. The floor: neither is ever given up.
    TrackColumns {
        title: width.saturating_sub(LEAD + GAP + DURATION_CELLS),
        artist: 0,
        album: 0,
        quality: 0,
    }
}

/// Returns (rendered row text, `is_now_playing`).
///
/// `visible` is passed in rather than recomputed: this runs once per rendered
/// row, and rebuilding the index list here made rendering O(rows x tracks).
fn row_text(app: &App, visible: &[usize], vi: usize, width: usize) -> (String, bool) {
    let idx = visible[vi];
    if app.view == View::Playlists {
        if let Some(p) = app.playlists.get(idx) {
            return (
                format!(
                    "  {} {:>4} tracks   {}",
                    field(&p.title, 44),
                    p.num_tracks,
                    fmt_hms(p.duration_secs)
                ),
                false,
            );
        }
        return (String::new(), false);
    }
    if app.view == View::Mixes {
        // No track count and no running time to put in those columns: the wire
        // carries neither for a mix. What it does carry is the subtitle, which
        // says what the mix was built from - so the row spends the width on
        // that instead of on two figures that would have to be invented.
        if let Some(m) = app.mixes.get(idx) {
            return (
                format!("  {} {}", field(&m.title, 44), trunc(&m.subtitle, 30)),
                false,
            );
        }
        return (String::new(), false);
    }
    let tracks = app.current_tracks();
    if let Some(t) = tracks.get(idx) {
        let is_now = app.now_playing.as_ref().is_some_and(|n| n.id == t.id);
        let mark = if is_now { "♪ " } else { "  " };
        // The heart is text here rather than a control: a row is selected by
        // clicking it, and a second clickable target inside the same row would
        // make a click mean two different things a cell apart. The keyboard row
        // carries the control, and it acts on whatever the click selected.
        let kept = heart(app.is_favorite(t.id));
        let c = track_columns(width);
        let mut row = String::with_capacity(width * 4);
        row.push_str(mark);
        row.push_str(kept);
        row.push(' ');
        row.push_str(&field(&t.title, c.title));
        for (text, cells) in [
            (&t.artist, c.artist),
            (&t.album, c.album),
            // The same spelling the badge beside the playing track uses. The raw
            // wire token does not fit this column, so a row that printed it
            // named the track's quality one way while the row above the progress
            // bar named it another.
            (&short_quality(&t.quality), c.quality),
        ] {
            if cells > 0 {
                push_gap(&mut row);
                row.push_str(&field(text, cells));
            }
        }
        push_gap(&mut row);
        let secs = fmt_dur(t.duration_secs);
        row.push_str(&" ".repeat(DURATION_CELLS.saturating_sub(cells(&secs))));
        row.push_str(&secs);
        (row, is_now)
    } else {
        (String::new(), false)
    }
}

fn push_gap(row: &mut String) {
    for _ in 0..GAP {
        row.push(' ');
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display-only: seconds and volume percent are non-negative and rendered whole"
)]
fn now_playing(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme();
    // Split rather than offset from `area.y`: on a terminal too short for three
    // rows, hand-computed offsets address cells outside the buffer and ratatui
    // panics. A layout clamps to what exists, yielding empty rects instead.
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let (l0, l1, l2) = (rows[0], rows[1], rows[2]);
    app.progress_rect = l1;

    let s = &app.status;
    let title = match &app.now_playing {
        Some(t) => format!("{} — {}", t.artist, t.title),
        None => "Nothing playing".into(),
    };
    let state = if s.paused {
        "⏸"
    } else if s.playing {
        "▶"
    } else {
        "·"
    };
    // Built through a `ControlBar` rather than as raw spans so the heart's hit
    // box comes out of the same left-to-right walk that paints it. The state and
    // the button are one glyph on purpose: a separate control elsewhere would be
    // a second thing to keep in step with what this row already says.
    let kept = app
        .now_playing
        .as_ref()
        .is_some_and(|t| app.is_favorite(t.id));
    let mut top = ControlBar::new(l0);
    top.label(format!(" {state} "), Style::default().fg(t.accent));
    if app.now_playing.is_some() {
        top.button(
            format!("{} ", heart(kept)),
            Hit::FavoriteNowPlaying,
            heart_style(kept, &t),
        );
    }
    top.label(title, Style::default());
    top.label(source_badge(app), Style::default().fg(t.faint));
    app.hits.extend(top.hits);
    f.render_widget(Paragraph::new(Line::from(top.spans)), l0);

    let ratio = if s.duration > 0.0 {
        (s.position / s.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(t.on(t.accent))
            .ratio(ratio)
            .label(format!(
                "{} / {}",
                fmt_dur(s.position as u32),
                fmt_dur(s.duration as u32)
            )),
        l1,
    );

    // DAC badge, the shared activity slot (resolving / buffering / buffered),
    // then the keyboard reference. The clickable controls live in the header.
    let (act_text, act_color) = activity(app);
    let (verdict_text, verdict_color) = verdict_badge(app);
    let mut bar = ControlBar::new(l2);
    bar.label(dac_badge(&app.status), Style::default().fg(t.active));
    // The verdict says *whether*; clicking it says *why*, through the same
    // method `[D]` runs. Registered in the walk that lays it out, like every
    // other control, and only when there is a word to click on.
    if !verdict_text.is_empty() {
        bar.label("  ", Style::default());
        bar.button(
            verdict_text,
            Hit::Graph,
            Style::default()
                .fg(verdict_color)
                .add_modifier(Modifier::BOLD),
        );
    }
    bar.label(act_text, Style::default().fg(act_color));
    bar.label("  ", Style::default());
    push_hints(&mut bar, &t);
    app.hits.extend(bar.hits);
    f.render_widget(Paragraph::new(Line::from(bar.spans)), l2);
}

/// One entry in the bottom keyboard reference.
///
/// Each key is individually clickable and maps to the same action its key press
/// runs, so the reference doubles as the mouse control strip - there is no
/// separate button to keep in sync with it.
struct Hint {
    keys: &'static [(&'static str, Hit)],
    label: &'static str,
}

/// Shown whenever there is room, in descending order of everyday use.
const HINTS: &[Hint] = &[
    Hint {
        keys: &[("space", Hit::PlayPause)],
        label: "play",
    },
    Hint {
        keys: &[("h", Hit::SeekBack), ("l", Hit::SeekFwd)],
        label: "seek",
    },
    Hint {
        keys: &[("H", Hit::Prev), ("L", Hit::Next)],
        label: "skip",
    },
    Hint {
        keys: &[("s", Hit::Shuffle)],
        label: "shuffle",
    },
    Hint {
        keys: &[("-", Hit::VolDown), ("+", Hit::VolUp)],
        label: "vol",
    },
    Hint {
        keys: &[("/", Hit::Filter)],
        label: "filter",
    },
    // The control for the *selected* row. The playing track has its own heart,
    // up beside the title where its state already is.
    Hint {
        keys: &[("f", Hit::FavoriteSelected)],
        label: "fav",
    },
    Hint {
        keys: &[("j", Hit::MoveDown), ("k", Hit::MoveUp)],
        label: "move",
    },
    Hint {
        keys: &[("Tab", Hit::CycleView)],
        label: "view",
    },
    Hint {
        keys: &[("g", Hit::Top), ("G", Hit::Bottom)],
        label: "ends",
    },
    // Last because it is the least everyday of these, which is also what makes
    // it the first to be dropped when the row runs out of width. The full
    // reference under `?` is where it is always findable.
    Hint {
        keys: &[("D", Hit::Graph)],
        label: "graph",
    },
];

/// Never dropped, however narrow the row: `?` is how everything else is found,
/// and `q` is the one binding a user cannot guess their way out of without.
const HINTS_ESSENTIAL: &[Hint] = &[
    Hint {
        keys: &[("?", Hit::Help)],
        label: "keys",
    },
    Hint {
        keys: &[("q", Hit::Quit)],
        label: "quit",
    },
];

/// Rendered width of a hint: `[a/b] label` plus its trailing gap.
fn hint_width(h: &Hint) -> u16 {
    let keys: usize = h.keys.iter().map(|(k, _)| k.chars().count()).sum();
    let separators = h.keys.len().saturating_sub(1);
    let width = 1 + keys + separators + 2 + h.label.chars().count() + 2;
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn push_hint(bar: &mut ControlBar, h: &Hint, t: &Theme) {
    let dim = Style::default().fg(t.faint);
    bar.label("[", dim);
    for (i, (key, hit)) in h.keys.iter().enumerate() {
        if i > 0 {
            bar.label("/", dim);
        }
        bar.button(*key, *hit, Style::default().fg(t.accent));
    }
    bar.label(format!("] {}  ", h.label), dim);
}

/// Fill the row with hints, reserving room for the essential ones so they are
/// never the ones clipped off the right edge.
fn push_hints(bar: &mut ControlBar, t: &Theme) {
    let reserved: u16 = HINTS_ESSENTIAL.iter().map(hint_width).sum();
    for h in HINTS {
        if bar.remaining() < hint_width(h).saturating_add(reserved) {
            break;
        }
        push_hint(bar, h, t);
    }
    for h in HINTS_ESSENTIAL {
        if bar.remaining() >= hint_width(h) {
            push_hint(bar, h, t);
        }
    }
}

/// Lays spans out left to right on one row, registering a hit box for anything
/// clickable. Keeping the layout and the hit boxes in the same walk is what
/// stops them drifting apart.
///
/// Widths come from `Span::width`, the same unicode-width measurement ratatui
/// uses to draw, so a multi-cell glyph gets a hit box that matches what was
/// actually painted. Counting `char`s here would misplace every control to the
/// right of the first wide glyph.
struct ControlBar {
    spans: Vec<Span<'static>>,
    hits: Vec<(Rect, Hit)>,
    x: u16,
    y: u16,
    end: u16,
}

impl ControlBar {
    fn new(area: Rect) -> Self {
        Self {
            spans: Vec::new(),
            hits: Vec::new(),
            x: area.x,
            y: area.y,
            end: area.x.saturating_add(area.width),
        }
    }

    fn push(&mut self, span: Span<'static>) -> Rect {
        let width = u16::try_from(span.width()).unwrap_or(u16::MAX);
        let r = Rect {
            x: self.x,
            y: self.y,
            width,
            height: 1,
        };
        self.x = self.x.saturating_add(width);
        self.spans.push(span);
        r
    }

    /// Cells left on the row. Used to choose how much detail still fits.
    fn remaining(&self) -> u16 {
        self.end.saturating_sub(self.x)
    }

    fn label(&mut self, text: impl Into<String>, style: Style) {
        self.push(Span::styled(text.into(), style));
    }

    fn button(&mut self, text: impl Into<String>, hit: Hit, style: Style) {
        let r = self.push(Span::styled(text.into(), style));
        // Do not register a control the row was too narrow to draw.
        if r.x < self.end {
            self.hits.push((r, hit));
        }
    }
}

/// The mutually-exclusive activity slot: resolving → buffering → buffered-ahead.
/// Padded to a fixed width so nothing else on the row shifts as it changes.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display-only: buffered seconds are non-negative and rendered whole"
)]
fn activity(app: &App) -> (String, Color) {
    const W: usize = 16; // widest content ("⤓ 214s buffered" ≈ 15)
    let t = app.theme();
    let (text, color) = if app.is_resolving() {
        (format!("{} resolving…", app.spinner()), t.notice)
    } else if app.is_buffering() {
        (format!("{} buffering…", app.spinner()), t.notice)
    } else if app.status.loaded && app.status.cache_secs >= 1.0 {
        let c = if app.status.cache_secs >= 10.0 {
            t.active
        } else {
            t.notice
        };
        (format!("⤓ {}s buffered", app.status.cache_secs as u32), c)
    } else {
        (String::new(), t.faint)
    };
    (format!("  {text:<W$}"), color)
}

fn source_badge(app: &App) -> String {
    if app.now_playing.is_none() {
        return String::new();
    }
    let m = &app.now_meta;
    let mut parts: Vec<String> = Vec::new();
    if m.bit_depth > 0 {
        parts.push(format!("{}-bit", m.bit_depth));
    }
    if m.sample_rate > 0 {
        parts.push(fmt_khz(m.sample_rate));
    }
    if !m.codec.is_empty() {
        parts.push(m.codec.to_uppercase());
    }
    let q = short_quality(&m.quality);
    if !q.is_empty() {
        parts.push(q);
    }
    if app.status.bitrate > 0 {
        parts.push(format!("~{} kbps", app.status.bitrate / 1000));
    }
    if parts.is_empty() {
        String::new()
    } else {
        // Two cells either side of the separator that joins the title to the
        // badge, against one either side of the separators inside it: enough of
        // a gap to read as a group, and even, which three-then-two was not.
        format!("  ·  {}", parts.join(" · "))
    }
}

/// The verdict: one word for what is reaching the device, and nothing else.
///
/// **Four words, no numbers, no stage named and no remedy.** The numbers are
/// already on screen twice - the source badge carries the track and the device
/// readout carries the output - so repeating them buys width and says nothing.
/// What cannot be worked out at a glance is *which kind* of alteration, and that
/// is what the word gives. Everything behind it is in the report under `[D]`,
/// which this badge is also the button for.
///
/// The inline "0 for unity" that used to sit here is gone for a stronger reason
/// than width: that remedy only clears priel's own volume, and now that the sink
/// is a possible cause it would be actively wrong in exactly the cases this
/// badge exists to catch.
///
/// `✓?` marks a tick reached without reading every stage. A glyph rather than a
/// dimmed colour, so it survives a light theme, a dark theme, a monochrome
/// terminal and the red/green deficiency the grades already lean on.
fn verdict_badge(app: &App) -> (String, Color) {
    let verdict = app.verdict();
    (
        verdict_words(verdict),
        verdict_colour(verdict.fidelity, &app.theme()),
    )
}

/// The verdict in words, shared by the row and the report so the two cannot
/// come to different conclusions about one moment.
pub(crate) fn verdict_words(verdict: Verdict) -> String {
    let tick = if verdict.needs_qualifying() {
        "✓?"
    } else {
        "✓"
    };
    match verdict.fidelity {
        Fidelity::Unknown => String::new(),
        Fidelity::BitPerfect => format!("{tick} bit-perfect"),
        // Every level change reads the same, whichever stage made it. Naming
        // the stage here would need the width the row does not have, and the
        // report's volume section is where the three of them are laid out side
        // by side.
        Fidelity::NearBitPerfect(_)
        | Fidelity::Altered(
            Alteration::VolumeScaled
            | Alteration::ServerVolumeScaled
            | Alteration::SinkVolumeScaled,
        ) => "≈ near bit-perfect".to_string(),
        Fidelity::Altered(Alteration::Resampled) => "⚠ resampled".to_string(),
        Fidelity::Altered(Alteration::Truncated) => "⚠ truncated".to_string(),
    }
}

/// The colour that goes with a grade. Never the only carrier of a meaning: the
/// glyph in front of each word says the same thing on a monochrome terminal.
fn verdict_colour(fidelity: Fidelity, t: &Theme) -> Color {
    match fidelity {
        Fidelity::Unknown => t.verdict_unknown,
        Fidelity::BitPerfect => t.verdict_clean,
        Fidelity::Altered(Alteration::Resampled | Alteration::Truncated) => t.verdict_altered,
        // Every level change, whichever stage made it and whichever grade it
        // arrived under. A rebuilt sample stream is the only thing that gets
        // red here.
        Fidelity::NearBitPerfect(_) | Fidelity::Altered(_) => t.verdict_near,
    }
}

/// One level as the report writes it: the unit it was set in, and - only where
/// something was actually multiplied in software - what it cost.
///
/// The decibel figure is the one that compares between stages, and the bits are
/// the answer to "how much did I lose", by the one-bit-per-6-dB rule the README
/// records. Below one bit the count is dropped rather than printed as zero,
/// which reads as a finding when it is the absence of one.
fn level_words(gain: f64) -> String {
    if (gain - 1.0).abs() <= f64::EPSILON {
        return "100%".to_string();
    }
    let mut out = format!("{}  {:.0} dB", fmt_pct(gain), SinkStage::db(gain));
    let bits = SinkStage::bits_lost(gain);
    if bits > 0 {
        let _ = write!(out, "  ~{bits} bits");
    }
    out
}

/// A linear gain as the percentage a mixer shows, with a decimal only where it
/// carries something.
fn fmt_pct(gain: f64) -> String {
    let pct = gain * 100.0;
    if (pct - pct.round()).abs() < 0.05 {
        format!("{pct:.0}%")
    } else {
        format!("{pct:.1}%")
    }
}

/// priel's own volume, which is a percentage already.
pub(crate) fn own_volume_words(volume_pct: f64) -> String {
    level_words(volume_pct / 100.0)
}

/// The sound server's level for priel's own stream.
pub(crate) fn stream_volume_words(s: &priel_player::PlaybackStatus) -> String {
    match s.stream_volume() {
        StreamVolume::Absent => "none in this chain".to_string(),
        StreamVolume::Unread => "unknown".to_string(),
        StreamVolume::Read(pct) => level_words(pct / 100.0),
    }
}

/// The sound server's level on the sink everything is mixed into.
///
/// The figure shown is the control - the number the listener set and a mixer
/// displays - and a loss is only ever quoted where the server was found to be
/// applying it. Quoting 31 dB of loss against a control the server is not
/// applying would invent a fault; showing nothing at all would hide a control
/// that was plainly set.
pub(crate) fn sink_volume_words(sink: &SinkVolume) -> String {
    match sink.stage() {
        SinkStage::Absent => "none in this chain".to_string(),
        SinkStage::Unread => "unknown".to_string(),
        SinkStage::Unity => "100%".to_string(),
        SinkStage::Silenced => "muted".to_string(),
        SinkStage::InSoftware { gain } => level_words(gain),
        SinkStage::Elsewhere { set } => fmt_pct(set),
    }
}

/// Where the sink's level is being applied, where that is worth a line.
///
/// The whole finding in one sentence. `channelVolumes` is the control and
/// `softVolumes` is what the server multiplies by, and on a real machine they
/// disagree - a sink at 2.7% whose software stage sits at unity, on a card with
/// no volume control in ALSA at all. Saying which of the two was read is the
/// difference between a measurement and a guess.
pub(crate) fn sink_volume_note(sink: &SinkVolume) -> Option<String> {
    match sink.stage() {
        SinkStage::InSoftware { .. } => Some("in software, by the server".to_string()),
        SinkStage::Elsewhere { .. } => {
            Some("not applied by the server; where is not in the graph".to_string())
        }
        SinkStage::Silenced => Some("the server is passing silence".to_string()),
        SinkStage::Absent | SinkStage::Unread | SinkStage::Unity => None,
    }
}

/// What priel is handing to the audio server.
///
/// Labelled `OUT`, not `DAC`, on purpose: mpv's `audio-out-params` describe the
/// format written to the audio API, which is what the server accepted from us -
/// not the rate the hardware is clocked at. `PipeWire` can accept 44.1 kHz and
/// resample it into a 48 kHz graph without mpv ever seeing it. Showing the real
/// device rate means reading the graph, which is a separate piece of work.
/// Is any output actually open?
///
/// Asked by both the device readout and the access line, so the report cannot
/// say `OUT —` and name an access mode in the same breath. The player owns the
/// question, so both of them read the same answer as the volume stages do.
fn has_output(s: &priel_player::PlaybackStatus) -> bool {
    s.output_is_open()
}

fn dac_badge(s: &priel_player::PlaybackStatus) -> String {
    format!(" {}", device_readout(s))
}

/// What is being played into, in the same words on the row and in the report.
pub(crate) fn device_readout(s: &priel_player::PlaybackStatus) -> String {
    if !has_output(s) {
        return "OUT —".into();
    }
    let (rate_hz, format) = s.effective_output();
    // `DAC` only when the numbers came from the ALSA device itself. Otherwise
    // this is what the audio server accepted from us, which it may yet resample.
    let label = if s.verdict_is_from_hardware() {
        "DAC"
    } else {
        "OUT"
    };
    let fmt = if format.is_empty() {
        "?".to_string()
    } else {
        format.to_uppercase()
    };
    let rate = if rate_hz > 0 {
        fmt_khz(rate_hz)
    } else {
        "?".to_string()
    };
    // No leading space, exactly as the `OUT —` case above returns none: the
    // callers add their own. One branch that padded itself and one that did not
    // moved the whole bottom row - and every hit box on it - one cell right the
    // moment an output opened.
    format!("{label} {fmt} · {rate}")
}

/// How the output device is being held.
///
/// A separate function rather than a condition inside [`device_readout`],
/// because the judgement is the player's and this only names it.
///
/// **Every state is named, including the ordinary shared one.** Leaving the
/// default silent made it a thing you had to know the absence of: a listener
/// who saw no word for it could not tell a shared device from a version that
/// did not report access at all.
///
/// It reads in the report rather than on the bottom row, because it is a
/// session-long setting rather than something that changes per track - and the
/// row it used to sit on had no width left to spare.
pub(crate) fn access_words(s: &priel_player::PlaybackStatus) -> String {
    // `OUT —` already says there is no output; naming an access mode beside it
    // would claim priel holds a device it does not hold. Both lines ask
    // `has_output`, so they cannot contradict each other.
    if !has_output(s) {
        return "nothing open".to_string();
    }
    match s.access {
        OutputAccess::Shared => "shared".to_string(),
        OutputAccess::Exclusive => "exclusive".to_string(),
        OutputAccess::Refused => "shared - exclusive was refused".to_string(),
    }
}

fn short_quality(q: &str) -> String {
    let u = q.to_uppercase();
    if u.contains("HI_RES") || u.contains("HIRES") {
        "HI-RES".into()
    } else if u.contains("LOSSLESS") {
        "LOSSLESS".into()
    } else if u.is_empty() {
        String::new()
    } else {
        u
    }
}

/// A column of exactly `n` cells: cut to fit, then padded out to the full width.
///
/// `format!("{:<n$}")` cannot do this. It pads to a *character* count, so a CJK
/// title of sixteen characters was thirty-two cells wide and then had sixteen
/// spaces added after it - forty-eight cells in a thirty-two cell column, with
/// every column to its right pushed off the grid.
fn field(s: &str, n: usize) -> String {
    let mut out = trunc(s, n);
    for _ in cells(&out)..n {
        out.push(' ');
    }
    out
}

/// Cut `s` down to at most `n` display cells, marking the cut with an ellipsis.
///
/// Cells, not characters: a wide glyph paints two of them and a combining mark
/// paints none, so a character count is not a width. Measured with
/// `unicode-width`, which is the crate ratatui measures `Span::width` with, so
/// what this reserves and what the renderer paints cannot disagree.
///
/// A wide glyph straddling the last cell is dropped rather than half-drawn, so
/// the result is occasionally one cell short of `n`. Never longer.
fn trunc(s: &str, n: usize) -> String {
    // A field with no room holds nothing. Returning the ellipsis alone made the
    // result one cell wider than the field it was asked to fit, which on a
    // narrow picker pushed the column beside it along.
    if n == 0 {
        return String::new();
    }
    if cells(s) <= n {
        return s.to_string();
    }
    // One cell is spoken for by the ellipsis that says the rest was cut.
    let budget = n - 1;
    let mut used = 0;
    let mut out = String::with_capacity(s.len().min(n * 4));
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Cut `s` down to its last `n` display cells, marking the cut at the front.
///
/// The mirror of `trunc`, for a field whose end is the part that matters: a
/// query being typed, where a cut from the back takes the caret and the letters
/// that were just entered.
fn trunc_start(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    if cells(s) <= n {
        return s.to_string();
    }
    // One cell is spoken for by the ellipsis that says the rest was cut.
    let budget = n - 1;
    let mut used = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        start = i;
    }
    format!("…{}", &s[start..])
}

/// How many cells a string paints, the way ratatui measures it to draw it.
fn cells(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Sample rates the way the people who care about them write them.
///
/// 44100 is "44.1 kHz", not "44 kHz": the 44.1 and 48 kHz families are the whole
/// distinction a bit-perfect chain turns on, and truncating the decimal away
/// makes 44.1 and 48 look like neighbours rather than different worlds.
pub(crate) fn fmt_khz(hz: u32) -> String {
    if hz == 0 {
        return "?".to_string();
    }
    format!("{} kHz", khz(hz))
}

/// Several rates the same way, with the unit said once at the end.
///
/// A permitted-rate list runs to ten entries, and repeating " kHz" after each
/// of them pushes the row past the box it is drawn in without adding a thing.
/// The decimal is what matters and it is kept.
pub(crate) fn fmt_khz_list(rates_hz: &[u32]) -> String {
    if rates_hz.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = rates_hz.iter().copied().map(khz).collect();
    format!("{} kHz", joined.join(" / "))
}

/// The number by itself, so the single and the list cannot disagree about it.
fn khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        (hz / 1000).to_string()
    } else {
        format!("{:.1}", f64::from(hz) / 1000.0)
    }
}

fn fmt_dur(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn fmt_hms(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ControlBar, HELP_LEFT, HELP_RIGHT, HINTS, HINTS_ESSENTIAL, hint_width, push_hints, render,
    };
    use crate::app::{App, Click, Hit, Mode, View};
    use crate::theme::Theme;
    use crate::worker::{FromWorker, ToWorker};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use priel_core::{Playlist, Track};
    use priel_player::OutputAccess;
    use priel_player::graph::{
        AudioGraph, ClockRates, DeviceHolder, GraphError, GraphNode, HeldDevice, NodeRole,
        SinkLevels, SinkVolume,
    };
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::mpsc::{Receiver, Sender};

    /// Holds the worker channel ends alongside the app: dropping them would
    /// disconnect the channels mid-test.
    struct Screen {
        app: App,
        to_app: Sender<FromWorker>,
        #[allow(dead_code, reason = "held to keep the worker channels alive")]
        from_app: Receiver<ToWorker>,
    }

    /// A renderable app backed by a silent player and a rigged worker. The
    /// channel ends are held so they do not disconnect mid-test.
    fn screen() -> Screen {
        let (app, to_app, from_app) = App::rigged();
        Screen {
            app,
            to_app,
            from_app,
        }
    }

    fn track(id: u64, title: &str) -> Track {
        Track {
            id,
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration_secs: 245,
            quality: "HI-RES".into(),
            ..Track::default()
        }
    }

    /// Render one frame and return it as plain text lines.
    fn draw(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("backend");
        term.draw(|f| render(f, app)).expect("render");
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn text(app: &mut App, w: u16, h: u16) -> String {
        draw(app, w, h).join("\n")
    }

    fn row(width: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height: 1,
        }
    }

    /// Render a frame and return the cells the control's hit box covers.
    ///
    /// The one assertion every clickable control owes: what was painted and what
    /// is clickable are the same cells. Comparing the two by eye in a frame dump
    /// is exactly the check that has been missed before.
    fn painted(app: &mut App, w: u16, h: u16, wanted: Hit) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("backend");
        term.draw(|f| render(f, app)).expect("render");
        let rect = app
            .hits
            .iter()
            .find(|(_, h)| *h == wanted)
            .map_or_else(|| panic!("{wanted:?} has no hit box"), |(r, _)| *r);
        let buf = term.backend().buffer().clone();
        (rect.x..rect.x.saturating_add(rect.width))
            .map(|x| buf[(x, rect.y)].symbol().to_string())
            .collect()
    }

    /// Press a control the way a user would: on its own hit box.
    fn click_hit(app: &mut App, wanted: Hit) {
        let rect = app
            .hits
            .iter()
            .find(|(_, h)| *h == wanted)
            .map_or_else(|| panic!("{wanted:?} has no hit box"), |(r, _)| *r);
        app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
    }

    #[test]
    fn hit_boxes_tile_the_row_without_gaps_or_overlap() {
        // Goal: the hit box of every control must start exactly where the
        // previous span ended. A gap or overlap here is a click landing on the
        // wrong control, which is invisible in a screenshot.
        let mut bar = ControlBar::new(row(80));
        bar.button(" a ", Hit::Prev, Theme::default().control());
        bar.label("--", Style::default());
        bar.button(" bb ", Hit::Next, Theme::default().control());

        assert_eq!(bar.hits.len(), 2);
        assert_eq!(bar.hits[0].0.x, 0);
        assert_eq!(bar.hits[0].0.width, 3);
        // 3 for the first button + 2 for the label.
        assert_eq!(bar.hits[1].0.x, 5);
        assert_eq!(bar.hits[1].0.width, 4);
    }

    #[test]
    fn wide_glyphs_are_measured_by_render_width_not_char_count() {
        // Goal: the transport glyphs are multi-cell. Counting chars would place
        // every later control one cell left per glyph, so clicks would drift.
        // `Span::width` is what ratatui itself draws with, so they cannot drift.
        let mut bar = ControlBar::new(row(80));
        bar.button(" |◁ ", Hit::Prev, Theme::default().control());
        bar.button(" ▷| ", Hit::Next, Theme::default().control());

        let first = bar.hits[0].0;
        let second = bar.hits[1].0;
        assert!(
            first.width >= 3,
            "a padded glyph occupies at least its two spaces plus one cell"
        );
        assert_eq!(
            second.x,
            first.x + first.width,
            "the second control must start where the first ended"
        );
    }

    #[test]
    fn hint_width_matches_what_is_rendered() {
        // Goal: the fitting logic reserves space using hint_width, so if that
        // disagrees with the spans push_hint emits, the row silently overflows
        // and the essential hints fall off the edge again.
        for h in HINTS.iter().chain(HINTS_ESSENTIAL) {
            let mut bar = ControlBar::new(row(200));
            super::push_hint(&mut bar, h, &Theme::default());
            let rendered: usize = bar.spans.iter().map(ratatui::text::Span::width).sum();
            assert_eq!(
                u16::try_from(rendered).unwrap(),
                hint_width(h),
                "hint {:?} measures differently than it renders",
                h.label
            );
        }
    }

    #[test]
    fn the_escape_hatches_survive_a_narrow_row() {
        // Goal: quit and help must be reachable at any width. They are reserved
        // for, so optional hints get dropped before they do.
        for width in [24u16, 40, 60, 80, 120, 200] {
            let mut bar = ControlBar::new(row(width));
            push_hints(&mut bar, &Theme::default());
            let text: String = bar.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.contains("quit") && text.contains("keys"),
                "width {width} dropped an escape hatch: {text:?}"
            );
            let drawn: usize = bar.spans.iter().map(ratatui::text::Span::width).sum();
            assert!(
                drawn <= width as usize,
                "width {width} overflowed: {text:?}"
            );
        }
    }

    #[test]
    fn every_hint_key_is_clickable() {
        // Goal: the reference doubles as the mouse strip, so each key glyph must
        // register a hit box - that is why there is no separate quit button.
        let mut bar = ControlBar::new(row(200));
        push_hints(&mut bar, &Theme::default());
        let keys: usize = HINTS
            .iter()
            .chain(HINTS_ESSENTIAL)
            .map(|h| h.keys.len())
            .sum();
        assert_eq!(bar.hits.len(), keys);
    }

    #[test]
    fn controls_past_the_right_edge_are_not_clickable() {
        // Goal: on a narrow terminal a control that was never painted must not
        // still swallow clicks at a position it does not occupy.
        let mut bar = ControlBar::new(row(4));
        bar.button(" aaaa ", Hit::Prev, Theme::default().control());
        bar.button(" bbbb ", Hit::Next, Theme::default().control());
        assert_eq!(bar.hits.len(), 1, "only the control inside the row counts");
        assert_eq!(bar.hits[0].1, Hit::Prev);
    }

    // ---- whole-frame rendering ----

    #[test]
    fn the_favorites_view_shows_rows_with_metadata() {
        // Goal: one real frame through the renderer. Track text, quality badge
        // and duration all have to reach the screen.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Blue in Green")];
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("Blue in Green"), "{out}");
        assert!(out.contains("Artist"), "{out}");
        assert!(out.contains("HI-RES"), "{out}");
        assert!(out.contains("4:05"), "245s should render as 4:05: {out}");
        assert!(out.contains("Favorites"), "{out}");
    }

    #[test]
    fn the_playlists_view_shows_counts_and_a_running_time() {
        // Goal: playlists render differently from tracks - count plus a h:mm:ss
        // total rather than artist and quality.
        let mut sc = screen();
        sc.app.playlists = vec![Playlist {
            uuid: "u".into(),
            title: "Evening".into(),
            num_tracks: 12,
            duration_secs: 3725,
        }];
        sc.app.view = View::Playlists;
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("Evening"), "{out}");
        assert!(out.contains("12"), "{out}");
        assert!(out.contains("1:02:05"), "hours format: {out}");
    }

    #[test]
    fn the_mixes_view_shows_what_each_mix_was_built_from() {
        // Goal: a mix row cannot show what a playlist row shows - the wire
        // carries no track count and no duration for one - so it has to spend
        // that width on the subtitle instead. A row that printed a count here
        // would be printing a number nobody sent.
        let mut sc = screen();
        sc.app.mixes = vec![priel_core::Mix {
            id: "0007a".into(),
            title: "My Mix 1".into(),
            subtitle: "Miles Davis, Bill Evans".into(),
        }];
        sc.app.view = View::Mixes;
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("My Mix 1"), "{out}");
        assert!(out.contains("Miles Davis, Bill Evans"), "{out}");
        assert!(out.contains("Mixes"), "the heading names the view: {out}");
        assert!(
            !out.contains("tracks"),
            "there is no count to show, so none is claimed: {out}"
        );
    }

    #[test]
    fn the_mixes_tab_is_painted_where_a_click_on_it_lands() {
        // Goal: the fourth tab is a control like the other three, and the rule
        // is that what was painted and what is clickable are the same cells.
        let mut sc = screen();
        assert_eq!(
            painted(&mut sc.app, 120, 12, Hit::View(View::Mixes)),
            " 4 Mixes ",
        );
        click_hit(&mut sc.app, Hit::View(View::Mixes));
        assert_eq!(sc.app.view, View::Mixes);
    }

    #[test]
    fn the_reference_offers_a_way_back_that_knows_which_list_it_came_from() {
        // Goal: `Esc` is now the way out of two different drill-downs, so the
        // control in the reference cannot name a destination. Clicking it from
        // inside a mix has to land on the mixes, which is exactly what a hit box
        // hard-wired to the playlists could not do.
        let mut sc = screen();
        sc.app.view = View::MixTracks;
        sc.app.mode = Mode::Help;
        assert_eq!(painted(&mut sc.app, 120, 40, Hit::Back), "Esc");
        click_hit(&mut sc.app, Hit::Back);
        assert_eq!(sc.app.view, View::Mixes);
    }

    #[test]
    fn the_favorites_heading_separates_rows_loaded_from_rows_there_are() {
        // Goal: a list that has paged in its first hundred must not read as the
        // whole library, or the user has no reason to keep scrolling. The total
        // stays once everything is loaded: `2 of 2` says the list is complete,
        // where a bare `2 tracks` only says so to a reader who knows that a
        // missing total means finished. The same reason the access badge names
        // the shared case rather than leaving it to be read off an absence.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "One"), track(2, "Two")];
        sc.app.favorites_paging.total = 417;
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("2 of 417 tracks"), "{out}");

        sc.app.favorites_paging.total = 2;
        let out = text(&mut sc.app, 120, 12);
        assert!(
            out.contains("2 of 2 tracks"),
            "a complete list says so rather than leaving it to an absence: {out}"
        );
    }

    #[test]
    fn every_heading_separates_rows_loaded_from_rows_there_are() {
        // Goal: the same reason as above, on the three views that used to stop
        // at their first page. A heading that hides the difference is a list
        // the user has no reason to keep scrolling.
        let mut sc = screen();
        sc.app.view = View::Playlists;
        sc.app.playlists = vec![Playlist {
            uuid: "u".into(),
            title: "Mix".into(),
            num_tracks: 3,
            duration_secs: 60,
        }];
        sc.app.playlists_paging.total = 40;
        assert!(text(&mut sc.app, 120, 12).contains("1 of 40"));

        let mut sc = screen();
        sc.app.view = View::PlaylistTracks;
        sc.app.open_playlist = Some(("u".into(), "Late Night".into()));
        sc.app.playlist_tracks = vec![track(2, "Track")];
        sc.app.playlist_tracks_paging.total = 312;
        assert!(text(&mut sc.app, 120, 12).contains("1 of 312 tracks"));

        let mut sc = screen();
        sc.app.view = View::Search;
        sc.app.search_query = "blue".into();
        sc.app.search_tracks = vec![track(3, "Kind of Blue")];
        sc.app.search_paging.total = 900;
        assert!(text(&mut sc.app, 120, 12).contains("1 of 900 results"));
    }

    #[test]
    fn an_open_playlist_is_titled_by_its_name() {
        // Goal: the drill-down has to say which playlist you are inside.
        let mut sc = screen();
        sc.app.view = View::PlaylistTracks;
        sc.app.open_playlist = Some(("u".into(), "Late Night".into()));
        sc.app.playlist_tracks = vec![track(2, "Track")];
        assert!(text(&mut sc.app, 120, 12).contains("Late Night"));
    }

    #[test]
    fn the_search_view_prompts_before_and_reports_after() {
        // Goal: the three states of the search title - empty, typing, results.
        let mut sc = screen();
        sc.app.view = View::Search;
        assert!(text(&mut sc.app, 120, 12).contains("Search"));

        sc.app.mode = Mode::Search;
        sc.app.search_query = "miles".into();
        assert!(text(&mut sc.app, 120, 12).contains("miles"));

        sc.app.mode = Mode::Normal;
        sc.app.search_tracks = vec![track(3, "Milestones")];
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("Milestones"), "{out}");
        assert!(out.contains("results"), "{out}");
    }

    #[test]
    fn a_filter_is_shown_in_the_header_while_it_is_being_typed() {
        // Goal: the filter is modal and easy to forget you are in; it must be
        // visible on screen.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Blue")];
        sc.app.mode = Mode::Filter;
        sc.app.filter = "blu".into();
        assert!(text(&mut sc.app, 120, 12).contains("/blu"));
    }

    #[test]
    fn the_now_playing_row_shows_the_track_and_source() {
        // Goal: the badge is the point of a hi-res client - it must show what is
        // actually being fed to the DAC.
        let mut sc = screen();
        sc.app.now_playing = Some(track(1, "So What"));
        sc.app.now_meta = crate::app::StreamMeta {
            bit_depth: 24,
            sample_rate: 192_000,
            codec: "flac".into(),
            quality: "HI_RES_LOSSLESS".into(),
        };
        sc.app.status.duration = 245.0;
        sc.app.status.position = 61.0;
        sc.app.status.playing = true;
        sc.app.status.sample_rate = 192_000;
        sc.app.status.out_format = "s32".into();

        let out = text(&mut sc.app, 130, 12);
        assert!(out.contains("So What"), "{out}");
        assert!(out.contains("24"), "bit depth: {out}");
        assert!(out.contains("192"), "sample rate: {out}");
        assert!(out.contains("1:01"), "position: {out}");
        assert!(out.contains("OUT"), "the output badge: {out}");
    }

    #[test]
    fn nothing_playing_still_renders_a_complete_frame() {
        // Goal: the first frame, before any track exists, must not be blank or
        // panic on the missing metadata.
        let mut sc = screen();
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("Nothing playing"), "{out}");
        assert!(out.contains("quit"), "the reference is always up: {out}");
    }

    #[test]
    fn activity_shows_one_state_at_a_time() {
        // Goal: resolving, buffering and buffered are mutually exclusive; two at
        // once would mean the slot is lying about one of them.
        let mut sc = screen();
        sc.app.now_playing = Some(track(1, "T"));
        let buffering = text(&mut sc.app, 130, 12);
        assert!(buffering.contains("buffering"), "{buffering}");

        sc.app.status.loaded = true;
        sc.app.status.playing = true;
        sc.app.status.cache_secs = 42.0;
        let buffered = text(&mut sc.app, 130, 12);
        assert!(buffered.contains("42s buffered"), "{buffered}");
        assert!(!buffered.contains("buffering"), "{buffered}");
    }

    #[test]
    fn long_titles_are_truncated_with_an_ellipsis() {
        // Goal: an over-long title must not push the columns out of alignment.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, &"x".repeat(120))];
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains('…'), "expected an ellipsis in {out}");
    }

    #[test]
    fn the_shuffle_and_queue_indicators_appear_when_active() {
        // Goal: both are state the user has to be able to see at a glance.
        let mut sc = screen();
        sc.app.shuffle = true;
        sc.app.queue = vec![track(1, "A"), track(2, "B")];
        sc.app.queue_pos = 1;
        let out = text(&mut sc.app, 130, 12);
        assert!(out.contains("queue 2/2"), "{out}");
    }

    // ---- favorites ----

    /// Put a page of favorites on screen the way the worker would. Every row of
    /// that listing is a favorite, which is the only thing that ever says so.
    fn favorites_arrive(sc: &mut Screen, tracks: Vec<Track>) {
        // Asked for first: a page nobody is waiting on is dropped, which is the
        // guard that stops one listing's reply landing in another's rows.
        sc.app.start();
        let total = u32::try_from(tracks.len()).unwrap_or(u32::MAX);
        sc.to_app
            .send(FromWorker::Favorites {
                offset: 0,
                page: priel_core::Page {
                    items: tracks,
                    total,
                },
            })
            .expect("the rigged worker channel is open");
        sc.app.drain_worker();
    }

    fn press(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    #[test]
    fn a_row_says_whether_the_track_is_kept() {
        // Goal: the state has to be readable without acting on it, on the row
        // itself. Both glyphs are asserted from one frame: a renderer that drew
        // the same heart either way would satisfy half of this.
        let mut sc = screen();
        favorites_arrive(&mut sc, vec![track(1, "Kept One"), track(2, "Dropped One")]);
        sc.app.selected = 1;
        press(&mut sc.app, 'f');

        let out = draw(&mut sc.app, 100, 20);
        let kept = out
            .iter()
            .find(|l| l.contains("Kept One"))
            .expect("the first row");
        let dropped = out
            .iter()
            .find(|l| l.contains("Dropped One"))
            .expect("the second row");
        assert!(kept.contains('\u{2665}'), "a kept track is filled: {kept}");
        assert!(
            dropped.contains('\u{2661}'),
            "and one taken off is hollow: {dropped}"
        );
    }

    #[test]
    fn the_playing_track_wears_its_state_beside_its_title() {
        // Goal: the track in the speakers is routinely not the row under the
        // cursor, so its own state has to be on its own row.
        let mut sc = screen();
        favorites_arrive(&mut sc, vec![track(1, "Playing")]);
        sc.app.now_playing = Some(track(1, "Playing"));
        let out = text(&mut sc.app, 100, 20);
        assert!(out.contains("\u{2665} Artist"), "{out}");
    }

    #[test]
    fn the_heart_beside_the_playing_track_is_the_button_for_it() {
        // Goal: the state and the control are one glyph, so what is painted and
        // what answers a click cannot come apart - and clicking it has to run
        // the same thing the key does.
        let mut sc = screen();
        favorites_arrive(&mut sc, vec![track(1, "Playing")]);
        sc.app.now_playing = Some(track(1, "Playing"));
        assert_eq!(
            painted(&mut sc.app, 100, 20, Hit::FavoriteNowPlaying),
            "\u{2665} ",
            "the hit box covers the heart that was drawn"
        );

        click_hit(&mut sc.app, Hit::FavoriteNowPlaying);
        assert!(!sc.app.is_favorite(1), "the click ran the action");
        assert!(
            text(&mut sc.app, 100, 20).contains("\u{2661} Artist"),
            "and the glyph followed it"
        );
    }

    #[test]
    fn nothing_playing_offers_no_heart_to_click() {
        // Goal: a control that could not act on anything must not be on screen.
        // Registering its hit box anyway is how a click lands on a no-op that
        // looks like a bug.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 100, 20);
        assert!(
            !sc.app
                .hits
                .iter()
                .any(|(_, h)| *h == Hit::FavoriteNowPlaying)
        );
    }

    // ---- the overlay ----

    #[test]
    fn the_help_overlay_covers_the_list_and_says_how_to_close() {
        // Goal: it is modal, so it must actually obscure what is behind it, and
        // it must always show the way out.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Hidden Title")];
        let before = text(&mut sc.app, 100, 26);
        assert!(before.contains("Hidden Title"));

        sc.app.mode = Mode::Help;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("Keyboard and mouse"), "{out}");
        assert!(out.contains("to close"), "{out}");
        assert!(
            out.contains("choose the device"),
            "a binding that is not in here cannot be discovered: {out}"
        );
        assert!(
            !out.contains("Hidden Title"),
            "the list should be covered: {out}"
        );
    }

    #[test]
    fn every_section_of_the_reference_is_reachable_on_a_short_terminal() {
        // Goal: the reference holds more rows than a short terminal can show,
        // and it is the only place several bindings are named. Clipping it would
        // delete those bindings silently - the same failure that once lost `[q]`
        // off the end of the bottom row - so it scrolls, and the last section
        // has to be reachable by scrolling rather than merely below the fold.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let first = text(&mut sc.app, 100, 26);
        assert!(first.contains("Views"), "the top is shown first: {first}");
        assert!(
            first.contains("j k scroll"),
            "a reference with more below has to say so: {first}"
        );

        press(&mut sc.app, 'G');
        let last = text(&mut sc.app, 100, 26);
        assert!(
            last.contains("Mouse"),
            "the last section has to be reachable: {last}"
        );
        assert!(
            last.contains("to close"),
            "and the way out never scrolls away: {last}"
        );

        press(&mut sc.app, 'g');
        let back = text(&mut sc.app, 100, 26);
        assert_eq!(back, first, "g returns to exactly the top");
    }

    #[test]
    fn a_terminal_tall_enough_for_the_whole_reference_is_not_told_to_scroll() {
        // Goal: the negative space. The scroll hint costs width on the one line
        // that must always carry the way out, so it appears only when there is
        // something below to scroll to.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let out = text(&mut sc.app, 100, 60);
        assert!(out.contains("Mouse"), "it all fits at this height: {out}");
        assert!(!out.contains("j k scroll"), "{out}");
    }

    #[test]
    fn every_action_the_reference_names_is_clickable_where_it_was_printed() {
        // Goal: this overlay is where the mouse reaches what the bottom row has
        // no width for, so every key that names an action owes a hit box on the
        // cells it was painted on. Counted rather than sampled: a row added
        // without a `Hit` would otherwise be a keyboard-only action again.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let wanted: usize = HELP_LEFT
            .iter()
            .chain(HELP_RIGHT)
            .flat_map(|(_, rows)| rows.iter())
            .flat_map(|r| r.keys.iter())
            .filter(|(_, h)| h.is_some())
            .count();
        let _ = draw(&mut sc.app, 120, 40);
        assert_eq!(
            sc.app.hits.len(),
            wanted,
            "every action in the reference has to publish a hit box"
        );

        for (hit, glyph) in [
            (Hit::Log, "M"),
            (Hit::Devices, "d"),
            (Hit::SignIn, "A"),
            (Hit::EditSearch, "i"),
            (Hit::PageDown, "J"),
            (Hit::HalfPageUp, "Ctrl-U"),
        ] {
            assert_eq!(
                painted(&mut sc.app, 120, 40, hit),
                glyph,
                "{hit:?} must be clickable on the key it printed"
            );
        }
    }

    #[test]
    fn a_header_too_narrow_for_a_control_still_leaves_the_reference() {
        // Goal: the header runs out of width like any row, and the rule does not
        // stop being true at 60 columns. What survives every width is the `[?]`
        // hint, which `push_hints` reserves space for, and the overlay it opens -
        // where the same action is clickable whatever the header had room for.
        // That is the whole reason the reference carries hit boxes.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 60, 30);
        assert!(
            !sc.app.hits.iter().any(|(_, h)| *h == Hit::Devices),
            "a control the header could not paint must not answer to a click"
        );
        assert!(
            sc.app.hits.iter().any(|(_, h)| *h == Hit::Help),
            "but the way to the reference is reserved and cannot be dropped"
        );

        sc.app.mode = Mode::Help;
        assert_eq!(
            painted(&mut sc.app, 60, 30, Hit::Devices),
            "d",
            "and the reference still offers the picker at that width"
        );
    }

    #[test]
    fn a_word_the_reference_only_explains_offers_nothing_to_click() {
        // Goal: half of the Output section is the vocabulary of the badges, not
        // actions. Making those clickable would promise a control that could not
        // do anything, so they stay plain - which is also the record of the
        // judgement that they are not actions.
        let glossary = HELP_LEFT
            .iter()
            .chain(HELP_RIGHT)
            .flat_map(|(_, rows)| rows.iter())
            .filter(|r| r.keys.iter().all(|(_, h)| h.is_none()))
            .count();
        assert!(
            glossary >= 12,
            "the badge vocabulary and the mouse gestures are not actions"
        );
    }

    #[test]
    fn the_reference_takes_the_hit_boxes_over_from_the_row_behind_it() {
        // Goal: modal means a click cannot reach a control underneath. The header
        // registers its controls before the overlay draws, so the overlay has to
        // replace them - otherwise clicking to dismiss would also reload the list
        // or skip a track.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 120, 40);
        let under = sc
            .app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::Reload)
            .map_or_else(|| panic!("the header should publish reload"), |(r, _)| *r);

        sc.app.mode = Mode::Help;
        let _ = draw(&mut sc.app, 120, 40);
        assert!(
            !sc.app.hits.iter().any(|(r, _)| r.y == under.y),
            "nothing on the header's row may still be clickable"
        );

        sc.app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: under.x,
            row: under.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(sc.app.mode, Mode::Normal, "the click dismissed instead");
        assert!(
            sc.from_app.try_recv().is_err(),
            "and asked the worker for nothing"
        );
    }

    #[test]
    fn clicking_a_key_in_the_reference_runs_it() {
        // Goal: the round trip that makes this the mouse's route to everything -
        // the renderer publishes the key's geometry and the click runs the same
        // shared method the key press does.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let _ = draw(&mut sc.app, 120, 40);
        click_hit(&mut sc.app, Hit::Log);
        assert_eq!(sc.app.mode, Mode::Log, "`M` opens the recent log");

        sc.app.mode = Mode::Help;
        let _ = draw(&mut sc.app, 120, 40);
        click_hit(&mut sc.app, Hit::EditSearch);
        assert_eq!(sc.app.view, View::Search);
        assert_eq!(sc.app.mode, Mode::Search, "`i` opens the query box");
    }

    #[test]
    fn the_log_overlay_shows_the_newest_lines_and_the_way_out() {
        // Goal: the answer to "logging in a TUI is odd" is that nobody has to
        // leave priel to read it. The newest line is the one that matters, so it
        // must be on screen without scrolling.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Hidden Title")];
        for i in 0..40 {
            // Both levels that get a colour of their own, so a change to that
            // mapping shows up here rather than only on someone's screen.
            let level = if i % 2 == 0 { "ERROR" } else { "WARN " };
            sc.app
                .recent
                .push(format!("00:00:00.000Z {level} [worker] p: line {i}\n"));
        }
        sc.app.mode = Mode::Log;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("line 39"), "the newest line: {out}");
        assert!(out.contains("to close"), "{out}");
        assert!(!out.contains("Hidden Title"), "the list is covered: {out}");
    }

    /// Put a chain on screen the way the worker would, since that is the only
    /// way in: the app's copy is private, and a test that reached around the
    /// channel would not be testing the path the app actually uses.
    fn with_chain(sc: &mut Screen, graph: AudioGraph) {
        sc.to_app
            .send(FromWorker::AudioGraph(Ok(graph)))
            .expect("send");
        sc.app.drain_worker();
    }

    fn node(description: &str, role: NodeRole, rate_hz: u32, format: &str) -> GraphNode {
        GraphNode {
            id: 1,
            name: description.into(),
            description: description.into(),
            media_class: "Audio/Sink".into(),
            role,
            rate_hz: Some(rate_hz),
            format: Some(format.into()),
            channels: Some(2),
        }
    }

    #[test]
    fn the_configuration_change_reaches_the_screen_whole() {
        // Goal: the overlay clips rather than wrapping, and a setting whose
        // tail is clipped is worse than no advice at all - it still looks like
        // something that can be copied. Ten permitted rates is a real machine's
        // list, and the line that adds one to it is the longest thing this
        // overlay ever draws.
        let mut sc = screen();
        sc.app.status.loaded = true;
        sc.app.status.playing = true;
        sc.app.status.in_sample_rate = 352_800;
        sc.app.status.in_format = "s32".into();
        sc.app.now_meta.bit_depth = 24;
        with_chain(
            &mut sc,
            AudioGraph {
                path: vec![node("Studio DAC", NodeRole::Device, 352_800, "S32LE")],
                clock: ClockRates {
                    allowed_hz: Some(vec![
                        44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 384_000, 705_600, 768_000,
                    ]),
                    current_hz: Some(48_000),
                    forced_hz: None,
                },
                ..AudioGraph::default()
            },
        );
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 40);
        assert!(out.contains("Server clock"), "{out}");
        assert!(out.contains("352.8 kHz  not permitted"), "{out}");
        assert!(
            out.contains("768 kHz"),
            "the readout carries onto a second row rather than losing its tail: {out}"
        );
        assert!(out.contains("default.clock.allowed-rates = ["), "{out}");
        assert!(out.contains("352800"), "the rate being added: {out}");
        assert!(out.contains("768000"), "and the tail of the list: {out}");
        assert!(out.contains("pipewire.conf.d"), "where it goes: {out}");
        assert!(out.contains("Restart the sound server"), "{out}");
    }

    #[test]
    fn the_reservation_rule_reaches_the_screen_whole() {
        // Goal: the same hazard the rate advice has. The ownership section is
        // the last thing in a box sized to its content, so it is the first
        // thing lost when anything above it grows - and a rule with its tail
        // clipped still looks like something that can be copied.
        let mut sc = screen();
        with_chain(
            &mut sc,
            AudioGraph {
                path: vec![node("Studio DAC", NodeRole::Device, 44_100, "S32LE")],
                holder: DeviceHolder::Server(HeldDevice {
                    sink: "Studio DAC".into(),
                    opened_by: Some("wireplumber".into()),
                    pcm: Some("hw:2,0".into()),
                    card_name: Some("alsa_card.usb-Studio_DAC-00".into()),
                }),
                ..AudioGraph::default()
            },
        );
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 40);
        assert!(out.contains("Output device"), "{out}");
        assert!(
            out.contains("the sound server (wireplumber)"),
            "what has it open: {out}"
        );
        assert!(out.contains("hw:2,0"), "which device: {out}");
        assert!(
            out.contains("wireplumber.conf.d"),
            "where the rule goes: {out}"
        );
        assert!(
            out.contains("alsa_card.usb-Studio_DAC-00"),
            "the card name survives the box: {out}"
        );
        assert!(out.contains("device.disabled = true"), "the change: {out}");
        assert!(
            out.contains("Nothing else on this machine"),
            "and what it costs: {out}"
        );
    }

    #[test]
    fn the_graph_overlay_lists_every_node_with_what_it_negotiated() {
        // Goal: the question the DAC badge cannot answer - which nodes sit in
        // front of the device, and what each of them is doing to the audio. All
        // of it has to survive onto the screen, not just into the model.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Hidden Title")];
        with_chain(
            &mut sc,
            AudioGraph {
                path: vec![
                    node("mpv", NodeRole::Stream, 44_100, "S16LE"),
                    node("Loopback", NodeRole::Intermediate, 48_000, "F32LE"),
                    node("Studio DAC", NodeRole::Device, 48_000, "S32LE"),
                ],
                ..AudioGraph::default()
            },
        );
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 36);
        assert!(out.contains("Output"), "{out}");
        assert!(out.contains("Loopback"), "the middle hop is shown: {out}");
        assert!(out.contains("Studio DAC"), "{out}");
        assert!(out.contains("44.1 kHz"), "the stream's rate: {out}");
        assert!(out.contains("S32LE"), "the device's format: {out}");
        assert!(out.contains("to close"), "{out}");
        // Unlike the help and log overlays this one is sized to its content, so
        // it is not expected to cover the whole list. Being modal is about the
        // input it swallows, which `app` covers.
    }

    #[test]
    fn the_node_that_altered_the_samples_is_marked_on_screen() {
        // Goal: the accusation is only useful if it survives onto the screen
        // next to the row it is about. The overlay is sized to its content and
        // clips whatever overflows, so a sentence added below the chain is
        // exactly the thing that can be lost without any test noticing.
        let mut sc = screen();
        sc.app.status.loaded = true;
        sc.app.status.playing = true;
        sc.app.status.volume = 100.0;
        sc.app.status.in_sample_rate = 44_100;
        sc.app.status.in_format = "s32".into();
        sc.app.status.sample_rate = 48_000;
        sc.app.status.out_format = "s32".into();
        sc.app.now_meta.bit_depth = 24;
        with_chain(
            &mut sc,
            AudioGraph {
                path: vec![
                    node("mpv", NodeRole::Stream, 44_100, "S32LE"),
                    node("Loopback", NodeRole::Intermediate, 48_000, "F32LE"),
                    node("Studio DAC", NodeRole::Device, 48_000, "S32LE"),
                ],
                ..AudioGraph::default()
            },
        );
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("⚠ Loopback"), "the row is marked: {out}");
        assert!(
            out.contains("Loopback is resampling."),
            "and named in words: {out}"
        );
        assert!(
            !out.contains("⚠ Studio DAC"),
            "the device behind it is at 48 kHz too and is not the cause: {out}"
        );
    }

    #[test]
    fn a_graph_that_could_not_be_read_explains_itself_on_screen() {
        // Goal: a machine with no PipeWire tools must get a sentence, not an
        // empty box - an empty overlay reads as priel being broken.
        let mut sc = screen();
        sc.to_app
            .send(FromWorker::AudioGraph(Err(GraphError::NotInstalled)))
            .expect("send");
        sc.app.drain_worker();
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("pw-dump"), "{out}");
        assert!(out.contains("PipeWire"), "and what to install: {out}");
    }

    #[test]
    fn the_reference_lists_the_key_that_opens_the_graph() {
        // Goal: the bottom row drops optional hints on a narrow terminal, so
        // `?` is the only place a binding is guaranteed to be findable. A key
        // that is nowhere in there is a key nobody discovers.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let out = text(&mut sc.app, 100, 30);
        assert!(out.contains("the output report"), "{out}");
    }

    #[test]
    fn clicking_the_graph_hint_opens_the_same_overlay_the_key_does() {
        // Goal: a key press and a click must run the same code. Going through
        // the published hit box is what proves the two paths have not drifted.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 200, 20);
        let (rect, _) = *sc
            .app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::Graph)
            .expect("the graph hint is clickable");
        sc.app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(sc.app.mode, Mode::Graph);
        assert!(
            matches!(sc.from_app.try_recv(), Ok(ToWorker::ReadAudioGraph)),
            "and asks the worker, exactly as the key press does"
        );
    }

    #[test]
    fn the_graph_overlay_says_it_is_reading_while_the_worker_is() {
        // Goal: the read is a subprocess on another thread, so there is always a
        // moment with no answer yet. That moment must say so rather than look
        // like an empty result.
        let mut sc = screen();
        sc.app.mode = Mode::Graph;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("Reading"), "{out}");
    }

    /// Three devices, the middle one a plausible DAC.
    fn devices() -> Vec<priel_player::AudioDevice> {
        [
            ("auto", "Autoselect device"),
            ("pipewire/some.dac", "A Nice DAC Pro"),
            ("alsa/hdmi:CARD=HDMI", "HDMI Audio Output"),
        ]
        .iter()
        .map(|(n, d)| priel_player::AudioDevice {
            name: (*n).to_string(),
            description: (*d).to_string(),
        })
        .collect()
    }

    #[test]
    fn the_device_picker_names_every_device_and_marks_the_one_in_use() {
        // Goal: the whole issue in one frame. The identifier is what --device
        // takes and the description is what makes it recognisable, so both have
        // to be on screen, and the row already in use has to stand out.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Hidden Title")];
        sc.app.set_devices_for_test(devices());
        sc.app.status.audio_device = "pipewire/some.dac".into();
        sc.app.mode = Mode::Devices;

        let out = text(&mut sc.app, 100, 20);
        assert!(out.contains("Output device"), "{out}");
        assert!(out.contains("pipewire/some.dac"), "{out}");
        assert!(out.contains("A Nice DAC Pro"), "{out}");
        assert!(out.contains("HDMI Audio Output"), "{out}");
        assert!(
            out.contains("* pipewire/some.dac"),
            "the device in use should be marked: {out}"
        );
        assert!(
            !out.contains("* auto"),
            "and only that one should be: {out}"
        );
        assert!(!out.contains("Hidden Title"), "the list is covered: {out}");
    }

    #[test]
    fn the_device_picker_says_the_choice_lasts_for_this_session() {
        // Goal: priel reads no configuration file, deliberately. A picker that
        // did not say so would look broken on the next start.
        let mut sc = screen();
        sc.app.set_devices_for_test(devices());
        sc.app.mode = Mode::Devices;
        let out = text(&mut sc.app, 100, 20);
        assert!(out.contains("this session only"), "{out}");
        assert!(
            out.contains("--device"),
            "and it must say what does make it permanent: {out}"
        );
        assert!(out.contains("to close"), "{out}");
    }

    #[test]
    fn every_device_row_on_screen_is_clickable_where_it_was_drawn() {
        // Goal: the rows are the only clickable thing in this overlay, and a hit
        // box that drifted from what was painted would switch the output to a
        // device other than the one under the pointer.
        let mut sc = screen();
        sc.app.set_devices_for_test(devices());
        sc.app.mode = Mode::Devices;
        let lines = draw(&mut sc.app, 100, 20);

        assert_eq!(sc.app.device_rows.len(), 3, "one hit box per drawn row");
        for (rect, index) in sc.app.device_rows.clone() {
            let painted = &lines[rect.y as usize];
            assert!(
                painted.contains(&sc.app.devices()[index].name),
                "row {index} claims a line that does not show it: {painted}"
            );
        }
    }

    #[test]
    fn an_empty_device_list_says_so_rather_than_showing_a_blank_box() {
        // Goal: a build without libmpv has nothing to list, and an empty box
        // reads as a bug rather than as an answer.
        let mut sc = screen();
        sc.app.mode = Mode::Devices;
        let out = text(&mut sc.app, 100, 20);
        assert!(
            out.contains("No output devices were reported"),
            "the empty case is itself the answer: {out}"
        );
        assert!(sc.app.device_rows.is_empty(), "nothing to click");
    }

    #[test]
    fn an_empty_log_says_so_rather_than_showing_a_blank_box() {
        // Goal: an empty overlay reads as broken. Nothing recorded is itself
        // the answer to "what went wrong", and worth saying in words.
        let mut sc = screen();
        sc.app.mode = Mode::Log;
        let out = text(&mut sc.app, 100, 26);
        assert!(out.contains("Nothing recorded yet"), "{out}");
    }

    #[test]
    fn the_overlay_stacks_into_one_column_on_a_narrow_terminal() {
        // Goal: two columns on a narrow screen clip every description in half.
        // Stacking keeps the text readable instead.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let narrow = text(&mut sc.app, 60, 40);
        assert!(narrow.contains("Views"), "{narrow}");
        assert!(narrow.contains("Mouse"), "{narrow}");
        assert!(narrow.contains("to close"), "{narrow}");
    }

    #[test]
    fn a_tiny_terminal_renders_without_panicking() {
        // Goal: every rect here is computed by hand from the frame size. A
        // terminal smaller than the layout expects must clamp, not overflow.
        for (w, h) in [(1u16, 1u16), (5, 3), (20, 5), (40, 6)] {
            let mut sc = screen();
            sc.app.favorites = vec![track(1, "T")];
            sc.app.now_playing = Some(track(1, "T"));
            let _ = draw(&mut sc.app, w, h);
            sc.app.mode = Mode::Help;
            let _ = draw(&mut sc.app, w, h);
            sc.app.recent.push("a line that will not fit\n".to_string());
            sc.app.mode = Mode::Log;
            let _ = draw(&mut sc.app, w, h);
            with_chain(
                &mut sc,
                AudioGraph {
                    path: vec![node(
                        "a very long device description",
                        NodeRole::Device,
                        96_000,
                        "S24_3LE",
                    )],
                    ..AudioGraph::default()
                },
            );
            sc.app.mode = Mode::Graph;
            let _ = draw(&mut sc.app, w, h);
        }
    }

    // ---- render feeds the mouse ----

    #[test]
    fn rendering_publishes_hit_boxes_that_clicking_a_tab_then_uses() {
        // Goal: the round trip that makes the mouse work - the renderer records
        // geometry, the app acts on it. Testing them apart would miss a drift
        // between the two.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 120, 12);
        let (rect, _) = *sc
            .app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::View(View::Search))
            .expect("the search tab should be clickable");

        sc.app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(sc.app.view, View::Search);
    }

    #[test]
    fn the_reload_control_is_painted_where_a_click_on_it_lands() {
        // Goal: every action has to be reachable with the mouse as well as from
        // the keyboard, and a control whose hit box has drifted from what was
        // drawn is reachable from neither.
        let mut sc = screen();
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains('↻'), "the control has to be visible: {out}");
        let (rect, _) = *sc
            .app
            .hits
            .iter()
            .find(|(_, h)| *h == Hit::Reload)
            .expect("reload should be clickable");
        assert!(rect.width > 0, "and have somewhere to click");
    }

    #[test]
    fn the_list_rect_recorded_by_the_renderer_maps_clicks_to_rows() {
        // Goal: `list_inner` is written during render and read by the click
        // handler; if the layout moves and this does not, clicks hit the wrong
        // row.
        let mut sc = screen();
        sc.app.favorites = (0..8).map(|i| track(i, "T")).collect();
        let _ = draw(&mut sc.app, 120, 14);
        let inner = sc.app.list_inner;
        assert!(inner.height > 0, "the list must have been given a rect");

        sc.app.on_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: inner.x + 1,
            row: inner.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(sc.app.selected, 2, "the third visible row");
    }

    #[test]
    fn a_click_on_the_painted_bar_seeks_to_the_fraction_it_landed_on() {
        // Goal: `progress_rect` is written by the renderer and read by the click
        // handler, and a click that lands on the wrong seconds is invisible in a
        // screenshot. Method: render a real frame, ask what a click on a known
        // cell of the painted bar means, and check the seconds it names.
        let mut sc = screen();
        sc.app.now_playing = Some(track(1, "Blue in Green"));
        sc.app.status.duration = 200.0;
        let _ = draw(&mut sc.app, 80, 24);

        let pr = sc.app.progress_rect;
        assert!(pr.width > 0, "the bar must have been given a rect");
        assert_eq!(sc.app.click_at(pr.x, pr.y), Click::Seek(0.0));
        assert_eq!(
            sc.app.click_at(pr.x + pr.width / 2, pr.y),
            Click::Seek(100.0),
            "halfway along the bar is halfway through the track"
        );
    }

    #[test]
    fn scrolling_keeps_the_selection_on_screen() {
        // Goal: the list window follows the cursor. Without this the selection
        // walks off the bottom and the user is moving something invisible.
        let mut sc = screen();
        sc.app.favorites = (0..50).map(|i| track(i, "T")).collect();
        let _ = draw(&mut sc.app, 120, 12);
        sc.app.selected = 40;
        let _ = draw(&mut sc.app, 120, 12);
        assert!(sc.app.list_offset > 0, "the window should have scrolled");
        assert!(
            sc.app.selected >= sc.app.list_offset
                && sc.app.selected < sc.app.list_offset + sc.app.list_inner.height as usize,
            "the selection must stay inside the window"
        );
    }

    // ---- the bit-perfect indicator ----

    /// Put the app into a playing state with a given decode/output chain.
    fn chain(sc: &mut Screen, source_bits: u32, in_rate: u32, out_rate: u32, out_fmt: &str) {
        sc.app.now_playing = Some(track(1, "T"));
        sc.app.now_meta = crate::app::StreamMeta {
            bit_depth: source_bits,
            sample_rate: in_rate,
            codec: "flac".into(),
            quality: "HI_RES_LOSSLESS".into(),
        };
        sc.app.status.loaded = true;
        sc.app.status.playing = true;
        sc.app.status.volume = 100.0;
        sc.app.status.in_sample_rate = in_rate;
        sc.app.status.in_format = "s32".into();
        sc.app.status.sample_rate = out_rate;
        sc.app.status.out_format = out_fmt.into();
        // Both stages mpv can see, read and at unity, so a test only has to
        // move the one it is about - and so a clean chain reads as a clean
        // tick rather than an unlooked-at one.
        sc.app.status.ao_volume = Some(100.0);
        // The third stage lives in the graph dump and arrives the same way the
        // overlay's does. Without a reading the sink is a stage that exists and
        // went unlooked-at, which is a mark on every one of these frames.
        sc.to_app
            .send(FromWorker::AudioGraph(Ok(AudioGraph {
                volume: SinkVolume::Read(SinkLevels {
                    set: vec![1.0, 1.0],
                    software: vec![1.0, 1.0],
                    silenced: false,
                }),
                ..AudioGraph::default()
            })))
            .expect("send");
        sc.app.drain_worker();
    }

    #[test]
    fn a_clean_chain_is_announced_on_screen() {
        // Goal: this is the payoff for the whole bit-perfect design, so it has
        // to be visible without hunting for it.
        let mut sc = screen();
        chain(&mut sc, 24, 192_000, 192_000, "s32");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("bit-perfect"), "{out}");
    }

    #[test]
    fn a_wider_output_container_does_not_raise_a_warning() {
        // Goal: 24-bit content leaving as s32 is the ordinary case on a USB DAC.
        // Warning about it would train the listener to ignore the indicator.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("bit-perfect"), "{out}");
        assert!(!out.contains('⚠'), "{out}");
    }

    #[test]
    fn a_truncating_output_names_the_format_it_dropped_to() {
        // Goal: 24-bit content leaving as s16 is a real loss, and the listener
        // needs to know which link to fix.
        let mut sc = screen();
        chain(&mut sc, 24, 44_100, 44_100, "s16");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("truncated"), "{out}");
        assert!(out.contains("S16"), "should name the output format: {out}");
        assert!(!out.contains("bit-perfect"), "{out}");
    }

    #[test]
    fn resampling_shows_both_rates() {
        // Goal: the rate pair is the diagnosis - it points straight at a sink
        // locked to one rate.
        let mut sc = screen();
        chain(&mut sc, 24, 44_100, 48_000, "s32");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("resampled"), "{out}");
        assert!(out.contains("44") && out.contains("48"), "{out}");
    }

    #[test]
    fn a_level_change_is_graded_apart_from_a_rebuilt_stream() {
        // Goal: turning the volume down and resampling are different kinds of
        // thing, and flattening them into one warning makes the indicator
        // useless for the people who care most. The row says which kind; which
        // stage did it is the report's job.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.status.volume = 70.0;
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("near bit-perfect"), "{out}");
        assert!(!out.contains("resampled"), "{out}");
    }

    #[test]
    fn the_row_carries_no_remedy_of_its_own() {
        // Goal: `0 for unity` used to sit in this warning, and it only ever
        // cleared priel's own volume. Now that the sink is a possible cause, an
        // inline remedy would be actively wrong in exactly the cases this badge
        // exists to catch - so the row says what, and the report says what to
        // do.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.status.ao_volume = Some(40.0);
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("near bit-perfect"), "{out}");
        assert!(!out.contains("for unity"), "no remedy on the row: {out}");
    }

    #[test]
    fn the_verdict_is_one_of_four_words_and_carries_no_numbers() {
        // Goal: the row was over budget - at worst a hundred columns of badges
        // before a single key hint, so on an eighty-column terminal every hint
        // was already dropped. The numbers are on screen twice already, in the
        // source badge and the device readout, so the verdict adds a word and
        // nothing else.
        let mut sc = screen();
        chain(&mut sc, 24, 44_100, 44_100, "s16");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("⚠ truncated"), "{out}");
        assert!(
            !out.contains("truncated to"),
            "the format it dropped to is in the device readout already: {out}"
        );
    }

    #[test]
    fn a_tick_reached_without_reading_every_stage_says_so() {
        // Goal: the overstatement this work removes. Today's tick already means
        // "as far as I looked" and nobody can tell. A stage that exists and
        // could not be read does not silently keep the tick - and does not void
        // it either.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        assert!(
            text(&mut sc.app, 140, 12).contains("✓ bit-perfect"),
            "everything readable was read"
        );

        // The sound server is in the chain and its level for our stream could
        // not be read, which is a stage that exists and went unlooked-at.
        sc.app.status.ao_volume = None;
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("✓? bit-perfect"), "{out}");
    }

    #[test]
    fn the_mark_carries_its_meaning_with_no_colour_at_all() {
        // Goal: a dimmed tick would say nothing on a monochrome terminal, in a
        // light theme, or to the red/green deficiency these grades already lean
        // on. The glyph costs one column and survives all of them.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.status.ao_volume = None;
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains('?'), "the mark is in the text itself: {out}");
    }

    #[test]
    fn clicking_the_verdict_opens_the_report_behind_it() {
        // Goal: the row says whether; clicking it says why. It runs the same
        // method `[D]` does, so the two cannot drift, and the hit box is
        // registered in the walk that painted the span.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        // Wide enough for the bottom row to still be carrying `[D] graph`,
        // which is the point: the badge is a second way in, never a
        // replacement for the labelled key that makes it discoverable.
        let _ = text(&mut sc.app, 220, 12);
        let boxes: Vec<Rect> = sc
            .app
            .hits
            .iter()
            .filter(|(_, h)| *h == Hit::Graph)
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(
            boxes.len(),
            2,
            "the verdict and the [D] hint both open it: {boxes:?}"
        );
        click_hit(&mut sc.app, Hit::Graph);
        assert_eq!(sc.app.mode, Mode::Graph);
    }

    #[test]
    fn nothing_is_claimed_before_playback_starts() {
        // Goal: a green light on an idle player would be a lie, and this badge
        // is the one thing in the interface that must never overstate.
        let mut sc = screen();
        let out = text(&mut sc.app, 140, 12);
        assert!(!out.contains("bit-perfect"), "{out}");
        assert!(!out.contains('⚠'), "{out}");
    }

    #[test]
    fn the_output_badge_does_not_claim_to_be_the_dac() {
        // Goal: mpv reports the format it wrote to the audio API, not the rate
        // the hardware is clocked at. Labelling that "DAC" would tell an
        // audiophile something priel cannot actually know.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("OUT S32"), "{out}");
        assert!(!out.contains("DAC"), "the badge must not overstate: {out}");
    }

    #[test]
    fn the_report_always_says_how_the_device_is_held() {
        // Goal: the whole reason for taking a device is that the chain is then
        // priel's alone, and an interface that could not say so would leave the
        // listener no way to tell they got what they asked for. The shared case
        // is named too, so the state is read off the line rather than inferred
        // from a word being absent.
        //
        // It reads here rather than on the bottom row: access is a session-long
        // setting rather than something that changes per track, and the row had
        // no width left to spare.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.set_mode_for_test(Mode::Graph);

        let shared = text(&mut sc.app, 140, 24);
        assert!(
            shared.contains("shared"),
            "how the device is held is always on screen, not only when it is \
             exclusive: {shared}"
        );
        assert!(
            !shared.contains("exclusive"),
            "and the shared path must not read as the exclusive one: {shared}"
        );

        sc.app.status.access = OutputAccess::Exclusive;
        let out = text(&mut sc.app, 140, 24);
        assert!(out.contains("exclusive"), "{out}");
    }

    #[test]
    fn nothing_playing_names_no_access_at_all() {
        // Goal: `OUT —` already says there is no output open. Naming an access
        // mode beside it would claim priel is holding a device shared when it
        // is holding none, which is the same overstatement the `DAC`/`OUT`
        // distinction exists to avoid.
        let mut sc = screen();
        sc.app.set_mode_for_test(Mode::Graph);

        let out = text(&mut sc.app, 140, 24);
        assert!(
            !out.contains("shared"),
            "an idle player holds nothing, so it names no access: {out}"
        );
        assert!(
            out.contains("nothing open"),
            "and says so, rather than leaving the line blank: {out}"
        );
    }

    #[test]
    fn a_refused_exclusive_open_is_reported_as_shared_output() {
        // Goal: the indicator never claims exclusivity it did not get. A player
        // that fell back to the mixer while the report still implied a direct
        // connection would be worse than not offering the path at all.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.status.access = OutputAccess::Refused;
        sc.app.set_mode_for_test(Mode::Graph);

        let out = text(&mut sc.app, 140, 24);
        assert!(out.contains("shared"), "it has to say what it got: {out}");
        assert!(
            out.contains("refused"),
            "and that the request was not met: {out}"
        );
    }

    #[test]
    fn the_picker_carries_the_exclusivity_toggle_where_it_can_be_clicked() {
        // Goal: parity runs both ways - the `x` key and this control run the
        // same method, and a hit box that drifted from what was painted would
        // take a device on a click meant for something else.
        let mut sc = screen();
        sc.app.set_devices_for_test(devices());
        sc.app.mode = Mode::Devices;

        let lines = draw(&mut sc.app, 100, 20);
        let rect = sc.app.device_exclusive_rect;
        assert!(rect.width > 0, "the control has to be drawn to be clicked");
        let painted = &lines[rect.y as usize];
        assert!(
            painted.contains("exclusive"),
            "the hit box claims a line that does not show it: {painted}"
        );
        assert!(
            painted.contains("off"),
            "and it says which way it is set: {painted}"
        );

        sc.app
            .on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let after = draw(&mut sc.app, 100, 20);
        assert!(
            after[sc.app.device_exclusive_rect.y as usize].contains("on"),
            "the toggle has to show the new state: {:?}",
            after[sc.app.device_exclusive_rect.y as usize]
        );
    }

    #[test]
    fn a_picker_too_short_to_draw_the_toggle_offers_nothing_to_click() {
        // Goal: hit boxes outlive the frame that drew them unless they are
        // cleared, and a control that was not painted must not still take the
        // device when something else is clicked where it used to be.
        let mut sc = screen();
        sc.app.set_devices_for_test(devices());
        sc.app.mode = Mode::Devices;
        draw(&mut sc.app, 100, 20);
        assert!(sc.app.device_exclusive_rect.width > 0);

        draw(&mut sc.app, 100, 4);
        assert_eq!(
            sc.app.device_exclusive_rect,
            Rect::default(),
            "nothing was painted, so there is nothing to click"
        );
    }

    #[test]
    fn the_output_picker_is_opened_by_a_control_painted_in_the_header() {
        // Goal: the picker answered to `d` and to nothing at all on screen, which
        // is the asymmetry the parity rule exists to catch. The control has to be
        // painted, and its hit box has to be the cells it was painted on.
        let mut sc = screen();
        assert_eq!(
            painted(&mut sc.app, 120, 12, Hit::Devices),
            " ◎ ",
            "the hit box must cover the glyph that was drawn"
        );
        click_hit(&mut sc.app, Hit::Devices);
        assert_eq!(
            sc.app.mode,
            Mode::Devices,
            "and clicking it must open the same picker `d` opens"
        );
    }

    #[test]
    fn the_theme_picker_is_opened_by_a_control_painted_in_the_header() {
        // Goal: parity runs both ways - the picker answers to `t`, and it must
        // also be reachable by pointing at something. The hit box has to be the
        // cells the glyph was painted on, or the click lands elsewhere.
        let mut sc = screen();
        assert_eq!(
            painted(&mut sc.app, 120, 12, Hit::Themes),
            " ◐ ",
            "the hit box must cover the glyph that was drawn"
        );
        click_hit(&mut sc.app, Hit::Themes);
        assert_eq!(
            sc.app.mode,
            Mode::Themes,
            "and clicking it must open the same picker `t` opens"
        );
    }

    #[test]
    fn the_theme_picker_names_every_palette_and_how_long_a_choice_lasts() {
        // Goal: priel reads no configuration file, so the overlay owes the
        // reader the same promise the output picker makes - this is for the
        // session, and the flag is what keeps it.
        let mut sc = screen();
        sc.app.mode = Mode::Themes;
        let out = text(&mut sc.app, 100, 20);
        for name in crate::theme::OFFERED {
            let label = crate::theme::label(*name);
            assert!(out.contains(&label), "{label} is not offered: {out}");
        }
        assert!(out.contains("this session only"), "{out}");
        assert!(out.contains("--theme"), "{out}");
        assert!(out.contains("to close"), "an overlay must say the way out");
    }

    #[test]
    fn every_theme_row_on_screen_is_clickable_where_it_was_drawn() {
        // Goal: the rows are the control, so a hit box that drifted from what
        // was painted would repaint priel in a palette the user did not point
        // at.
        let mut sc = screen();
        sc.app.mode = Mode::Themes;
        let lines = draw(&mut sc.app, 100, 20);
        assert_eq!(
            sc.app.theme_rows.len(),
            crate::theme::OFFERED.len(),
            "every palette on screen needs a hit box"
        );
        for (rect, name) in sc.app.theme_rows.clone() {
            let painted = &lines[rect.y as usize];
            assert!(
                painted.contains(&crate::theme::label(name)),
                "the hit box for {name:?} claims a line that does not show it: {painted}"
            );
        }
    }

    #[test]
    fn each_theme_row_shows_its_own_three_grades_rather_than_the_current_ones() {
        // Goal: the grades are the reason a palette is a decision at all, so
        // the picker previews them in the palette being offered - each mark in
        // that theme's own colour, on that theme's own background. Reading the
        // current theme's colours here would show five identical rows.
        let mut sc = screen();
        sc.app.mode = Mode::Themes;
        let mut term = Terminal::new(TestBackend::new(100, 20)).expect("backend");
        term.draw(|f| render(f, &mut sc.app)).expect("render");
        let buf = term.backend().buffer().clone();
        assert!(
            !sc.app.theme_rows.is_empty(),
            "no rows means nothing proved"
        );

        for (rect, name) in sc.app.theme_rows.clone() {
            let t = Theme::of(name);
            let mut seen = Vec::new();
            for x in rect.x..rect.x.saturating_add(rect.width) {
                let cell = &buf[(x, rect.y)];
                if matches!(cell.symbol(), "✓" | "≈" | "⚠") {
                    seen.push((cell.symbol().to_string(), cell.fg, cell.bg));
                }
            }
            assert_eq!(seen.len(), 3, "{name:?} should preview all three grades");
            let colours = [t.verdict_clean, t.verdict_near, t.verdict_altered];
            for ((glyph, fg, bg), want) in seen.iter().zip(colours) {
                assert_eq!(*fg, want, "{name:?}: {glyph} is not its own grade colour");
                assert_eq!(*bg, t.background, "{name:?}: {glyph} is on the wrong base");
            }
        }
    }

    #[test]
    fn the_reference_lists_the_key_that_changes_the_palette() {
        // Goal: `t` is not on the bottom row - it is not an everyday action -
        // so the `?` overlay is the only place it can be discovered, and the
        // only place it can be clicked at a narrow width.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let out = text(&mut sc.app, 100, 40);
        assert!(out.contains("colour theme"), "{out}");
        let has_control = sc.app.hits.iter().any(|(_, h)| *h == Hit::Themes);
        assert!(has_control, "and the reference is where it is clicked");
    }

    #[test]
    fn the_reference_lists_the_key_that_takes_the_device() {
        // Goal: the bottom row stays short by keeping everything not used
        // constantly in the `?` overlay, which makes the overlay the only place
        // this key is discoverable.
        let mut sc = screen();
        sc.app.mode = Mode::Help;
        let out = text(&mut sc.app, 100, 30);
        assert!(out.contains("exclusive"), "{out}");
    }

    #[test]
    fn the_volume_readout_marks_unity_as_the_good_state() {
        // Goal: enthusiasts need to see at a glance that 100% is the wanted
        // value, not just read a number with no reference.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        assert!(text(&mut sc.app, 140, 12).contains("100%"));

        sc.app.status.volume = 65.0;
        let out = text(&mut sc.app, 140, 12);
        assert!(out.contains("65%"), "{out}");
    }

    #[test]
    fn a_level_set_outside_priel_still_breaks_the_chain() {
        // Goal: the audio server attenuates in software just as priel does, so
        // priel sitting at unity does not make the chain clean. Which stage it
        // was is the report's answer; that it happened is the row's.
        let mut sc = screen();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        sc.app.status.ao_volume = Some(40.0);
        let out = text(&mut sc.app, 160, 12);
        assert!(out.contains("near bit-perfect"), "{out}");
        assert!(!out.contains("✓ bit-perfect"), "{out}");
    }

    #[test]
    fn the_badge_says_dac_only_when_it_read_the_device() {
        // Goal: `DAC` is a claim about hardware. It may appear only when the
        // numbers came from /proc/asound, and the hardware rate must win over
        // whatever the audio server reported.
        let mut sc = screen();
        chain(&mut sc, 24, 44_100, 44_100, "s32");
        assert!(text(&mut sc.app, 160, 12).contains("OUT S32"));

        sc.app.status.hw = Some(priel_player::hw::HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        let out = text(&mut sc.app, 160, 12);
        assert!(out.contains("DAC S32_LE · 48 kHz"), "{out}");
        assert!(
            out.contains("resampled"),
            "the device rate contradicts the stream, which is the whole point: {out}"
        );
    }

    #[test]
    fn rates_keep_the_decimal_that_distinguishes_the_families() {
        // Goal: 44.1 and 48 kHz are the two worlds a bit-perfect chain lives
        // between. Rendering 44100 as "44 kHz" makes them look adjacent, and
        // hides that a 44.1 source on a 48 kHz device is being rebuilt.
        assert_eq!(super::fmt_khz(44_100), "44.1 kHz");
        assert_eq!(super::fmt_khz(88_200), "88.2 kHz");
        assert_eq!(super::fmt_khz(176_400), "176.4 kHz");
        assert_eq!(super::fmt_khz(48_000), "48 kHz", "whole rates stay whole");
        assert_eq!(super::fmt_khz(192_000), "192 kHz");
        assert_eq!(super::fmt_khz(0), "?");
    }

    #[test]
    fn a_list_of_rates_says_the_unit_once_and_keeps_every_decimal() {
        // Goal: a permitted-rate list runs to ten entries, and a " kHz" after
        // each of them pushes the row past the box for no information at all -
        // but the decimal is the whole distinction and stays on every one.
        assert_eq!(
            super::fmt_khz_list(&[44_100, 48_000, 705_600]),
            "44.1 / 48 / 705.6 kHz"
        );
        assert_eq!(super::fmt_khz_list(&[48_000]), "48 kHz");
        assert!(
            super::fmt_khz_list(&[]).is_empty(),
            "no rates is no reading, not a bare unit"
        );
    }

    #[test]
    fn a_resample_names_the_rate_the_hardware_is_actually_running_at() {
        // Goal: the destination must come from the device, not from the audio
        // server. In a resample the server reports back the rate it accepted -
        // the source rate - so reading it there produced "resampled 44.1→44.1".
        let mut sc = screen();
        chain(&mut sc, 24, 44_100, 44_100, "s32");
        sc.app.status.hw = Some(priel_player::hw::HwParams {
            card: "AUDIO".into(),
            rate: 48_000,
            format: "S32_LE".into(),
            channels: 2,
        });
        let out = text(&mut sc.app, 160, 12);
        assert!(out.contains("⚠ resampled"), "{out}");
        assert!(
            out.contains("DAC S32_LE · 48 kHz"),
            "the destination is the device's own rate, not the one the server \
             accepted from us - reading it there produced 44.1 to 44.1: {out}"
        );
    }

    #[test]
    fn the_consent_screen_states_what_it_will_do_before_doing_it() {
        // Goal: priel is about to download a credential belonging to someone
        // else's application. A user who later discovers that must find nothing
        // here they were not told: where it comes from, whose it is, what gets
        // written where, and how to avoid the download entirely.
        let mut sc = screen();
        sc.app.set_mode_for_test(Mode::Credentials);
        let out = text(&mut sc.app, 96, 34);

        assert!(
            out.contains("github.com/EbbLabs/python-tidal"),
            "the source: {out}"
        );
        assert!(out.contains("credentials.json"), "the destination: {out}");
        assert!(out.contains("not priel's key"), "whose key it is: {out}");
        assert!(
            out.contains("not an official client"),
            "what priel is not: {out}"
        );
        assert!(out.contains("downloads it once"), "what it will do: {out}");
        assert!(
            out.contains("do it yourself"),
            "the manual alternative: {out}"
        );
        assert!(out.contains("[f]") && out.contains("[Esc]") && out.contains("[q]"));
    }

    #[test]
    fn the_consent_screens_choices_are_painted_where_a_click_lands() {
        // Goal: this screen appears before anything else priel does, and it had
        // three actions and no way to point at any of them. Each key is a
        // control on the cells it printed, and declining runs the same shared
        // method `Esc` does.
        let mut sc = screen();
        sc.app.set_mode_for_test(Mode::Credentials);
        for (hit, key) in [
            (Hit::FetchCredentials, "f"),
            (Hit::DeclineCredentials, "Esc"),
            (Hit::Quit, "q"),
        ] {
            assert_eq!(painted(&mut sc.app, 100, 30, hit), key);
        }
        click_hit(&mut sc.app, Hit::DeclineCredentials);
        assert_eq!(sc.app.mode, Mode::Normal, "declining continues without it");
    }

    #[test]
    fn the_consent_screen_takes_the_hit_boxes_over_from_the_row_behind_it() {
        // Goal: modal means a click cannot reach a control underneath. The header
        // is drawn first, so its controls have to be replaced by this screen's.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 100, 30);
        let header_row = sc
            .app
            .hits
            .iter()
            .map(|(r, _)| r.y)
            .min()
            .expect("the header should publish controls");
        sc.app.set_mode_for_test(Mode::Credentials);
        let _ = draw(&mut sc.app, 100, 30);
        assert!(
            !sc.app.hits.iter().any(|(r, _)| r.y == header_row),
            "nothing behind the consent screen may still be clickable"
        );
    }

    #[test]
    fn the_consent_screen_fits_a_modest_terminal() {
        // Goal: a wall of text that overflows is a wall of text nobody reads.
        for (w, h) in [(80u16, 30u16), (96, 34), (120, 40)] {
            let mut sc = screen();
            sc.app.set_mode_for_test(Mode::Credentials);
            let out = text(&mut sc.app, w, h);
            assert!(out.contains("client identity"), "{w}x{h}: {out}");
        }
    }

    /// An app mid sign-in, using the real credentials path so the flow builds.
    fn signing_in(sc: &mut Screen) {
        sc.app.set_paths_for_test(
            "/nonexistent/priel/token.json".into(),
            credentials_fixture(),
        );
        sc.app.start_login();
    }

    /// A throwaway credentials file, so the login screen can be built without
    /// depending on whatever is configured on the machine.
    fn credentials_fixture() -> String {
        let dir = std::env::temp_dir().join(format!(
            "priel-login-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"client_id":"cid","client_secret":"sec"}"#).expect("write");
        path.to_str().expect("path").to_string()
    }

    #[test]
    fn the_sign_in_screens_actions_are_painted_where_a_click_lands() {
        // Goal: four actions, none of which could be pointed at. Each is a
        // control on the cells its key printed, and cancelling runs the same
        // shared method `Esc` does.
        let mut sc = screen();
        signing_in(&mut sc);
        for (hit, key) in [
            (Hit::SubmitLogin, "Enter"),
            (Hit::ReopenBrowser, "Ctrl-O"),
            (Hit::ClearPaste, "Ctrl-U"),
            (Hit::CancelLogin, "Esc"),
        ] {
            assert_eq!(painted(&mut sc.app, 88, 24, hit), key);
        }
        click_hit(&mut sc.app, Hit::CancelLogin);
        assert_eq!(sc.app.mode, Mode::Normal);
        assert!(sc.app.login().is_none(), "and drops the flow with it");
    }

    #[test]
    fn the_sign_in_screen_takes_the_hit_boxes_over_from_the_row_behind_it() {
        // Goal: modal means a click cannot reach a control underneath, and this
        // screen covers a header whose controls were registered before it drew.
        let mut sc = screen();
        let _ = draw(&mut sc.app, 88, 24);
        let header_row = sc
            .app
            .hits
            .iter()
            .map(|(r, _)| r.y)
            .min()
            .expect("the header should publish controls");
        signing_in(&mut sc);
        let _ = draw(&mut sc.app, 88, 24);
        assert!(
            !sc.app.hits.iter().any(|(r, _)| r.y == header_row),
            "nothing behind the sign-in screen may still be clickable"
        );
    }

    #[test]
    fn the_sign_in_screen_explains_the_page_that_looks_broken() {
        // Goal: the redirect lands on a page that reads as an error, and a user
        // who is not warned will assume the sign-in failed and give up. Saying
        // so up front is the single most valuable line on this screen.
        let mut sc = screen();
        signing_in(&mut sc);
        let out = text(&mut sc.app, 88, 24);
        assert!(out.contains("looks like an error"), "{out}");
        assert!(out.contains("That is"), "and that it is expected: {out}");
        assert!(out.contains("paste"), "{out}");
        assert!(out.contains("[Enter]") && out.contains("[Esc]"), "{out}");
    }

    #[test]
    fn a_pasted_url_shows_its_tail_rather_than_its_head() {
        // Goal: the URL is far wider than the box, and the useful part - the
        // code - is at the end. Showing the head would look identical for a
        // successful paste and a wrong one.
        let mut sc = screen();
        signing_in(&mut sc);
        sc.app.on_paste(&format!(
            "https://tidal.com/android/login/auth?{}code=ENDCODE",
            "x".repeat(200)
        ));
        let out = text(&mut sc.app, 88, 24);
        assert!(out.contains("ENDCODE"), "the tail must be visible: {out}");
    }

    #[test]
    fn a_bad_paste_is_explained_without_losing_the_screen() {
        // Goal: the likeliest mistake is pasting the login page instead of the
        // page it redirected to. That has to be recoverable in place.
        let mut sc = screen();
        signing_in(&mut sc);
        sc.app.on_paste("https://login.tidal.com/authorize");
        sc.app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        let out = text(&mut sc.app, 88, 24);
        assert!(out.contains("query string"), "the reason: {out}");
        assert!(
            out.contains("[Enter]"),
            "still on the sign-in screen: {out}"
        );
    }

    // ---- measurement harness ----
    //
    // Outside the normal suite: it times thousands of frames. Run deliberately:
    //   cargo test -p priel-tui -- --ignored --nocapture measure_
    //
    // What it settled is written up in
    // `docs/adr/0002-a-display-of-the-audio-costs-the-gapless-transition.md`.

    #[test]
    #[ignore = "times thousands of frames"]
    fn measure_what_redrawing_on_every_tick_would_cost() {
        // Goal: `run` draws only when the app says something changed, because
        // priel runs for hours in front of a screen that mostly does not move.
        // Anything driven by the audio marks the screen changed on every tick
        // for as long as a track plays, so this is the saving that would be
        // handed back. Each frame is built from nothing, which is what a screen
        // that changed everywhere amounts to; the terminal write is not
        // included, so the number is a floor rather than the whole bill.
        const FRAMES: u32 = 500;
        let mut sc = screen();
        sc.app.favorites = (1..=500)
            .map(|id| track(id, "A track with a plausible sort of title"))
            .collect();
        let started = std::time::Instant::now();
        for _ in 0..FRAMES {
            let mut term = Terminal::new(TestBackend::new(120, 40)).expect("backend");
            term.draw(|f| render(f, &mut sc.app)).expect("render");
        }
        let per_frame = started.elapsed().as_secs_f64() / f64::from(FRAMES);
        println!(
            "MEASURE a full 120x40 frame costs {:.0} us; at the 10 fps the event loop allows that \
             is {:.1}% of one core, at 30 fps {:.1}%",
            per_frame * 1e6,
            per_frame * 10.0 * 100.0,
            per_frame * 30.0 * 100.0
        );
    }

    // ---- editing playlists ----

    fn with_two_playlists(sc: &mut Screen) {
        sc.app.playlists = vec![
            priel_core::Playlist {
                uuid: "a1".into(),
                title: "Morning".into(),
                num_tracks: 12,
                duration_secs: 900,
            },
            priel_core::Playlist {
                uuid: "b2".into(),
                title: "Evening".into(),
                num_tracks: 3,
                duration_secs: 400,
            },
        ];
        sc.app.view = View::Playlists;
    }

    #[test]
    fn the_confirmation_names_what_it_will_destroy_and_says_it_is_final() {
        // Goal: this is the one screen where saying yes cannot be walked back,
        // so the question has to be checkable by the person answering it. A
        // uuid, an index, or a bare "are you sure" would all be answered out of
        // habit.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        press(&mut sc.app, 'X');
        let out = text(&mut sc.app, 90, 26);
        assert!(out.contains("Morning"), "it names the playlist: {out}");
        assert!(
            out.contains("no way to bring it back"),
            "and says it is final: {out}"
        );
        assert!(
            out.contains("delete it"),
            "the control says what it does, not OK: {out}"
        );
        assert!(out.contains("keep it"), "and so does the other one: {out}");
    }

    #[test]
    fn the_confirmations_controls_are_clickable_exactly_where_they_were_painted() {
        // Goal: the hit box and the painted cells are built in one walk, so a
        // click cannot land on "delete it" while the pointer is over "keep it".
        // On this screen that difference is a playlist.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        press(&mut sc.app, 'X');
        assert_eq!(painted(&mut sc.app, 90, 26, Hit::ConfirmYes), "y");
        assert_eq!(painted(&mut sc.app, 90, 26, Hit::ConfirmNo), "n");
    }

    #[test]
    fn a_confirmation_too_short_to_draw_its_controls_registers_none() {
        // Goal: a control clipped off the box must not still answer to a click
        // at the place it would have been - which on this screen would be a
        // delete nobody could see they were agreeing to.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        press(&mut sc.app, 'X');
        let _ = text(&mut sc.app, 90, 5);
        assert!(
            !sc.app.hits.iter().any(|(_, h)| *h == Hit::ConfirmYes),
            "nothing was painted, so nothing may be clicked"
        );
    }

    #[test]
    fn the_name_prompt_shows_what_has_been_typed() {
        // Goal: it is a text box, and a text box that does not echo is one
        // nobody can check before pressing Enter.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        press(&mut sc.app, 'N');
        for c in "Late night".chars() {
            press(&mut sc.app, c);
        }
        let out = text(&mut sc.app, 90, 26);
        assert!(out.contains("Name the new playlist"), "{out}");
        assert!(out.contains("Late night"), "{out}");
        assert_eq!(painted(&mut sc.app, 90, 26, Hit::SubmitPrompt), "Enter");
    }

    #[test]
    fn the_picker_lists_the_playlists_a_track_can_go_into() {
        // Goal: the choice has to be readable before it is made, and a row's hit
        // box has to be the row that was drawn - adding a track to the wrong
        // playlist is a mess somebody has to clean up by hand.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        sc.app.view = View::Favorites;
        sc.app.favorites = vec![track(1, "One")];
        press(&mut sc.app, 'a');
        let out = text(&mut sc.app, 90, 26);
        assert!(out.contains("Add to playlist"), "{out}");
        assert!(out.contains("Morning"), "{out}");
        assert!(out.contains("Evening"), "{out}");
        assert!(
            out.contains("Enter add"),
            "it says what choosing does: {out}"
        );

        let rows = sc.app.add_rows.clone();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1].1, 1,
            "the second box stands for the second playlist"
        );
    }

    #[test]
    fn the_picker_says_it_is_still_fetching_rather_than_showing_an_empty_list() {
        // Goal: somebody who has never opened the playlists tab has none loaded,
        // and an empty picker would read as "you have no playlists".
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "One")];
        press(&mut sc.app, 'a');
        let out = text(&mut sc.app, 90, 26);
        assert!(out.contains("Fetching your playlists"), "{out}");
        assert!(sc.app.add_rows.is_empty(), "and nothing to click yet");
    }

    #[test]
    fn a_modal_of_this_feature_covers_the_list_behind_it() {
        // Goal: all three are modal. What is behind must not show through, and
        // - more to the point - the header's controls registered earlier in the
        // frame must not still be reachable.
        let mut sc = screen();
        with_two_playlists(&mut sc);
        sc.app.favorites = vec![track(1, "Hidden Title")];
        for (open, name) in [('N', "prompt"), ('X', "confirmation")] {
            sc.app.view = View::Playlists;
            sc.app.mode = Mode::Normal;
            press(&mut sc.app, open);
            let out = text(&mut sc.app, 90, 26);
            assert!(!out.contains("Hidden Title"), "the {name} covers it: {out}");
            assert!(
                !sc.app.hits.iter().any(|(_, h)| *h == Hit::Quit),
                "and nothing behind the {name} is clickable"
            );
        }
    }

    // ---- readability: alignment, truncation and one spelling per fact ----

    /// The leading blanks on a line, which is where its content starts.
    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    #[test]
    fn the_output_badge_starts_in_the_same_column_whether_or_not_one_is_open() {
        // Goal: the bottom row must not move when an output opens.
        // `device_readout` returned a leading space in one case and not in the
        // other, so the verdict, the activity slot and every key hint - and
        // every one of their hit boxes - stepped one cell right the moment
        // playback started.
        // Method: render both states and compare where the row's content
        // begins.
        let mut sc = screen();
        let idle = draw(&mut sc.app, 130, 12);
        let idle_row = idle.last().cloned().unwrap_or_default();
        chain(&mut sc, 24, 96_000, 96_000, "s32");
        let open = draw(&mut sc.app, 130, 12);
        let open_row = open.last().cloned().unwrap_or_default();
        assert_eq!(
            indent(&idle_row),
            indent(&open_row),
            "idle: {idle_row}\nopen: {open_row}"
        );
    }

    #[test]
    fn a_row_names_the_quality_the_way_the_badge_does() {
        // Goal: one fact, one spelling. The wire says `HI_RES_LOSSLESS`; the
        // badge beside the playing track says `HI-RES`; the list said
        // `HI_RES_…` because the raw token did not fit its own column, so the
        // same track was named two ways two lines apart.
        let mut sc = screen();
        let mut t = track(1, "T");
        t.quality = "HI_RES_LOSSLESS".into();
        favorites_arrive(&mut sc, vec![t]);
        let out = text(&mut sc.app, 120, 10);
        assert!(out.contains("HI-RES"), "{out}");
        assert!(
            !out.contains("HI_RES"),
            "the wire token must not reach a row: {out}"
        );
    }

    #[test]
    fn the_source_badge_is_joined_on_with_an_even_separator() {
        // Goal: the separator between the title and the badge carried three
        // spaces on its left and two on its right, where every separator inside
        // the badge carries one of each. An uneven gap around one glyph reads
        // as a misalignment rather than as a grouping.
        let mut sc = screen();
        sc.app.now_playing = Some(track(1, "So What"));
        sc.app.now_meta = crate::app::StreamMeta {
            bit_depth: 24,
            sample_rate: 192_000,
            codec: "flac".into(),
            quality: "HI_RES_LOSSLESS".into(),
        };
        let out = text(&mut sc.app, 130, 12);
        assert!(out.contains("  ·  24-bit · 192 kHz"), "{out}");
        assert!(
            !out.contains("   ·  24-bit"),
            "and no third space on the left: {out}"
        );
    }

    #[test]
    fn a_field_with_no_room_truncates_to_nothing_rather_than_overflowing() {
        // Goal: `trunc(s, 0)` returned a lone ellipsis, one cell wider than the
        // field it was asked to fit. Reachable from the device picker
        // (`body.width / 2`) and the add-to picker (`body.width - 14`) on a
        // narrow overlay, where one cell of overflow shifts the column beside
        // it.
        assert_eq!(super::trunc("anything", 0), "");
        assert_eq!(super::trunc("anything", 1), "…");
        assert_eq!(super::trunc("ab", 2), "ab");
    }

    /// Where a line's content starts, counted from the overlay's own left
    /// border rather than from the edge of the frame - the list behind an
    /// overlay paints the columns to its left, so a raw indent measures that
    /// instead.
    fn inside(line: &str, border: usize) -> usize {
        let after: String = line.chars().skip(border + 1).collect();
        after.len() - after.trim_start().len()
    }

    #[test]
    fn an_overlay_body_starts_where_its_own_footer_does() {
        // Goal: one left edge per box. The recent log and the device picker's
        // empty state sat against the border while their own footers were
        // indented two cells, so each of those boxes had a ragged left edge
        // that no other overlay has.
        // Method: find the box's own left border from its title row, then
        // measure the body and the footer from there.
        for (mode, title) in [(Mode::Log, "Recent log"), (Mode::Devices, "Output device")] {
            let name = format!("{mode:?}");
            let mut sc = screen();
            sc.app.set_mode_for_test(mode);
            let frame = draw(&mut sc.app, 100, 30);
            let border = frame
                .iter()
                .find(|l| l.contains(title))
                // The last corner on the row, not the first: the list behind
                // the overlay draws its own border further left.
                .and_then(|l| {
                    l.char_indices()
                        .rfind(|(_, c)| *c == '\u{250c}')
                        .map(|(b, _)| l[..b].chars().count())
                })
                .unwrap_or_default();
            let body = frame
                .iter()
                .find(|l| l.contains("Nothing recorded yet") || l.contains("No output devices"))
                .cloned()
                .unwrap_or_default();
            let footer = frame
                .iter()
                .find(|l| l.contains("Esc or q to close"))
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                inside(&body, border),
                inside(&footer, border),
                "{name}\nbody:   {body}\nfooter: {footer}"
            );
        }
    }

    #[test]
    fn every_overlay_says_the_way_out_the_same_way() {
        // Goal: five overlays print one closing instruction and one of them
        // used to say `press ?, Esc or q to close` where the rest said
        // `M, Esc or q to close`. A single stray verb is the kind of drift the
        // reader reads as a difference in meaning.
        for mode in [
            Mode::Help,
            Mode::Log,
            Mode::Graph,
            Mode::Devices,
            Mode::Themes,
        ] {
            let name = format!("{mode:?}");
            let mut sc = screen();
            sc.app.set_mode_for_test(mode);
            let out = text(&mut sc.app, 100, 40);
            assert!(out.contains(", Esc or q to close"), "{name}: {out}");
            assert!(!out.contains("press "), "{name} says it its own way: {out}");
        }
    }

    /// Display width, measured through ratatui rather than through the crate
    /// `ui` measures with: an independent oracle for the same number.
    fn drawn(s: &str) -> usize {
        ratatui::text::Span::raw(s).width()
    }

    #[test]
    fn truncation_cuts_at_a_cell_count_not_a_character_count() {
        // Goal: a CJK title paints two cells per character, so a field asked
        // for twelve cells was painting up to twenty-four and shifting every
        // column to its right off the grid. Measured with `Span::width`, which
        // is what ratatui itself draws with.
        let wide = "夜に駆ける夜に駆ける夜に駆ける夜に駆ける";
        let cut = super::trunc(wide, 12);
        assert!(
            drawn(&cut) <= 12,
            "{cut:?} paints {} cells in a 12-cell field",
            drawn(&cut)
        );
        assert!(cut.ends_with('…'), "a cut field says it was cut: {cut:?}");
    }

    #[test]
    fn a_string_that_fits_in_cells_is_left_alone() {
        // Goal: the other half of the same bug. Six wide characters are twelve
        // cells, so a twelve-cell field holds them whole - and a character
        // count would have kept sixteen of them.
        let six = "夜に駆ける夜";
        assert_eq!(drawn(six), 12);
        assert_eq!(super::trunc(six, 12), six);
        assert!(super::trunc(six, 11).ends_with('…'));
    }

    #[test]
    fn combining_marks_cost_no_cells_of_their_own() {
        // Goal: an accent is a separate character that paints on the letter
        // before it. Counting characters would cut a name in half for accents
        // that take no room at all.
        let accented = "Bjo\u{308}rk Guðmundsdóttir";
        assert!(accented.chars().count() > drawn(accented));
        assert_eq!(super::trunc(accented, drawn(accented)), accented);
    }

    #[test]
    fn a_two_cell_emoji_is_measured_as_two() {
        // Goal: a title with an emoji in it is ordinary in a catalogue, and it
        // paints two cells per glyph like any other wide character.
        let s = "🎵🎵🎵🎵";
        assert_eq!(drawn(&super::trunc(s, 5)), 5);
        assert!(drawn(&super::trunc(s, 4)) <= 4);
    }

    #[test]
    fn a_field_is_exactly_as_many_cells_as_it_was_asked_for() {
        // Goal: `format!("{:<32}")` pads by character count, so a padded CJK
        // title ran to sixty-four cells with thirty-two spaces after it. A
        // column that pads by cells cannot push the column beside it along.
        for s in ["Nude", "夜に駆ける夜に駆ける", "Bjo\u{308}rk", "🎵 mix", ""] {
            assert_eq!(drawn(&super::field(s, 20)), 20, "{s:?}");
            assert_eq!(drawn(&super::field(s, 6)), 6, "{s:?}");
        }
    }

    /// The box's own line: the top border, with the list title on it.
    fn title_line(app: &mut App, w: u16) -> String {
        draw(app, w, 12).remove(1)
    }

    #[test]
    fn the_list_title_carries_no_key_hints() {
        // Goal: the hints were about eighty-five cells in a seventy-eight cell
        // box, so an eighty-column terminal deleted a binding mid-word - the
        // failure class `push_hints` was hardened against, on the commonest
        // terminal width there is. They were also five different grammars for
        // what the bottom row and the `?` reference already say, and since the
        // reference became a complete clickable menu nothing is lost with them.
        let mut sc = screen();
        sc.app.playlists = vec![Playlist {
            uuid: "u".into(),
            title: "Deep Cuts".into(),
            num_tracks: 18,
            duration_secs: 60,
        }];
        sc.app.open_playlist = Some(("u".into(), "Deep Cuts".into()));
        sc.app.open_mix = Some(("m".into(), "My Mix 1".into()));
        sc.app.search_query = "blue".into();
        sc.app.search_tracks = vec![track(9, "Blue in Green")];
        for view in [
            View::Favorites,
            View::Playlists,
            View::PlaylistTracks,
            View::Mixes,
            View::MixTracks,
            View::Search,
        ] {
            sc.app.view = view;
            for mode in ["Normal", "Search"] {
                sc.app.mode = if mode == "Search" {
                    Mode::Search
                } else {
                    Mode::Normal
                };
                for w in [60u16, 80, 120] {
                    let line = title_line(&mut sc.app, w);
                    for hint in ["j/k", "Enter", "Esc", "(", "refresh", "shuffle"] {
                        assert!(
                            !line.contains(hint),
                            "{view:?} {mode} at {w}: the title still names {hint}: {line}"
                        );
                    }
                }
            }
        }
        sc.app.mode = Mode::Normal;
    }

    #[test]
    fn an_opened_playlist_names_the_list_it_was_opened_from() {
        // Goal: opening a playlist used to change one character at the far left
        // of the title and nothing else, so the tab strip highlighted the same
        // tab one level up and one level down. The title carries the trail.
        let mut sc = screen();
        sc.app.view = View::PlaylistTracks;
        sc.app.open_playlist = Some(("u".into(), "Deep Cuts".into()));
        sc.app.playlist_tracks = vec![track(1, "One"), track(2, "Two")];
        sc.app.playlist_tracks_paging.total = 18;
        let line = title_line(&mut sc.app, 120);
        assert!(
            line.contains("Playlists › Deep Cuts — 2 of 18 tracks"),
            "{line}"
        );
    }

    #[test]
    fn an_opened_mix_names_the_list_it_was_opened_from() {
        // Goal: the same trail from the other drill-down, so `Esc` has a
        // visible destination rather than one the user has to remember.
        let mut sc = screen();
        sc.app.view = View::MixTracks;
        sc.app.open_mix = Some(("m".into(), "My Mix 1".into()));
        sc.app.mix_tracks = vec![track(1, "One")];
        let line = title_line(&mut sc.app, 120);
        assert!(line.contains("Mixes › My Mix 1 — 1 tracks"), "{line}");
    }

    #[test]
    fn a_long_name_in_the_title_is_cut_so_the_count_survives() {
        // Goal: the title is the thing that was being clipped, and what a clip
        // takes is whatever is on the right - which is the count. Cut the name,
        // which the row below repeats, rather than the figure only this line
        // carries.
        let mut sc = screen();
        sc.app.view = View::PlaylistTracks;
        sc.app.open_playlist = Some((
            "u".into(),
            "A Playlist Whose Name Nobody Could Reasonably Have Expected To Fit".into(),
        ));
        sc.app.playlist_tracks = vec![track(1, "One")];
        sc.app.playlist_tracks_paging.total = 312;
        for w in [60u16, 80, 120] {
            let line = title_line(&mut sc.app, w);
            assert_eq!(drawn(&line), usize::from(w), "{w}: the box lost a cell");
            assert!(line.starts_with('┌') && line.ends_with('┐'), "{w}: {line}");
            assert!(
                line.contains("1 of 312 tracks"),
                "{w}: the count was clipped away: {line}"
            );
        }
    }

    #[test]
    fn a_query_being_typed_keeps_its_caret_however_long_it_is() {
        // Goal: the title doubles as the search box, so what a clip takes is
        // the caret and the characters just typed. A text field cut from the
        // front keeps the end, which is where the typing is.
        let mut sc = screen();
        sc.app.view = View::Search;
        sc.app.mode = Mode::Search;
        sc.app.search_query = "a very long query that nobody would type but which must not \
             lose the cursor off the right hand end of the box"
            .into();
        let line = title_line(&mut sc.app, 60);
        assert_eq!(drawn(&line), 60, "the box lost a cell");
        assert!(
            line.contains("box ▏") || line.contains("box▏"),
            "the caret and the last words typed are gone: {line}"
        );
        assert!(line.contains('…'), "the cut is not marked: {line}");
    }

    #[test]
    fn the_track_columns_drop_in_a_documented_order() {
        // Goal: the row is a budget, not a constant, and what it gives up as it
        // narrows is written down. Duration is not in this table because it is
        // never dropped: it is pinned to the right edge at every width.
        let cols = super::track_columns;

        // Widest: title, artist, album, quality.
        for w in [92, 118, 198] {
            let c = cols(w);
            assert!(c.album > 0, "{w}: the album is what the width is for");
            assert!(c.artist > 0, "{w}");
            assert_eq!(c.quality, 8, "{w}");
        }
        // The album goes first.
        for w in [54, 78, 91] {
            let c = cols(w);
            assert_eq!(c.album, 0, "{w}: the album is the first column to go");
            assert!(c.artist > 0, "{w}: the artist outlives the album");
            assert_eq!(c.quality, 8, "{w}");
        }
        // Then the artist.
        for w in [38, 53] {
            let c = cols(w);
            assert_eq!(c.artist, 0, "{w}");
            assert_eq!(c.quality, 8, "{w}: the tier outlives the artist");
        }
        // Then the quality tier. The title and the duration are all that is left.
        for w in [20, 37] {
            let c = cols(w);
            assert_eq!(c.quality, 0, "{w}");
            assert!(c.title > 0, "{w}: the title is never dropped");
        }
    }

    #[test]
    fn a_column_that_is_drawn_is_wide_enough_to_read() {
        // Goal: the alternative to dropping a column is shaving every column
        // into uselessness, which is the failure this replaces. A column either
        // has room to say something or it is not there.
        for w in 12..=240usize {
            let c = super::track_columns(w);
            assert!(c.title >= 8 || w < 24, "{w}: {c:?}");
            assert!(c.artist == 0 || c.artist >= 12, "{w}: {c:?}");
            assert!(c.album == 0 || c.album >= 18, "{w}: {c:?}");
            assert!(
                c.album == 0 || c.artist > 0,
                "{w}: an album without an artist is out of order: {c:?}"
            );
            assert!(
                c.artist == 0 || c.quality > 0,
                "{w}: an artist without a tier is out of order: {c:?}"
            );
        }
    }

    #[test]
    fn a_row_fills_the_width_it_was_given_and_never_overruns_it() {
        // Goal: the duration is pinned right, which is only true if the row is
        // exactly as wide as the box. One cell over and the box eats it; one
        // cell under and the column of times stops lining up.
        let mut sc = screen();
        sc.app.favorites = vec![
            track(1, "Nude"),
            Track {
                id: 2,
                title: "夜に駆ける夜に駆ける夜に駆ける夜に駆ける".into(),
                artist: "Some Extremely Long Artist Name Indeed".into(),
                album: "An Album With A Rather Long Name Too".into(),
                duration_secs: 3671,
                quality: "LOSSLESS".into(),
                ..Track::default()
            },
        ];
        for w in 12..=240usize {
            for vi in 0..2 {
                let (text, _) = super::row_text(&sc.app, &[0, 1], vi, w);
                assert_eq!(drawn(&text), w, "row {vi} at {w}: {text:?}");
            }
        }
    }

    #[test]
    fn a_narrow_list_keeps_the_quality_and_the_duration() {
        // Goal: at sixty columns the fixed block ran past the right-hand edge
        // and the two right-most columns were silently clipped away. Nothing
        // told the user they existed.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Everything In Its Right Place")];
        let out = text(&mut sc.app, 60, 20);
        assert!(out.contains("HI-RES"), "the tier was clipped away: {out}");
        assert!(out.contains("4:05"), "the duration was clipped away: {out}");
    }

    #[test]
    fn a_wide_list_spends_the_width_on_the_album_rather_than_on_blanks() {
        // Goal: at two hundred columns a hundred and sixteen cells of every row
        // were blank while `Track::album` was fetched, stored and never drawn.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Nude")];
        let wide = text(&mut sc.app, 200, 20);
        assert!(wide.contains("Album"), "the album is not drawn: {wide}");
        let narrow = text(&mut sc.app, 80, 20);
        assert!(
            !narrow.contains("Album"),
            "the album should have been dropped first: {narrow}"
        );
    }

    #[test]
    fn the_duration_is_pinned_to_the_right_edge_at_every_width() {
        // Goal: `number-tabular` - a column of times is only scannable when the
        // digits line up, and a duration floating at column 66 in a 198-cell
        // box is not a column at all.
        let mut sc = screen();
        sc.app.favorites = vec![track(1, "Nude"), track(2, "Weird Fishes")];
        for w in [60u16, 80, 120, 200] {
            let first = column_of(&mut sc.app, w, 20, 2, "4:05");
            let second = column_of(&mut sc.app, w, 20, 3, "4:05");
            assert_eq!(first, second, "{w}: the two durations are not a column");
            // The box's own right-hand border sits at `w - 1`, so the last cell
            // a row may paint is `w - 2` and a four-digit time starts three
            // cells before it.
            assert_eq!(
                first,
                Some(w - 5),
                "{w}: the duration is not against the right-hand edge"
            );
        }
    }

    /// The column an ASCII needle starts at on row `y`, scanned cell by cell.
    ///
    /// A character index into a joined line is not a column: a wide glyph fills
    /// one cell and leaves the next one blank, so the two disagree by one per
    /// wide glyph - which is the whole bug under test.
    fn column_of(app: &mut App, w: u16, h: u16, y: u16, needle: &str) -> Option<u16> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("backend");
        term.draw(|f| render(f, app)).expect("render");
        let buf = term.backend().buffer().clone();
        (0..w).find(|x| {
            (*x..w)
                .map(|i| buf[(i, y)].symbol())
                .collect::<String>()
                .starts_with(needle)
        })
    }

    #[test]
    fn a_wide_title_leaves_the_columns_beside_it_where_they_were() {
        // Goal: read off a rendered frame, not asserted in the abstract. A CJK
        // title is ordinary in a catalogue and paints two cells per character,
        // and the row carrying one used to push the artist, the quality and the
        // duration rightwards - off the edge of the box on a narrow terminal.
        let mut sc = screen();
        sc.app.favorites = vec![
            track(1, "Nude"),
            Track {
                id: 2,
                title: "夜に駆ける夜に駆ける夜に駆ける夜に駆ける".into(),
                artist: "Artist".into(),
                quality: "HI-RES".into(),
                duration_secs: 245,
                ..Track::default()
            },
        ];
        let plain = column_of(&mut sc.app, 80, 24, 2, "HI-RES");
        let wide = column_of(&mut sc.app, 80, 24, 3, "HI-RES");
        assert!(plain.is_some(), "the plain row lost its quality column");
        assert_eq!(wide, plain, "the wide row's quality column moved");
    }

    #[test]
    fn a_field_with_no_room_holds_nothing() {
        // Goal: a zero-width field once returned the ellipsis on its own, one
        // cell wider than the field it was asked to fit.
        assert_eq!(super::trunc("Nude", 0), "");
        assert_eq!(super::field("Nude", 0), "");
    }
}
