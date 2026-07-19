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
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

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
    let l0 = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let l1 = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    let l2 = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: 1,
    };
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
    let hint = hints(bar.remaining());
    bar.label(hint, Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(Line::from(bar.spans)), l2);
}

/// Keyboard reference tiers, widest first.
///
/// priel is keyboard-first, so this row is not decoration - it is how bindings
/// are discovered. Letting the renderer clip it silently drops the keys on the
/// right, which is how `[q] quit` once vanished from a narrow terminal. Pick the
/// widest tier that fits instead, so the most important keys always survive.
const HINT_TIERS: [&str; 4] = [
    "[space] play  [h/l] seek  [H/L] skip  [j/k] move  [g/G] ends  [s] shuffle  [+/-] vol  [/] filter  [Tab] view  [q] quit",
    "[space] play  [h/l] seek  [H/L] skip  [j/k] move  [s] shuffle  [+/-] vol  [/] filter  [q] quit",
    "[space] play  [h/l] seek  [H/L] skip  [s] shuffle  [+/-] vol  [q] quit",
    "[space] [h/l] [H/L] [s] [+/-] [/] [q]",
];

fn hints(available: u16) -> &'static str {
    HINT_TIERS
        .iter()
        .copied()
        .find(|h| u16::try_from(h.chars().count()).unwrap_or(u16::MAX) <= available)
        .unwrap_or("")
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
    use super::{ControlBar, HINT_TIERS, button_style, hints};
    use crate::app::Hit;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

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
    fn hints_pick_the_widest_tier_that_fits() {
        // Goal: the bottom row must never be clipped by the renderer. Every tier
        // has to fit the width it is chosen for, and a wide row gets the fullest.
        for available in [0u16, 20, 40, 60, 80, 100, 140, 200] {
            let h = hints(available);
            assert!(
                h.chars().count() <= available as usize,
                "tier {h:?} does not fit in {available} cells"
            );
        }
        assert_eq!(
            hints(200),
            HINT_TIERS[0],
            "a wide row gets the full reference"
        );
        assert_eq!(hints(0), "", "nothing fits in no space");
    }

    #[test]
    fn every_hint_tier_keeps_quit_reachable() {
        // Goal: quit is the one binding a user cannot guess their way out of
        // without. No tier may drop it, however narrow.
        for tier in HINT_TIERS {
            assert!(tier.contains("[q]"), "tier {tier:?} dropped the quit hint");
        }
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
}
