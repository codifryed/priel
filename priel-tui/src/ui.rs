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

use crate::app::{App, Hit, Mode, View};

pub fn render(f: &mut Frame, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header / tabs
        Constraint::Min(1),    // list
        Constraint::Length(3), // now-playing
    ])
    .split(f.area());

    // Hit boxes are geometry, so the renderer owns them. Rebuilt every frame.
    app.hits.clear();
    header(f, app, rows[0]);
    list(f, app, rows[1]);
    now_playing(f, app, rows[2]);

    // Drawn last so it sits over everything, and after the hit boxes above have
    // been registered - `App` ignores them while the overlay is up.
    if app.mode == Mode::Help {
        help_overlay(f, f.area());
    }
}

/// The complete reference, in two columns. The bottom row carries only what is
/// used constantly; everything else is discoverable from here, which is what
/// keeps that row short enough to survive a narrow terminal.
const HELP_LEFT: &[(&str, &[(&str, &str)])] = &[
    (
        "Views",
        &[
            ("Tab", "cycle views"),
            ("1 2 3", "jump to a view"),
            ("Enter", "open playlist"),
            ("Esc", "back to playlists"),
        ],
    ),
    (
        "Move",
        &[
            ("j k", "move the selection"),
            ("Down Up", "move the selection"),
            ("J K", "page down / up"),
            ("Ctrl-D Ctrl-U", "half page down / up"),
            ("g G", "first / last row"),
        ],
    ),
];

const HELP_RIGHT: &[(&str, &[(&str, &str)])] = &[
    (
        "Play",
        &[
            ("Enter", "play selected track"),
            ("space", "play / pause"),
            ("h l", "seek -5s / +5s"),
            ("L H", "next / previous track"),
            ("n p", "next / previous track"),
            ("s", "shuffle this view"),
            ("+ -", "volume up / down"),
        ],
    ),
    (
        "Find",
        &[
            ("/", "filter this list"),
            ("i", "edit search query"),
            ("Enter Esc", "accept / cancel"),
        ],
    ),
    (
        "Mouse",
        &[
            ("wheel", "move the selection"),
            ("double-click", "play a row"),
            ("click drag", "seek on progress bar"),
            ("click", "header and hint keys"),
        ],
    ),
];

fn help_lines(sections: &[(&str, &[(&str, &str)])]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (title, rows) in sections {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            (*title).to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for (keys, what) in *rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys:<14}"), Style::default().fg(Color::White)),
                Span::styled((*what).to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    lines
}

fn help_overlay(f: &mut Frame, area: Rect) {
    let left = help_lines(HELP_LEFT);
    let right = help_lines(HELP_RIGHT);
    let width = area.width.min(84);
    // Two columns need a 14-cell key field plus a description, twice over.
    // Below that, stacking beats clipping every description in half.
    let stacked = width.saturating_sub(2) < 76;
    let rows = if stacked {
        u16::try_from(left.len() + right.len() + 1).unwrap_or(u16::MAX)
    } else {
        u16::try_from(left.len().max(right.len())).unwrap_or(u16::MAX)
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
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Keyboard and mouse ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(Rect {
            height: inner.height.saturating_sub(1),
            ..inner
        });
    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(Paragraph::new(right), cols[1]);

    if inner.height > 0 {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  press ?, Esc or q to close",
                Style::default().fg(Color::DarkGray),
            ))),
            Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            },
        );
    }
}

fn tab_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// A transport control. Rendered as a glyph on a raised background rather than
/// bracketed text: brackets read as punctuation to scan past, a filled block
/// reads as something to click.
///
/// The glyphs are deliberately the *white* triangles (U+25C1/U+25B7) and not the
/// media-player codepoints (U+23EE..U+23EF). The latter have emoji presentation,
/// so a terminal with an emoji font paints them two cells wide while
/// unicode-width calls them one - and every control to their right would then sit
/// one cell away from its own hit box.
fn button_style() -> Style {
    Style::default().fg(Color::Cyan).bg(Color::DarkGray)
}

fn toggle_style(on: bool) -> Style {
    if on {
        Style::default().fg(Color::Black).bg(Color::Green)
    } else {
        Style::default().fg(Color::Gray).bg(Color::DarkGray)
    }
}

/// Tabs on the left, transport controls next to them, then status. Controls live
/// up here because this is the row the eye already tracks; the bottom row stays
/// the keyboard reference.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display-only: volume percent is non-negative and rendered whole"
)]
fn header(f: &mut Frame, app: &mut App, area: Rect) {
    let in_playlists = matches!(app.view, View::Playlists | View::PlaylistTracks);
    let tabs = [
        ("1 Favorites", View::Favorites, app.view == View::Favorites),
        ("2 Playlists", View::Playlists, in_playlists),
        ("3 Search", View::Search, app.view == View::Search),
    ];
    // Read what the bar needs up front: it borrows `app` mutably to record hits.
    let playing = app.status.playing;
    let shuffle = app.shuffle;
    let volume = app.status.volume as u32;
    let queue = app.queue_indicator();
    let filtering = app.mode == Mode::Filter;
    let filter = app.filter.clone();
    let notice = app.notice.clone();
    let dim = Style::default().fg(Color::DarkGray);

    let mut bar = ControlBar::new(area);
    for (label, view, active) in tabs {
        bar.button(format!(" {label} "), Hit::View(view), tab_style(active));
    }

    bar.label("   ", Style::default());
    bar.button(" |◁ ", Hit::Prev, button_style());
    // A control shows the action it performs, not the state it is in.
    bar.button(
        if playing { " ‖ " } else { " ▷ " },
        Hit::PlayPause,
        button_style(),
    );
    bar.button(" ▷| ", Hit::Next, button_style());
    bar.label(" ", Style::default());
    bar.button(" ⇄ ", Hit::Shuffle, toggle_style(shuffle));
    bar.label(" ", Style::default());
    bar.button(" - ", Hit::VolDown, button_style());
    bar.label(format!(" {volume}% "), dim);
    bar.button(" + ", Hit::VolUp, button_style());
    bar.label("  ", Style::default());

    if let Some(q) = queue {
        bar.label(format!("queue {q}  "), Style::default().fg(Color::Blue));
    }
    if filtering {
        bar.label(format!("/{filter}"), Style::default().fg(Color::Yellow));
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
    let vis = app.visible();
    let title = list_title(app, vis.len());
    let block = Block::default().borders(Borders::ALL).title(title);
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
        let (text, is_now) = row_text(app, &vis, vi);
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else if is_now {
            Style::default().fg(Color::Green)
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

fn list_title(app: &App, count: usize) -> String {
    match app.view {
        View::Favorites => format!(
            "Favorites — {count} tracks   (Tab views · j/k move · Enter play · / filter · s shuffle)"
        ),
        View::Playlists => format!("Playlists — {count}   (Enter to open · j/k move)"),
        View::PlaylistTracks => {
            let name = app.open_playlist.as_ref().map_or("", |(_, t)| t.as_str());
            format!("▸ {name} — {count} tracks   (Esc back · Enter play)")
        }
        View::Search => {
            if app.mode == Mode::Search {
                format!(
                    "Search: {}▏   (Enter to search · Esc cancel)",
                    app.search_query
                )
            } else if app.search_query.is_empty() {
                "Search   (i or type to search TIDAL)".to_string()
            } else {
                format!(
                    "Search: {} — {count} results   (i to edit)",
                    app.search_query
                )
            }
        }
    }
}

/// Returns (rendered row text, `is_now_playing`).
///
/// `visible` is passed in rather than recomputed: this runs once per rendered
/// row, and rebuilding the index list here made rendering O(rows x tracks).
fn row_text(app: &App, visible: &[usize], vi: usize) -> (String, bool) {
    let idx = visible[vi];
    if app.view == View::Playlists {
        if let Some(p) = app.playlists.get(idx) {
            return (
                format!(
                    "  {:<44} {:>4} tracks   {}",
                    trunc(&p.title, 44),
                    p.num_tracks,
                    fmt_hms(p.duration)
                ),
                false,
            );
        }
        return (String::new(), false);
    }
    let tracks = app.current_tracks();
    if let Some(t) = tracks.get(idx) {
        let is_now = app.now_playing.as_ref().is_some_and(|n| n.id == t.id);
        let mark = if is_now { "♪ " } else { "  " };
        (
            format!(
                "{mark}{:<32} {:<20} {:<8}{:>6}",
                trunc(&t.title, 32),
                trunc(&t.artist, 20),
                trunc(&t.quality, 8),
                fmt_dur(t.duration),
            ),
            is_now,
        )
    } else {
        (String::new(), false)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display-only: seconds and volume percent are non-negative and rendered whole"
)]
fn now_playing(f: &mut Frame, app: &mut App, area: Rect) {
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
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {state} "), Style::default().fg(Color::Cyan)),
            Span::raw(title),
            Span::styled(source_badge(app), Style::default().fg(Color::DarkGray)),
        ])),
        l0,
    );

    let ratio = if s.duration > 0.0 {
        (s.position / s.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    f.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
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
    let mut bar = ControlBar::new(l2);
    bar.label(dac_badge(&app.status), Style::default().fg(Color::Green));
    bar.label(act_text, Style::default().fg(act_color));
    bar.label("  ", Style::default());
    push_hints(&mut bar);
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

fn push_hint(bar: &mut ControlBar, h: &Hint) {
    let dim = Style::default().fg(Color::DarkGray);
    bar.label("[", dim);
    for (i, (key, hit)) in h.keys.iter().enumerate() {
        if i > 0 {
            bar.label("/", dim);
        }
        bar.button(*key, *hit, Style::default().fg(Color::Cyan));
    }
    bar.label(format!("] {}  ", h.label), dim);
}

/// Fill the row with hints, reserving room for the essential ones so they are
/// never the ones clipped off the right edge.
fn push_hints(bar: &mut ControlBar) {
    let reserved: u16 = HINTS_ESSENTIAL.iter().map(hint_width).sum();
    for h in HINTS {
        if bar.remaining() < hint_width(h).saturating_add(reserved) {
            break;
        }
        push_hint(bar, h);
    }
    for h in HINTS_ESSENTIAL {
        if bar.remaining() >= hint_width(h) {
            push_hint(bar, h);
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
    let (text, color) = if app.is_resolving() {
        (format!("{} resolving…", app.spinner()), Color::Yellow)
    } else if app.is_buffering() {
        (format!("{} buffering…", app.spinner()), Color::Yellow)
    } else if app.status.loaded && app.status.cache_secs >= 1.0 {
        let c = if app.status.cache_secs >= 10.0 {
            Color::Green
        } else {
            Color::Yellow
        };
        (format!("⤓ {}s buffered", app.status.cache_secs as u32), c)
    } else {
        (String::new(), Color::DarkGray)
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
        parts.push(format!("{} kHz", m.sample_rate / 1000));
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
        format!("   ·  {}", parts.join(" · "))
    }
}

fn dac_badge(s: &priel_player::PlaybackStatus) -> String {
    if s.sample_rate == 0 && s.out_format.is_empty() {
        return " DAC —".into();
    }
    let fmt = if s.out_format.is_empty() {
        "?".into()
    } else {
        s.out_format.to_uppercase()
    };
    let rate = if s.sample_rate > 0 {
        format!("{} kHz", s.sample_rate / 1000)
    } else {
        "?".into()
    };
    format!(" DAC {fmt} · {rate}")
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

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut r: String = s.chars().take(n.saturating_sub(1)).collect();
        r.push('…');
        r
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
    use super::{ControlBar, HINTS, HINTS_ESSENTIAL, button_style, hint_width, push_hints, render};
    use crate::app::{App, Hit, Mode, View};
    use crate::worker::{FromWorker, ToWorker};
    use priel_core::{Playlist, Track};
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::mpsc::{Receiver, Sender};

    /// Holds the worker channel ends alongside the app: dropping them would
    /// disconnect the channels mid-test.
    struct Screen {
        app: App,
        #[allow(dead_code, reason = "held to keep the worker channels alive")]
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
            duration: 245,
            quality: "HI-RES".into(),
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

    #[test]
    fn hit_boxes_tile_the_row_without_gaps_or_overlap() {
        // Goal: the hit box of every control must start exactly where the
        // previous span ended. A gap or overlap here is a click landing on the
        // wrong control, which is invisible in a screenshot.
        let mut bar = ControlBar::new(row(80));
        bar.button(" a ", Hit::Prev, button_style());
        bar.label("--", Style::default());
        bar.button(" bb ", Hit::Next, button_style());

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
        bar.button(" |◁ ", Hit::Prev, button_style());
        bar.button(" ▷| ", Hit::Next, button_style());

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
            super::push_hint(&mut bar, h);
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
            push_hints(&mut bar);
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
        push_hints(&mut bar);
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
        bar.button(" aaaa ", Hit::Prev, button_style());
        bar.button(" bbbb ", Hit::Next, button_style());
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
            duration: 3725,
        }];
        sc.app.view = View::Playlists;
        let out = text(&mut sc.app, 120, 12);
        assert!(out.contains("Evening"), "{out}");
        assert!(out.contains("12"), "{out}");
        assert!(out.contains("1:02:05"), "hours format: {out}");
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
        assert!(out.contains("DAC"), "{out}");
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
            out.contains("Mouse"),
            "the mouse gestures are documented: {out}"
        );
        assert!(
            !out.contains("Hidden Title"),
            "the list should be covered: {out}"
        );
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
}
