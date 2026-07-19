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

use crate::app::{App, Mode, View};

pub fn render(f: &mut Frame, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header / tabs
        Constraint::Min(1),    // list
        Constraint::Length(3), // now-playing
    ])
    .split(f.area());

    header(f, app, rows[0]);
    list(f, app, rows[1]);
    now_playing(f, app, rows[2]);
}

fn tab(label: &str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray))
    }
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let in_playlists = matches!(app.view, View::Playlists | View::PlaylistTracks);
    let mut spans = vec![
        tab("1 Favorites", app.view == View::Favorites),
        tab("2 Playlists", in_playlists),
        tab("3 Search", app.view == View::Search),
        Span::raw("  "),
    ];
    if app.shuffle {
        spans.push(Span::styled(
            "⇄ shuffle  ",
            Style::default().fg(Color::Green),
        ));
    }
    if let Some(q) = app.queue_indicator() {
        spans.push(Span::styled(
            format!("queue {q}  "),
            Style::default().fg(Color::Blue),
        ));
    }
    if app.mode == Mode::Filter {
        spans.push(Span::styled(
            format!("/{}", app.filter),
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(n) = &app.notice {
        spans.push(Span::styled(
            n.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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

    // DAC badge, then the shared activity slot (resolving / buffering / buffered),
    // then controls.
    let (act_text, act_color) = activity(app);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(dac_badge(s), Style::default().fg(Color::Green)),
            Span::styled(act_text, Style::default().fg(act_color)),
            Span::styled(
                format!(
                    "  [space] play  [h/l] seek  [H/L] skip  [s] shuffle  [+/-] vol {}%  [q] quit",
                    s.volume as u32
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        l2,
    );
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
