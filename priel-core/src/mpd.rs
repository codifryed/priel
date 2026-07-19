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

#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::parse;

    /// A minimal MPD shaped like the ones the catalogue returns.
    fn mpd(segments: &str) -> String {
        format!(
            r#"<MPD><Period><AdaptationSet><Representation codecs="flac"
               audioSamplingRate="96000">
               <SegmentTemplate media="https://cdn/x/$Number$.mp4?t=1&amp;u=2"
                 initialization="https://cdn/x/init.mp4">
                 <SegmentTimeline>{segments}</SegmentTimeline>
               </SegmentTemplate></Representation></AdaptationSet></Period></MPD>"#
        )
    }

    #[test]
    fn reads_codec_and_sample_rate() {
        // Goal: the two metadata fields the player shows come straight off the
        // manifest, so a change in attribute spelling must fail loudly.
        let info = parse(&mpd(r#"<S d="10"/>"#)).unwrap();
        assert_eq!(info.codec, "flac");
        assert_eq!(info.sample_rate, 96_000);
    }

    #[test]
    fn missing_metadata_degrades_instead_of_failing() {
        // Goal: a manifest without codec or rate is still playable; only the
        // segment template is load-bearing. Absent fields read as unknown.
        let xml = r#"<MPD><SegmentTemplate media="https://cdn/$Number$.mp4"/></MPD>"#;
        let info = parse(xml).unwrap();
        assert_eq!(info.sample_rate, 0);
        assert_eq!(info.codec, "");
        assert_eq!(info.segment_urls.len(), 2);
    }

    #[test]
    fn a_manifest_without_a_media_template_is_an_error() {
        // Goal: this is the one unrecoverable case - without the template there
        // is nothing to fetch, so it must be an error and not an empty list.
        let err = parse("<MPD><Period/></MPD>").unwrap_err();
        assert!(
            err.to_string().contains("SegmentTemplate"),
            "the message should name what was missing: {err}"
        );
    }

    #[test]
    fn segment_count_is_init_plus_first_plus_every_repeat() {
        // Goal: pin the counting rule that mirrors tidalapi. Off-by-one here
        // truncates the end of a track or requests a segment that 404s, and
        // both are silent - playback simply stops early.
        // One <S> with no r: 1 init + 1 first + 1 = 3.
        assert_eq!(parse(&mpd(r#"<S d="4"/>"#)).unwrap().segment_urls.len(), 3);
        // r="5" means five *additional* segments: 1 + 1 + 5 = 7.
        assert_eq!(
            parse(&mpd(r#"<S d="4" r="5"/>"#))
                .unwrap()
                .segment_urls
                .len(),
            7
        );
        // Runs accumulate: 1 + 1 + 3 + 1 = 6.
        assert_eq!(
            parse(&mpd(r#"<S d="4" r="3"/><S d="4"/>"#))
                .unwrap()
                .segment_urls
                .len(),
            6
        );
        // No timeline at all still yields the init and first media segments.
        assert_eq!(parse(&mpd("")).unwrap().segment_urls.len(), 2);
    }

    #[test]
    fn urls_are_numbered_from_zero_and_xml_unescaped() {
        // Goal: `$Number$` starts at 0 (the init segment) and `&amp;` must be
        // turned back into `&`, or every query-signed CDN URL is rejected.
        let info = parse(&mpd(r#"<S d="4"/>"#)).unwrap();
        assert_eq!(info.segment_urls[0], "https://cdn/x/0.mp4?t=1&u=2");
        assert_eq!(info.segment_urls[1], "https://cdn/x/1.mp4?t=1&u=2");
        assert_eq!(info.segment_urls[2], "https://cdn/x/2.mp4?t=1&u=2");
        assert!(
            !info.segment_urls[0].contains("&amp;"),
            "escaped entities would break the signature"
        );
    }

    #[test]
    fn a_self_closing_and_open_s_tag_count_the_same() {
        // Goal: real manifests use both `<S .../>` and `<S ...>`; the regex has
        // an optional slash and this locks that in.
        let closed = parse(&mpd(r#"<S d="4" r="2"/>"#)).unwrap();
        let open = parse(&mpd(r#"<S d="4" r="2">"#)).unwrap();
        assert_eq!(closed.segment_urls.len(), open.segment_urls.len());
    }
}
