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

//! priel-core — TIDAL API access + hi-res stream resolution.
//!
//! UI-agnostic: no printing, no globals. Blocking HTTP (reqwest) so callers run
//! it on a worker thread. Auth currently reuses a hiresTI PKCE token file; a
//! native PKCE flow is the obvious next addition (see `Client::from_token_file`).

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;

pub mod mpd;

const API: &str = "https://api.tidal.com";
const UA: &str = concat!("priel/", env!("CARGO_PKG_VERSION"));

/// Requested audio quality tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Low,
    High,
    Lossless,
    HiRes,
}

impl Quality {
    #[must_use]
    pub fn as_api_str(self) -> &'static str {
        match self {
            Quality::Low => "LOW",
            Quality::High => "HIGH",
            Quality::Lossless => "LOSSLESS",
            Quality::HiRes => "HI_RES_LOSSLESS",
        }
    }
}

/// A track as it appears in library/favorites listings.
#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: u32, // seconds
    /// Short quality tier from the listing (HI-RES / LOSSLESS / HIGH / …).
    /// Per-track sample rate is only known after `resolve_stream`.
    pub quality: String,
}

/// Derive a short quality label from a track's mediaMetadata tags / audioQuality.
fn quality_label(tags: &[String], audio_quality: &str) -> String {
    let up: Vec<String> = tags.iter().map(|s| s.to_uppercase()).collect();
    if up.iter().any(|t| t.contains("HIRES")) {
        "HI-RES".into()
    } else if up.iter().any(|t| t == "LOSSLESS") || audio_quality.eq_ignore_ascii_case("LOSSLESS") {
        "LOSSLESS".into()
    } else if audio_quality.eq_ignore_ascii_case("HIGH") {
        "HIGH".into()
    } else if !audio_quality.is_empty() {
        audio_quality.to_uppercase()
    } else {
        String::new()
    }
}

/// A playlist as it appears in listings / search.
#[derive(Clone, Debug)]
pub struct Playlist {
    pub uuid: String,
    pub title: String,
    pub num_tracks: u32,
    pub duration: u32,
}

/// Multi-type search results.
#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
}

/// What the player needs to actually play a resolved track.
#[derive(Clone, Debug)]
pub enum PlayableSource {
    /// BTS manifest: a single direct URL mpv can fetch itself.
    Direct(String),
    /// MPD/DASH manifest: ordered FLAC segments to stream + concatenate.
    Segments(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct ResolvedStream {
    pub source: PlayableSource,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub codec: String,
    pub quality: String,
}

/// Authenticated session identity.
#[derive(Clone, Debug)]
pub struct Session {
    pub user_id: u64,
    pub country_code: String,
}

pub struct Client {
    http: reqwest::blocking::Client,
    token: String,
    session: Option<Session>,
}

// ---- wire types ----

#[derive(Deserialize)]
struct TokenFile {
    access_token: String,
}

#[derive(Deserialize)]
struct SessionResp {
    #[serde(rename = "userId")]
    user_id: u64,
    #[serde(rename = "countryCode")]
    country_code: String,
}

#[derive(Deserialize)]
struct FavTracksResp {
    items: Vec<FavItem>,
}
#[derive(Deserialize)]
struct FavItem {
    item: TrackBrief,
}
#[derive(Deserialize)]
struct TrackBrief {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    duration: u32,
    #[serde(default)]
    artists: Vec<ArtistBrief>,
    #[serde(default)]
    album: AlbumBrief,
    #[serde(rename = "audioQuality", default)]
    audio_quality: String,
    #[serde(rename = "mediaMetadata", default)]
    media_metadata: MediaMeta,
}
#[derive(Deserialize, Default)]
struct MediaMeta {
    #[serde(default)]
    tags: Vec<String>,
}

impl TrackBrief {
    fn into_track(self) -> Track {
        Track {
            id: self.id,
            title: self.title,
            artist: self
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            album: self.album.title,
            duration: self.duration,
            quality: quality_label(&self.media_metadata.tags, &self.audio_quality),
        }
    }
}

#[derive(Deserialize)]
struct PlaylistBrief {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    title: String,
    #[serde(rename = "numberOfTracks", default)]
    number_of_tracks: u32,
    #[serde(default)]
    duration: u32,
}
impl PlaylistBrief {
    fn into_playlist(self) -> Playlist {
        Playlist {
            uuid: self.uuid,
            title: self.title,
            num_tracks: self.number_of_tracks,
            duration: self.duration,
        }
    }
}
#[derive(Deserialize, Default)]
struct ArtistBrief {
    #[serde(default)]
    name: String,
}
#[derive(Deserialize, Default)]
struct AlbumBrief {
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct Stream {
    #[serde(rename = "audioQuality", default)]
    audio_quality: String,
    #[serde(rename = "manifestMimeType", default)]
    manifest_mime_type: String,
    #[serde(default)]
    manifest: String,
    #[serde(rename = "bitDepth", default)]
    bit_depth: Option<u32>,
    #[serde(rename = "sampleRate", default)]
    sample_rate: Option<u32>,
}

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct BtsManifest {
    #[serde(default)]
    codecs: String,
    #[serde(default)]
    urls: Vec<String>,
}

impl Client {
    /// # Errors
    /// If the HTTP client cannot be built (TLS backend or system config).
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(UA)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            token,
            session: None,
        })
    }

    /// Load the bearer token from a hiresTI token file.
    ///
    /// # Errors
    /// If the file is unreadable (not logged in) or is not the expected JSON.
    pub fn from_token_file(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading token file {path} (is hiresTI logged in?)"))?;
        let tf: TokenFile =
            serde_json::from_str(&raw).with_context(|| format!("parsing token file {path}"))?;
        Self::new(tf.access_token)
    }

    /// Default hiresTI token location: `$XDG_CONFIG_HOME/hiresti/hiresti_token.json`.
    #[must_use]
    pub fn default_token_path() -> String {
        let base = std::env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
        format!("{base}/hiresti/hiresti_token.json")
    }

    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<reqwest::blocking::Response> {
        Ok(self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .query(query)
            .send()?)
    }

    /// Validate the token and cache `userId`/`countryCode`.
    ///
    /// # Errors
    /// On a transport failure, or a non-success status - most often an expired
    /// token, which the message calls out.
    pub fn connect(&mut self) -> Result<Session> {
        let resp = self.get(&format!("{API}/v1/sessions"), &[])?;
        if !resp.status().is_success() {
            bail!(
                "GET /v1/sessions -> HTTP {} (token expired? re-login in hiresTI)",
                resp.status()
            );
        }
        let s: SessionResp = resp.json()?;
        let sess = Session {
            user_id: s.user_id,
            country_code: s.country_code,
        };
        self.session = Some(sess.clone());
        Ok(sess)
    }

    fn session(&self) -> Result<&Session> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow!("call connect() first"))
    }

    /// A page of the user's favorite tracks, newest first.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn favorite_tracks(&self, offset: u32, limit: u32) -> Result<Vec<Track>> {
        let sess = self.session()?;
        let url = format!("{API}/v1/users/{}/favorites/tracks", sess.user_id);
        let (off, lim) = (offset.to_string(), limit.to_string());
        let resp = self.get(
            &url,
            &[
                ("countryCode", sess.country_code.as_str()),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
                ("order", "DATE"),
                ("orderDirection", "DESC"),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("favorites/tracks -> HTTP {}", resp.status());
        }
        let fr: FavTracksResp = resp.json()?;
        Ok(fr.items.into_iter().map(|i| i.item.into_track()).collect())
    }

    /// The user's own playlists.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn user_playlists(&self, offset: u32, limit: u32) -> Result<Vec<Playlist>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            items: Vec<PlaylistBrief>,
        }

        let sess = self.session()?;
        let url = format!("{API}/v1/users/{}/playlists", sess.user_id);
        let (off, lim) = (offset.to_string(), limit.to_string());
        let resp = self.get(
            &url,
            &[
                ("countryCode", sess.country_code.as_str()),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("playlists -> HTTP {}", resp.status());
        }
        let r: R = resp.json()?;
        Ok(r.items
            .into_iter()
            .map(PlaylistBrief::into_playlist)
            .collect())
    }

    /// Tracks in a playlist.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status (an unknown or private `uuid` yields 404).
    pub fn playlist_tracks(&self, uuid: &str, offset: u32, limit: u32) -> Result<Vec<Track>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            items: Vec<TrackBrief>,
        }

        let sess = self.session()?;
        let url = format!("{API}/v1/playlists/{uuid}/tracks");
        let (off, lim) = (offset.to_string(), limit.to_string());
        let resp = self.get(
            &url,
            &[
                ("countryCode", sess.country_code.as_str()),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("playlist tracks -> HTTP {}", resp.status());
        }
        let r: R = resp.json()?;
        Ok(r.items.into_iter().map(TrackBrief::into_track).collect())
    }

    /// Multi-type search (tracks + playlists).
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status. An empty result set is `Ok`, not an error.
    pub fn search(&self, query: &str, limit: u32) -> Result<SearchResults> {
        #[derive(Deserialize)]
        struct Wrap<T> {
            items: Vec<T>,
        }
        impl<T> Default for Wrap<T> {
            fn default() -> Self {
                Self { items: Vec::new() }
            }
        }
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            tracks: Wrap<TrackBrief>,
            #[serde(default)]
            playlists: Wrap<PlaylistBrief>,
        }

        let sess = self.session()?;
        let url = format!("{API}/v1/search");
        let lim = limit.to_string();
        let resp = self.get(
            &url,
            &[
                ("query", query),
                ("countryCode", sess.country_code.as_str()),
                ("limit", lim.as_str()),
                ("types", "TRACKS,PLAYLISTS"),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("search -> HTTP {}", resp.status());
        }
        let r: R = resp.json()?;
        Ok(SearchResults {
            tracks: r
                .tracks
                .items
                .into_iter()
                .map(TrackBrief::into_track)
                .collect(),
            playlists: r
                .playlists
                .items
                .into_iter()
                .map(PlaylistBrief::into_playlist)
                .collect(),
        })
    }

    /// Resolve a track to a playable source at the requested quality.
    ///
    /// # Errors
    /// On a transport failure, a non-success status (the response body is
    /// included, since it carries the reason a track is unplayable), or a
    /// manifest that is not valid base64, not a known mime type, or empty.
    pub fn resolve_stream(&self, track_id: u64, quality: Quality) -> Result<ResolvedStream> {
        let url = format!("{API}/v1/tracks/{track_id}/playbackinfopostpaywall");
        let resp = self.get(
            &url,
            &[
                ("audioquality", quality.as_api_str()),
                ("playbackmode", "STREAM"),
                ("assetpresentation", "FULL"),
            ],
        )?;
        if !resp.status().is_success() {
            let code = resp.status();
            bail!(
                "playbackinfo -> HTTP {code}: {}",
                resp.text().unwrap_or_default()
            );
        }
        let s: Stream = resp.json()?;
        decode_manifest(&s)
    }
}

fn decode_manifest(s: &Stream) -> Result<ResolvedStream> {
    let bytes = STANDARD
        .decode(s.manifest.trim())
        .context("base64 manifest")?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let mt = s.manifest_mime_type.to_lowercase();

    let (source, codec, sr) = if mt.contains("dash") {
        let info = mpd::parse(&text)?;
        (
            PlayableSource::Segments(info.segment_urls),
            info.codec,
            info.sample_rate,
        )
    } else if mt.contains("bts") {
        let m: BtsManifest = serde_json::from_str(&text).context("BTS manifest json")?;
        if m.urls.is_empty() {
            bail!("BTS manifest had no urls");
        }
        (PlayableSource::Direct(m.urls[0].clone()), m.codecs, 0)
    } else {
        bail!("unknown manifestMimeType: {}", s.manifest_mime_type);
    };

    Ok(ResolvedStream {
        source,
        sample_rate: s.sample_rate.unwrap_or(sr),
        bit_depth: s.bit_depth.unwrap_or(0),
        codec,
        quality: s.audio_quality.clone(),
    })
}
