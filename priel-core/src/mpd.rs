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

//! Minimal MPD/DASH parser: reconstruct the ordered segment-URL list.
//! Concatenating those segments yields a plain FLAC stream. Segment-count logic
//! mirrors tidalapi's `DashInfo.get_urls()` so it matches the live catalog.

use anyhow::{Result, anyhow};
use regex::Regex;

pub struct MpdInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub segment_urls: Vec<String>,
}

/// # Errors
/// If the MPD carries no `<SegmentTemplate media=...>` to build URLs from.
pub fn parse(xml: &str) -> Result<MpdInfo> {
    let unescape = |s: &str| s.replace("&amp;", "&");

    let re_rate = Regex::new(r#"audioSamplingRate="(\d+)""#)?;
    let re_codec = Regex::new(r#"codecs="([^"]+)""#)?;
    let re_media = Regex::new(r#"media="([^"]+)""#)?;
    let re_s = Regex::new(r"<S\b([^>]*?)/?>")?;
    let re_r = Regex::new(r#"\br="(\d+)""#)?;

    let sample_rate = re_rate
        .captures(xml)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let codec = re_codec
        .captures(xml)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    let media_tpl = re_media
        .captures(xml)
        .map(|c| unescape(&c[1]))
        .ok_or_else(|| anyhow!("no <SegmentTemplate media=...> in MPD"))?;

    // segments_count = 1 (init) + 1 (first media) + sum(r or 1 per <S>)
    let mut count: usize = 2;
    for s in re_s.captures_iter(xml) {
        count += re_r
            .captures(&s[1])
            .and_then(|c| c[1].parse::<usize>().ok())
            .unwrap_or(1);
    }

    let segment_urls = (0..count)
        .map(|i| media_tpl.replace("$Number$", &i.to_string()))
        .collect();

    Ok(MpdInfo {
        codec,
        sample_rate,
        segment_urls,
    })
}
