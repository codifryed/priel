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
//! it on a worker thread. Sessions are OAuth PKCE and priel owns its own: see
//! the `auth` module for signing in and renewing.

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::io::Read as _;
use ureq::http::Response;
use ureq::{Agent, Body};

pub mod auth;
pub mod mpd;
pub mod update;

const API: &str = "https://api.tidal.com";
const UA: &str = concat!("priel/", env!("CARGO_PKG_VERSION"));

/// Rows one scan of a playlist's items asks for, looking for a track's position.
const PLAYLIST_ITEMS_PAGE: u32 = 500;

/// How many such scans one removal makes before giving up.
///
/// A bound rather than "until the end" because the listing is external input
/// and a service that kept answering would keep this loop running. Twenty pages
/// reaches ten thousand tracks, which is past the longest playlist the vendor
/// will store.
const PLAYLIST_SCANS_MAX: u32 = 20;

/// The digits a percent-escape is written with.
const HEX: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
];

/// Percent-encode a set of pairs as an HTML form body.
///
/// Written here rather than taken from a crate because it is fifteen lines and
/// the only caller is the two playlist writes: a title is user-written text and
/// may hold a space, an ampersand or an accent, any of which would otherwise
/// arrive as a different title or as two fields.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(char::from(byte));
                }
                b' ' => out.push('+'),
                _ => {
                    // Two hex digits, upper case, written straight in: the
                    // encoding is defined on bytes, and formatting through a
                    // temporary String to append two characters is waste on a
                    // path a title with an accent takes for every one of them.
                    out.push('%');
                    out.push(HEX[usize::from(byte >> 4)]);
                    out.push(HEX[usize::from(byte & 0x0f)]);
                }
            }
        }
    }
    out
}

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
///
/// A listing row carries about thirty fields and this holds the twelve a
/// terminal can act on. What the rest are, and why each was left where it was,
/// is written down in `docs/track-fields.md` rather than guessed at again: the
/// rows are **not** abbreviated by the listing endpoints, so anything missing
/// here was discarded by this crate and can be had by asking for it.
///
/// `Default` is derived so a caller can build a partial row - a placeholder, a
/// test fixture - without listing every field, and so that adding a twelfth does
/// not break every construction site.
#[derive(Clone, Debug, Default)]
pub struct Track {
    pub id: u64,
    pub title: String,
    /// The first credited artist, for the places with one line to spend.
    pub artist: String,
    /// Everyone credited, in the order the service credits them. `artist` is
    /// the first of these; a collaboration is the reason both exist.
    pub artists: Vec<String>,
    pub album: String,
    pub duration_secs: u32,
    /// Short quality tier from the listing (HI-RES / LOSSLESS / HIGH / …).
    /// Per-track sample rate is only known after `resolve_stream`.
    pub quality: String,
    /// What distinguishes this recording from the plain one: "Remastered",
    /// "Live at Wembley", "Radio Edit". Empty when there is nothing to add.
    ///
    /// Worth its own field rather than folded into the title, because two rows
    /// that are otherwise character-for-character identical differ only here.
    pub version: String,
    /// Whether the service marks the lyrics explicit.
    pub explicit: bool,
    /// The recording's global identifier, empty when the row omits it.
    ///
    /// Absent from a mix's rows, which are a shorter shape than the other
    /// listings send. The empty string is that, not a track without one.
    pub isrc: String,
    /// The rights line, as the service words it. Empty when the row omits it.
    pub copyright: String,
    /// Whether the service says this track can be played at all.
    ///
    /// Two wire bits both have to hold. Absence is read as playable: defaulting
    /// the other way would condemn a whole listing the moment the service
    /// trimmed a field, which is worse than one play failing with its reason.
    pub streamable: bool,
    /// The radio mix the service builds around this track, for
    /// [`Client::mix_tracks`]. Empty when the row named none.
    ///
    /// Every listing row carries it, which is what lets a caller carry on
    /// playing past the end of a queue without asking for anything first. Empty
    /// is a real answer and not a failure: a track with no mix is a track
    /// nothing can be continued from, and the caller has something to say about
    /// that rather than something to retry.
    pub mix_id: String,
    /// The album art identifier, empty when the row named none.
    ///
    /// An opaque id, not a URL: [`cover_url`] turns it into one. Empty is a real
    /// answer - a track whose listing carried no cover - and a frontend that
    /// cannot draw pictures ignores it at no cost, which is why it is exposed
    /// here rather than resolved in the library.
    pub cover: String,
}

/// The URL of a track's album cover at a given square size, or `None` when the
/// track named no cover.
///
/// **The pattern is the documented public one and is unverified against a live
/// response in this repository** - there are no captures here to check it, so a
/// caller must confirm it on real hardware before trusting it, and a cover that
/// fails to fetch is treated as absent rather than as an error. See
/// `docs/cover-art.md`.
///
/// The service keys its image host by the cover id with its dashes turned into
/// path separators, and serves a square JPEG at a handful of fixed sizes. `size`
/// is snapped to the nearest of those the caller is expected to have asked for;
/// this function does not validate it, because the set is the service's and can
/// change without this code.
#[must_use]
pub fn cover_url(cover_id: &str, size: u32) -> Option<String> {
    if cover_id.is_empty() {
        return None;
    }
    let path = cover_id.replace('-', "/");
    Some(format!(
        "https://resources.tidal.com/images/{path}/{size}x{size}.jpg"
    ))
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
    pub duration_secs: u32,
}

/// A mix the service builds for the listener.
///
/// Not a [`Playlist`] under another name, and the differences are the reason
/// this is a type of its own rather than a flag on that one. A mix is keyed by
/// an opaque string rather than a uuid; nobody can edit it; and it is rebuilt
/// under the listener, so a copy of it goes stale in a way a playlist a person
/// wrote does not.
///
/// **The wire carries no track count and no duration for a mix.** A playlist row
/// says how long it is before a single track has been asked for; a mix row
/// cannot, and the only way to learn a mix's length is to ask for its tracks.
/// What a mix carries instead is a subtitle - what the service built it from -
/// which is the nearest thing to a description of its contents there is.
#[derive(Clone, Debug)]
pub struct Mix {
    /// The service's identifier for this mix. Opaque: not a number, not a uuid.
    pub id: String,
    pub title: String,
    /// What the mix was built from, in the service's own words.
    pub subtitle: String,
}

/// One page of a listing, plus how long the whole listing is.
///
/// The total is the only honest end-of-list signal. A page shorter than the
/// limit asked for looks like the end and is not: the service caps some
/// listings below whatever limit it was given, and a caller that stopped there
/// would silently hide the rest of the library.
#[derive(Clone, Debug, Default)]
pub struct Page<T> {
    /// The rows in this page, in the order the service returned them.
    pub items: Vec<T>,
    /// Rows in the whole listing, as the service reports it. Zero when the
    /// answer carried no count - a guess would be worse than none.
    pub total: u32,
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
    /// The master's replay gain in decibels, or zero when the answer carried
    /// none. Reported, never applied: scaling the samples is the one thing a
    /// bit-perfect path may not do.
    pub replay_gain_db: f32,
    /// The master's peak amplitude as a fraction of full scale, or zero when the
    /// answer carried none. A modern master reads a hair under 1.0.
    pub peak: f32,
}

/// Authenticated session identity.
#[derive(Clone, Debug)]
pub struct Session {
    pub user_id: u64,
    pub country_code: String,
}

/// Everything needed to keep a session alive on its own.
struct AuthState {
    cfg: auth::AuthConfig,
    token_path: String,
    stored: auth::StoredToken,
}

pub struct Client {
    http: Agent,
    token: String,
    session: Option<Session>,
    base: String,
    /// Present only when credentials were configured. Without it the client
    /// still works, it just cannot renew the session when it expires.
    auth: Option<AuthState>,
}

// ---- wire types ----

/// Read a value that may be **missing or an explicit `null`**.
///
/// `#[serde(default)]` covers only the first. This service spells absence as
/// `null` freely - `version` on most tracks, `accessType`, a playlist
/// `description`, a whole `mixes` object on a track it built no radio for - so
/// a defaulted non-`Option` field rejects the commonest shape it will meet, and
/// one such field fails the entire page rather than one row.
///
/// Not hypothetical: it took the favorites listing down, and the guard was on
/// the field *inside* the object rather than on the object itself. Use this on
/// every defaulted field that is not already an `Option` and the distinction
/// stops mattering.
fn null_to_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
    items: Vec<ItemRow>,
    /// Sent by the service on every listing, and discarded here until paging
    /// needed it. Defaulted rather than required: an answer without it is still
    /// a usable page.
    #[serde(
        rename = "totalNumberOfItems",
        default,
        deserialize_with = "null_to_default"
    )]
    total_number_of_items: u32,
}
/// The wrapper the service puts round a row it could have sent plainly.
///
/// Shared by the favorites and by a mix's tracks, which send the same shape. A
/// row also carries a `type` saying whether it is a track or a video, and that
/// is discarded on purpose: dropping the video rows here would leave the
/// caller's next offset counting a different thing from the service's, and page
/// some of the rows twice.
#[derive(Deserialize)]
struct ItemRow {
    item: TrackBrief,
}
/// One row of a playlist's *items* listing, read only for its position.
///
/// Deliberately not [`ItemRow`]: this one is asked what number a row is, not
/// what is on it, so every field is optional. A playlist whose contents cannot
/// be parsed is a playlist a track cannot be removed from, and a sparse row is
/// not a good enough reason to refuse that.
#[derive(Deserialize)]
struct PlaylistItemRow {
    #[serde(rename = "type", default, deserialize_with = "null_to_default")]
    kind: String,
    #[serde(default, deserialize_with = "null_to_default")]
    item: PlaylistItemId,
}
#[derive(Deserialize, Default)]
struct PlaylistItemId {
    #[serde(default, deserialize_with = "null_to_default")]
    id: u64,
}

/// What a caller must hold before it may change a playlist.
struct PlaylistState {
    /// The description as it stands, which a rename has to send back unchanged.
    description: String,
    /// The service's concurrency token for this playlist's contents.
    token: String,
}
/// One track as a listing sends it, in the service's own spelling.
///
/// Every string here is `Option<String>` rather than `#[serde(default)]`, and
/// that is not tidiness. `#[serde(default)]` fills in a *missing* key and
/// rejects an explicit `null` - and `null` is how this service spells "no
/// version", on the majority of tracks in the catalogue. The distinction cost a
/// whole listing per null before it was understood, so keep the `Option`s.
#[derive(Deserialize)]
struct TrackBrief {
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<u32>,
    #[serde(default, deserialize_with = "null_to_default")]
    artists: Vec<ArtistBrief>,
    #[serde(default)]
    album: Option<AlbumBrief>,
    #[serde(rename = "audioQuality", default)]
    audio_quality: Option<String>,
    #[serde(
        rename = "mediaMetadata",
        default,
        deserialize_with = "null_to_default"
    )]
    media_metadata: MediaMeta,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    explicit: Option<bool>,
    #[serde(default)]
    isrc: Option<String>,
    #[serde(default)]
    copyright: Option<String>,
    #[serde(rename = "allowStreaming", default)]
    allow_streaming: Option<bool>,
    #[serde(rename = "streamReady", default)]
    stream_ready: Option<bool>,
    #[serde(default, deserialize_with = "null_to_default")]
    mixes: Option<MixIds>,
}
#[derive(Deserialize, Default)]
struct MediaMeta {
    #[serde(default, deserialize_with = "null_to_default")]
    tags: Vec<String>,
}
/// The mixes a track names, of which one can be played.
///
/// The object also carries the ids of screens built round the album and the
/// artist, and those are left on the wire: they key pages this client has no
/// view for, where `TRACK_MIX` keys the one listing [`Client::mix_tracks`] can
/// already fetch. `Option` rather than `#[serde(default)]` for the same reason
/// every string on [`TrackBrief`] is - an explicit `null` is how this service
/// spells absence, and a defaulted `String` rejects it.
#[derive(Deserialize, Default)]
struct MixIds {
    #[serde(rename = "TRACK_MIX", default)]
    track_mix: Option<String>,
}

impl TrackBrief {
    fn into_track(self) -> Track {
        let artists: Vec<String> = self
            .artists
            .into_iter()
            .filter_map(|a| a.name.filter(|n| !n.is_empty()))
            .collect();
        let audio_quality = self.audio_quality.unwrap_or_default();
        let album = self.album.unwrap_or_default();
        Track {
            id: self.id,
            title: self.title.unwrap_or_default(),
            artist: artists.first().cloned().unwrap_or_default(),
            artists,
            cover: album.cover.unwrap_or_default(),
            album: album.title.unwrap_or_default(),
            duration_secs: self.duration.unwrap_or_default(),
            quality: quality_label(&self.media_metadata.tags, &audio_quality),
            version: self.version.unwrap_or_default(),
            explicit: self.explicit.unwrap_or(false),
            isrc: self.isrc.unwrap_or_default(),
            copyright: self.copyright.unwrap_or_default(),
            // Both, and absent means yes. See the field's own note.
            streamable: self.allow_streaming.unwrap_or(true) && self.stream_ready.unwrap_or(true),
            mix_id: self.mixes.and_then(|m| m.track_mix).unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
struct PlaylistBrief {
    #[serde(default, deserialize_with = "null_to_default")]
    uuid: String,
    #[serde(default, deserialize_with = "null_to_default")]
    title: String,
    #[serde(
        rename = "numberOfTracks",
        default,
        deserialize_with = "null_to_default"
    )]
    number_of_tracks: u32,
    #[serde(default, deserialize_with = "null_to_default")]
    duration: u32,
}
impl PlaylistBrief {
    fn into_playlist(self) -> Playlist {
        Playlist {
            uuid: self.uuid,
            title: self.title,
            num_tracks: self.number_of_tracks,
            duration_secs: self.duration,
        }
    }
}
#[derive(Deserialize)]
struct MixBrief {
    #[serde(default, deserialize_with = "null_to_default")]
    id: String,
    #[serde(default, deserialize_with = "null_to_default")]
    title: String,
    #[serde(rename = "subTitle", default, deserialize_with = "null_to_default")]
    sub_title: String,
}
impl MixBrief {
    fn into_mix(self) -> Mix {
        Mix {
            id: self.id,
            title: self.title,
            subtitle: self.sub_title,
        }
    }
}

/// A "page" answer: the rows of a screen rather than the rows of a listing.
///
/// Two of the calls here are served by an endpoint that describes a screen in
/// the vendor's own web client - a list of rows, each holding modules, only some
/// of which carry anything to read. That shape is the service's business and not
/// a frontend's, so it is unwrapped into a [`Page`] here and never leaves the
/// crate. Deserialised generically because the two callers differ only in what
/// their list holds.
//
// The `bound` on each of these is load-bearing: `#[serde(default)]` on a field
// whose type mentions `T` makes the derive ask for `T: Default` as well, and the
// row types have no sensible default. Naming the bound keeps the requirement to
// the one thing actually needed.
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ScreenResp<T> {
    #[serde(default = "Vec::new", deserialize_with = "null_to_default")]
    rows: Vec<ScreenRow<T>>,
}
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ScreenRow<T> {
    #[serde(default = "Vec::new", deserialize_with = "null_to_default")]
    modules: Vec<ScreenModule<T>>,
}
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct ScreenModule<T> {
    #[serde(rename = "pagedList", default = "Option::default")]
    paged_list: Option<PagedList<T>>,
}
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct PagedList<T> {
    #[serde(default = "Vec::new", deserialize_with = "null_to_default")]
    items: Vec<T>,
    #[serde(
        rename = "totalNumberOfItems",
        default,
        deserialize_with = "null_to_default"
    )]
    total_number_of_items: u32,
}

impl<T> ScreenResp<T> {
    /// The first list on the screen, and the length the service gave it.
    ///
    /// The *first*, not all of them joined: a screen can hold several lists and
    /// each is paged separately, so pouring them into one sequence would make
    /// one offset stand for several places at once - the same mistake
    /// [`Client::search_tracks`] stopped making when it gave up asking for two
    /// kinds of result at a time. The modules before it carry headers and links
    /// and no list at all, which is why this looks for one rather than taking
    /// the first module.
    ///
    /// A screen with no list on it is an empty page. That is a real answer: it
    /// is what a listener with no mixes gets, and what an unknown mix gets.
    fn first_list(self) -> (Vec<T>, u32) {
        self.rows
            .into_iter()
            .flat_map(|row| row.modules)
            .find_map(|module| module.paged_list)
            .map_or_else(
                || (Vec::new(), 0),
                |list| (list.items, list.total_number_of_items),
            )
    }
}

#[derive(Deserialize, Default)]
struct ArtistBrief {
    #[serde(default)]
    name: Option<String>,
}
#[derive(Deserialize, Default)]
struct AlbumBrief {
    #[serde(default)]
    title: Option<String>,
    /// The album art identifier, the service's own opaque id for the cover.
    ///
    /// Left on the wire until now as "art a terminal cannot use"; a half-block
    /// renderer is what changed that. Still just an id here - turning it into a
    /// URL is [`cover_url`], and fetching the bytes is the caller's to do.
    #[serde(default)]
    cover: Option<String>,
}

#[derive(Deserialize)]
struct Stream {
    #[serde(rename = "audioQuality", default, deserialize_with = "null_to_default")]
    audio_quality: String,
    #[serde(
        rename = "manifestMimeType",
        default,
        deserialize_with = "null_to_default"
    )]
    manifest_mime_type: String,
    #[serde(default, deserialize_with = "null_to_default")]
    manifest: String,
    #[serde(rename = "bitDepth", default)]
    bit_depth: Option<u32>,
    #[serde(rename = "sampleRate", default)]
    sample_rate: Option<u32>,
    #[serde(rename = "trackReplayGain", default)]
    track_replay_gain: Option<f32>,
    #[serde(rename = "trackPeakAmplitude", default)]
    track_peak_amplitude: Option<f32>,
}

#[allow(non_snake_case)]
#[derive(Deserialize)]
struct BtsManifest {
    #[serde(default, deserialize_with = "null_to_default")]
    codecs: String,
    #[serde(default, deserialize_with = "null_to_default")]
    urls: Vec<String>,
}

/// An HTTP agent configured the way this crate expects.
///
/// Exposed so callers that need a one-off request - fetching a client identity
/// before any `Client` exists, say - do not have to reinvent the settings.
#[must_use]
/// Why a request failed, for a caller that has to *act* rather than display.
///
/// The message still says what happened, in prose, for the log and for the
/// screen. This says what kind of thing it was, so the interface can branch
/// without matching on text - a reworded sentence must not be able to stop the
/// sign-in screen from being offered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The session is gone and cannot be renewed. Signing in again is the fix,
    /// and it is one keystroke.
    SignedOut,
    /// Nothing reached the service, or nothing came back. Worth retrying;
    /// there is nothing for the user to correct.
    Unreachable,
    /// The service answered, and the answer was no. Retrying will not help.
    Refused,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SignedOut => "the session is no longer valid",
            Self::Unreachable => "the service could not be reached",
            Self::Refused => "the request was refused",
        })
    }
}

impl Fault {
    /// Classify an error.
    ///
    /// An explicit marker wins, because only the code that made the request
    /// knows a refusal was final. Failing that the transport decides: a request
    /// that never arrived is a different thing from an answer we did not like,
    /// and only ureq can tell them apart.
    pub fn of(err: &anyhow::Error) -> Self {
        // `downcast_ref` on an anyhow error searches the whole chain, including
        // values attached with `.context()`, which is how the marker is carried.
        if let Some(fault) = err.downcast_ref::<Self>() {
            return *fault;
        }
        match err.downcast_ref::<ureq::Error>() {
            Some(
                ureq::Error::Io(_)
                | ureq::Error::Timeout(_)
                | ureq::Error::HostNotFound
                | ureq::Error::ConnectionFailed
                | ureq::Error::RedirectFailed,
            ) => Self::Unreachable,
            _ => Self::Refused,
        }
    }
}

impl std::error::Error for Fault {}

#[must_use]
pub fn new_agent() -> Agent {
    Agent::new_with_config(
        Agent::config_builder()
            .user_agent(UA)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .http_status_as_error(false)
            .build(),
    )
}

impl Client {
    /// # Errors
    /// Currently infallible; kept fallible so adding TLS/proxy configuration
    /// later is not a breaking change.
    pub fn new(token: String) -> Result<Self> {
        // `http_status_as_error(false)` keeps ureq's behaviour the same as
        // reqwest's: a non-2xx is a normal response, and each call site decides
        // what to say about it.
        let http = Agent::new_with_config(
            Agent::config_builder()
                .user_agent(UA)
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .http_status_as_error(false)
                .build(),
        );
        Ok(Self {
            http,
            token,
            session: None,
            base: API.to_string(),
            auth: None,
        })
    }

    /// Load the bearer token from a saved session.
    ///
    /// # Errors
    /// If the file is unreadable (not logged in) or is not the expected JSON.
    pub fn from_token_file(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::Error::new(e).context(Fault::SignedOut))
            .with_context(|| format!("reading token file {path} (not signed in?)"))?;
        let tf: TokenFile =
            serde_json::from_str(&raw).with_context(|| format!("parsing token file {path}"))?;
        Self::new(tf.access_token)
    }

    /// Where priel keeps its session: `$XDG_STATE_HOME/priel/token.json`.
    ///
    /// State rather than config - a session is obtained at runtime, expires, and
    /// is regenerated by signing in again. Closer to a persisted cookie than to
    /// a setting.
    #[must_use]
    pub fn default_token_path() -> String {
        format!("{}/token.json", auth::state_dir())
    }

    /// Point the client at a different API origin.
    ///
    /// The only reason this exists is testability: without it every request is
    /// nailed to the live service and nothing can be exercised against a stub.
    #[must_use]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// A client that can renew its own session.
    ///
    /// This is what stops the access token expiring mid-listen: the stored
    /// expiry is checked before every request, and a rejected request is retried
    /// once after a refresh.
    ///
    /// # Errors
    /// If the token file cannot be read, or the HTTP client cannot be built.
    pub fn with_auth(token_path: &str, cfg: auth::AuthConfig) -> Result<Self> {
        let stored = auth::StoredToken::load(token_path)?;
        let mut client = Self::new(stored.access_token.clone())?;
        client.auth = Some(AuthState {
            cfg,
            token_path: token_path.to_string(),
            stored,
        });
        Ok(client)
    }

    /// Seconds until the stored session expires, if it is known.
    #[must_use]
    pub fn session_expires_in(&self) -> Option<i64> {
        self.auth
            .as_ref()
            .and_then(|a| a.stored.seconds_remaining(auth::now_epoch()))
    }

    /// Renew the access token now and persist it.
    ///
    /// # Errors
    /// If there are no credentials configured, or the service refuses the
    /// refresh - which usually means the refresh token has expired too and a
    /// full login is required.
    pub fn refresh_session(&mut self) -> Result<()> {
        let state = self
            .auth
            .as_mut()
            .ok_or_else(|| anyhow!("no client credentials configured, cannot refresh"))?;
        let renewed = auth::refresh(
            &self.http,
            &state.cfg,
            &state.stored.refresh_token,
            auth::now_epoch(),
        )?;
        // Persist before adopting: a token we failed to save would be lost on
        // the next start, and the old one is still on disk and still valid.
        renewed.save(&state.token_path)?;
        self.token.clone_from(&renewed.access_token);
        state.stored = renewed;
        Ok(())
    }

    /// Refresh ahead of expiry, if a refresh is possible and due.
    fn ensure_fresh(&mut self) -> Result<()> {
        let due = self
            .auth
            .as_ref()
            .is_some_and(|a| a.stored.needs_refresh(auth::now_epoch()));
        if due {
            self.refresh_session()?;
        }
        Ok(())
    }

    /// A request that renews the session when it has to.
    ///
    /// Proactive refresh handles the common case; the retry covers a token the
    /// service rejected earlier than its stated expiry, which happens when a
    /// session is revoked elsewhere.
    ///
    /// The request arrives as a closure rather than as a built request, and it
    /// is handed the token afresh each time it is called. ureq's builder is a
    /// different type per verb, so there is no one request object the retry
    /// could re-send; and a request built once would carry the header of the
    /// token that had just been rejected. Writing this once matters more than it
    /// looks: the retry lived in the GET path alone, and a write bolted on
    /// beside it would have been the one request that could not survive an
    /// expired session.
    fn send_authed<F>(&mut self, send: F) -> Result<Response<Body>>
    where
        F: Fn(&Agent, &str) -> Result<Response<Body>>,
    {
        self.ensure_fresh()?;
        let resp = send(&self.http, &self.token)?;
        if resp.status() != 401 || self.auth.is_none() {
            return Ok(resp);
        }
        drop(resp);
        self.refresh_session()
            .map_err(|e| e.context(Fault::SignedOut))
            .context("the session was rejected and could not be renewed; log in again")?;
        send(&self.http, &self.token)
    }

    fn get_authed(&mut self, url: &str, query: &[(&str, &str)]) -> Result<Response<Body>> {
        self.send_authed(|http, token| {
            let mut req = http
                .get(url)
                .header("Authorization", format!("Bearer {token}"));
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.call()?)
        })
    }

    /// A form POST that renews the session the way a GET does.
    fn post_form_authed(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
        form: &str,
    ) -> Result<Response<Body>> {
        self.send_authed(|http, token| {
            let mut req = http
                .post(url)
                .header("Authorization", format!("Bearer {token}"))
                .content_type("application/x-www-form-urlencoded");
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.send(form)?)
        })
    }

    /// A DELETE that renews the session the way a GET does.
    fn delete_authed(&mut self, url: &str, query: &[(&str, &str)]) -> Result<Response<Body>> {
        self.send_authed(|http, token| {
            let mut req = http
                .delete(url)
                .header("Authorization", format!("Bearer {token}"));
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.call()?)
        })
    }

    /// A PUT that renews the session the way a GET does.
    ///
    /// Everything this verb is used for here carries its arguments in the query
    /// string, so there is no body parameter to go with it.
    fn put_authed(&mut self, url: &str, query: &[(&str, &str)]) -> Result<Response<Body>> {
        self.send_authed(|http, token| {
            let mut req = http
                .put(url)
                .header("Authorization", format!("Bearer {token}"));
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.send_empty()?)
        })
    }

    /// A change of a playlist's contents, sent under a concurrency token.
    ///
    /// The token is the whole reason this is not `post_form_authed`. The
    /// service will only apply a change to a playlist whose contents still look
    /// the way they did when the caller last read them, and it spells that
    /// condition `If-None-Match` - the header's usual meaning inverted, which
    /// is why the name says what it is *for* rather than what it sends.
    fn post_form_guarded(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
        form: &str,
        token: &str,
    ) -> Result<Response<Body>> {
        self.send_authed(|http, bearer| {
            let mut req = http
                .post(url)
                .header("Authorization", format!("Bearer {bearer}"))
                .header("If-None-Match", token)
                .content_type("application/x-www-form-urlencoded");
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.send(form)?)
        })
    }

    /// A DELETE sent under a concurrency token. See [`Self::post_form_guarded`].
    fn delete_guarded(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
        token: &str,
    ) -> Result<Response<Body>> {
        self.send_authed(|http, bearer| {
            let mut req = http
                .delete(url)
                .header("Authorization", format!("Bearer {bearer}"))
                .header("If-None-Match", token);
            for (k, v) in query {
                req = req.query(*k, *v);
            }
            Ok(req.call()?)
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Validate the token and cache `userId`/`countryCode`.
    ///
    /// # Errors
    /// On a transport failure, or a non-success status - most often an expired
    /// token, which the message calls out.
    pub fn connect(&mut self) -> Result<Session> {
        // The URL is built first so the borrow is released before the request
        // path takes `&mut self` to renew the session.
        let url = self.url("/v1/sessions");
        let mut resp = self.get_authed(&url, &[])?;
        if !resp.status().is_success() {
            // Only 401 means the session itself was refused. Any other status is
            // the service saying no for its own reasons, and telling the user to
            // sign in again would send them round a loop that cannot help.
            let status = resp.status();
            if status == 401 {
                return Err(anyhow!(Fault::SignedOut).context(format!(
                    "GET /v1/sessions -> HTTP {status} (session expired? log in again)"
                )));
            }
            bail!("GET /v1/sessions -> HTTP {status}");
        }
        let s: SessionResp = resp.body_mut().read_json()?;
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
    /// The reply carries the length of the whole listing as well as the rows,
    /// so a caller paging through it knows when to stop without having to guess
    /// from a short page.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn favorite_tracks(&mut self, offset: u32, limit: u32) -> Result<Page<Track>> {
        let sess = self.session()?.clone();
        let url = self.url(&format!("/v1/users/{}/favorites/tracks", sess.user_id));
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
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
        let fr: FavTracksResp = resp.body_mut().read_json()?;
        let mut items = Vec::with_capacity(fr.items.len());
        for row in fr.items {
            items.push(row.item.into_track());
        }
        Ok(Page {
            items,
            total: fr.total_number_of_items,
        })
    }

    /// Fetch the bytes at an absolute URL, unauthenticated, with a size ceiling.
    ///
    /// For the album cover, whose host is a public CDN rather than the API: no
    /// bearer, no session renewal, no `countryCode`. Kept general and dumb - it
    /// is handed a whole URL by [`cover_url`] rather than building one - so the
    /// one unverified thing (the URL pattern) lives in one place and this stays
    /// correct whatever that turns out to be.
    ///
    /// `cap` bounds what is read into memory: a redirect to something enormous,
    /// or a body that is not the picture it claimed, must not be able to fill
    /// the heap. A cover is tens of kilobytes; the ceiling is set well above
    /// that and well below anything that would matter.
    ///
    /// # Errors
    /// A transport failure, a non-success status, or a body past `cap`.
    pub fn fetch_bytes(&self, url: &str, cap: usize) -> Result<Vec<u8>> {
        let mut resp = self.http.get(url).call()?;
        if !resp.status().is_success() {
            bail!("image fetch -> HTTP {}", resp.status());
        }
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .take(cap as u64)
            .read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// A page of the user's own playlists.
    ///
    /// Carries the length of the whole listing, as [`Self::favorite_tracks`]
    /// does, so a caller paging through it stops on the service's count rather
    /// than on a short page.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn user_playlists(&mut self, offset: u32, limit: u32) -> Result<Page<Playlist>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "null_to_default")]
            items: Vec<PlaylistBrief>,
            #[serde(
                rename = "totalNumberOfItems",
                default,
                deserialize_with = "null_to_default"
            )]
            total_number_of_items: u32,
        }

        let sess = self.session()?.clone();
        let url = self.url(&format!("/v1/users/{}/playlists", sess.user_id));
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
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
        let r: R = resp.body_mut().read_json()?;
        Ok(Page {
            items: r
                .items
                .into_iter()
                .map(PlaylistBrief::into_playlist)
                .collect(),
            total: r.total_number_of_items,
        })
    }

    /// A page of the tracks in a playlist.
    ///
    /// The total is reported here too, but a caller need not wait for it: the
    /// playlist listing already says how many tracks a playlist holds.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status (an unknown or private `uuid` yields 404).
    pub fn playlist_tracks(&mut self, uuid: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "null_to_default")]
            items: Vec<TrackBrief>,
            #[serde(
                rename = "totalNumberOfItems",
                default,
                deserialize_with = "null_to_default"
            )]
            total_number_of_items: u32,
        }

        let sess = self.session()?.clone();
        let url = self.url(&format!("/v1/playlists/{uuid}/tracks"));
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
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
        let r: R = resp.body_mut().read_json()?;
        Ok(Page {
            items: r.items.into_iter().map(TrackBrief::into_track).collect(),
            total: r.total_number_of_items,
        })
    }

    /// A page of the mixes the service builds for this listener.
    ///
    /// The listing behind the vendor's own "my mixes" screen, which is why the
    /// answer is a screen rather than a listing and is unwrapped here. Every
    /// frontend would otherwise have to know that shape to read three fields out
    /// of it.
    ///
    /// The rows are [`Mix`], not [`Playlist`]: no track count, no duration, an
    /// opaque id, and contents that are rebuilt without anyone asking. A caller
    /// that reloads a playlist to pick up an edit has to reload a mix to find
    /// out what is in it today.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn user_mixes(&mut self, offset: u32, limit: u32) -> Result<Page<Mix>> {
        let sess = self.session()?.clone();
        let url = self.url("/v1/pages/my_collection_my_mixes");
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
            &url,
            &[
                ("countryCode", sess.country_code.as_str()),
                // Required. The screen endpoints answer per client kind, and
                // without this one they answer with nothing to show.
                ("deviceType", "BROWSER"),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("mixes -> HTTP {}", resp.status());
        }
        let screen: ScreenResp<MixBrief> = resp.body_mut().read_json()?;
        let (rows, total) = screen.first_list();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.into_mix());
        }
        Ok(Page { items, total })
    }

    /// A page of the tracks in one mix.
    ///
    /// The only place a mix's length is knowable: the mix row itself carries no
    /// count, so unlike [`Self::playlist_tracks`] the caller cannot know where
    /// this listing ends before its first page arrives.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status. An id the service does not know answers with a screen
    /// that has no list on it, which is an empty page rather than an error.
    pub fn mix_tracks(&mut self, mix_id: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        let sess = self.session()?.clone();
        let url = self.url("/v1/pages/mix");
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
            &url,
            &[
                ("mixId", mix_id),
                ("countryCode", sess.country_code.as_str()),
                ("deviceType", "BROWSER"),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("mix tracks -> HTTP {}", resp.status());
        }
        let screen: ScreenResp<ItemRow> = resp.body_mut().read_json()?;
        let (rows, total) = screen.first_list();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.item.into_track());
        }
        Ok(Page { items, total })
    }

    /// A page of tracks matching a search.
    ///
    /// **Tracks only, deliberately.** The endpoint will also return playlists,
    /// albums and artists, and this used to ask for playlists as well and hand
    /// them back for every caller to discard. Paging is why that is no longer
    /// harmless: one `offset` applies to the whole request, so advancing
    /// through the tracks would advance through the other kinds in step with
    /// them, and a caller reading one kind would silently skip most of the
    /// other. Searching a second kind means a second call with an offset of its
    /// own, which is what a frontend that wants playlist hits should add.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status. An empty result set is `Ok`, not an error.
    pub fn search_tracks(&mut self, query: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        // Defaulted throughout: an answer with no `tracks` object at all - which
        // is what a query with no hits can look like - is an empty page, not a
        // parse failure.
        #[derive(Deserialize, Default)]
        struct Wrap {
            #[serde(default, deserialize_with = "null_to_default")]
            items: Vec<TrackBrief>,
            #[serde(
                rename = "totalNumberOfItems",
                default,
                deserialize_with = "null_to_default"
            )]
            total_number_of_items: u32,
        }
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "null_to_default")]
            tracks: Wrap,
        }

        let sess = self.session()?.clone();
        let url = self.url("/v1/search");
        let (off, lim) = (offset.to_string(), limit.to_string());
        let mut resp = self.get_authed(
            &url,
            &[
                ("query", query),
                ("countryCode", sess.country_code.as_str()),
                ("limit", lim.as_str()),
                ("offset", off.as_str()),
                ("types", "TRACKS"),
            ],
        )?;
        if !resp.status().is_success() {
            bail!("search -> HTTP {}", resp.status());
        }
        let r: R = resp.body_mut().read_json()?;
        Ok(Page {
            items: r
                .tracks
                .items
                .into_iter()
                .map(TrackBrief::into_track)
                .collect(),
            total: r.tracks.total_number_of_items,
        })
    }

    /// Put a track in the user's favorites, or take it back off them.
    ///
    /// The first call in this crate that *changes* anything, so its shape is
    /// worth stating. It takes the state the caller wants rather than the verb
    /// that achieves it: a favorite is one bit, a caller always knows which way
    /// it should end up, and POST-to-add versus DELETE-to-remove is the
    /// service's spelling of that bit and belongs behind this boundary. Two
    /// methods would make every frontend write the same `if` before choosing
    /// between them.
    ///
    /// It answers with `()` for the same reason: there is no page to hand back,
    /// and the caller already holds the state it asked for. Only the failure
    /// carries information.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status - the body is included, since it carries the reason.
    pub fn set_favorite_track(&mut self, track_id: u64, favorite: bool) -> Result<()> {
        let sess = self.session()?.clone();
        let listing = self.url(&format!("/v1/users/{}/favorites/tracks", sess.user_id));
        let country = sess.country_code.clone();
        let mut resp = if favorite {
            // The id goes in a form body rather than the path: this endpoint
            // takes a comma-separated list, and one track is a list of one.
            self.post_form_authed(
                &listing,
                &[("countryCode", country.as_str())],
                &format!("trackId={track_id}"),
            )?
        } else {
            let one = format!("{listing}/{track_id}");
            self.delete_authed(&one, &[("countryCode", country.as_str())])?
        };
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "favorites/tracks -> HTTP {status}: {}",
                resp.body_mut().read_to_string().unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Make a new, empty playlist and hand it back.
    ///
    /// Unlike every other write here this one answers with something, because
    /// the caller cannot name what it just made: the uuid is the service's to
    /// choose, and without it the new playlist cannot be opened, renamed or
    /// added to. The listing the caller is showing is stale either way, so the
    /// row comes back rather than the caller being told to reload to find it.
    ///
    /// A playlist is created at the top level. The service files playlists into
    /// folders and takes a folder here; priel shows no folders, so it always
    /// says the root one rather than exposing a concept it does not display.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, on a
    /// non-success status, or if the answer carries no playlist.
    pub fn create_playlist(&mut self, title: &str) -> Result<Playlist> {
        // The one call in this crate on the newer API. The older one still
        // creates a playlist, but only the newer one files it, and a playlist
        // that is in no folder is one the vendor's own clients do not list.
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            data: Option<PlaylistBrief>,
        }

        let sess = self.session()?.clone();
        let url = self.url("/v2/my-collection/playlists/folders/create-playlist");
        let mut resp = self.put_authed(
            &url,
            &[
                ("countryCode", sess.country_code.as_str()),
                ("folderId", "root"),
                ("name", title),
                ("description", ""),
            ],
        )?;
        Self::accepted("create playlist", &mut resp)?;
        let r: R = resp.body_mut().read_json()?;
        let brief = r
            .data
            .ok_or_else(|| anyhow!("create playlist: the service made no playlist"))?;
        Ok(brief.into_playlist())
    }

    /// Give a playlist a new title.
    ///
    /// **The service has no rename.** It has an edit that replaces every field
    /// it is sent, so a request carrying only a title erases whatever
    /// description the playlist had. This reads the current one back and sends
    /// it again, which is why a rename costs two round trips. That belongs here
    /// rather than in every caller: a frontend asked the user for a title, and
    /// nothing about the title it was given says a description exists.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status from either request.
    pub fn rename_playlist(&mut self, uuid: &str, title: &str) -> Result<()> {
        let country = self.session()?.country_code.clone();
        let held = self.playlist_state(uuid)?;
        let url = self.url(&format!("/v1/playlists/{uuid}"));
        let form = form_encode(&[("title", title), ("description", &held.description)]);
        let mut resp = self.post_form_authed(&url, &[("countryCode", country.as_str())], &form)?;
        Self::accepted("rename playlist", &mut resp)
    }

    /// Delete a playlist.
    ///
    /// **This cannot be undone from here, and the service offers no way back.**
    /// A caller that shows the result before the reply arrives is showing
    /// something it cannot put back if the answer is no; unlike a favorite,
    /// this one is worth waiting for.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn delete_playlist(&mut self, uuid: &str) -> Result<()> {
        let country = self.session()?.country_code.clone();
        let url = self.url(&format!("/v1/playlists/{uuid}"));
        let mut resp = self.delete_authed(&url, &[("countryCode", country.as_str())])?;
        Self::accepted("delete playlist", &mut resp)
    }

    /// Put a track at the end of a playlist.
    ///
    /// One track, though the endpoint takes a list and this sends a list of
    /// one - the same shape [`Self::set_favorite_track`] has, and for the same
    /// reason: no caller here has more than one, and a list would make every
    /// one of them build a vector to say so.
    ///
    /// A track already in the playlist is left alone rather than added twice.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status from either request. A playlist edited elsewhere
    /// since it was read is refused rather than overwritten.
    pub fn add_track_to_playlist(&mut self, uuid: &str, track_id: u64) -> Result<()> {
        let country = self.session()?.country_code.clone();
        let held = self.playlist_state(uuid)?;
        let url = self.url(&format!("/v1/playlists/{uuid}/items"));
        let form = form_encode(&[
            ("trackIds", &track_id.to_string()),
            // A track the catalogue has withdrawn is skipped rather than failing
            // the request, and a track already there is not doubled.
            ("onArtifactNotFound", "SKIP"),
            ("onDupes", "SKIP"),
        ]);
        let mut resp = self.post_form_guarded(
            &url,
            &[("countryCode", country.as_str())],
            &form,
            &held.token,
        )?;
        Self::accepted("add to playlist", &mut resp)
    }

    /// Take a track out of a playlist.
    ///
    /// **The service removes by position, not by track.** The position is
    /// looked up here rather than taken from the caller, and that is the whole
    /// point of this method: a frontend holds a filtered, partly-loaded view of
    /// a playlist, so any index it could offer is an index into something other
    /// than the playlist. Passing one in is how the wrong track gets deleted.
    ///
    /// A playlist holding the same track twice loses the first copy.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, on a
    /// non-success status, or if the playlist does not hold that track - which
    /// is an error rather than a silent success, because the caller believed it
    /// did and is about to say so on screen.
    pub fn remove_track_from_playlist(&mut self, uuid: &str, track_id: u64) -> Result<()> {
        let country = self.session()?.country_code.clone();
        let held = self.playlist_state(uuid)?;
        let position = self
            .playlist_position(uuid, track_id)?
            .ok_or_else(|| anyhow!("track {track_id} is not in this playlist"))?;
        let url = self.url(&format!("/v1/playlists/{uuid}/items/{position}"));
        let mut resp =
            self.delete_guarded(&url, &[("countryCode", country.as_str())], &held.token)?;
        Self::accepted("remove from playlist", &mut resp)
    }

    /// Read back what a caller has to know before it may change a playlist.
    ///
    /// Both facts come from the one request because both have the same shelf
    /// life: the description is what a rename must not erase, and the token is
    /// what the service checks before applying a change of contents.
    fn playlist_state(&mut self, uuid: &str) -> Result<PlaylistState> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "null_to_default")]
            description: String,
        }

        let country = self.session()?.country_code.clone();
        let url = self.url(&format!("/v1/playlists/{uuid}"));
        let mut resp = self.get_authed(&url, &[("countryCode", country.as_str())])?;
        Self::accepted("read playlist", &mut resp)?;
        let token = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let r: R = resp.body_mut().read_json()?;
        Ok(PlaylistState {
            description: r.description,
            token,
        })
    }

    /// Where a track sits in a playlist, counting from zero, if it is there.
    ///
    /// Counted over the *items* listing rather than the tracks one, because the
    /// positions the service deletes by count everything a playlist can hold. A
    /// playlist with a video in it renumbers every track after that video, and
    /// counting only the tracks would delete the wrong row from exactly the
    /// playlists whose owner would least expect it.
    fn playlist_position(&mut self, uuid: &str, track_id: u64) -> Result<Option<u32>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "null_to_default")]
            items: Vec<PlaylistItemRow>,
            #[serde(
                rename = "totalNumberOfItems",
                default,
                deserialize_with = "null_to_default"
            )]
            total_number_of_items: u32,
        }

        let country = self.session()?.country_code.clone();
        let url = self.url(&format!("/v1/playlists/{uuid}/items"));
        let mut offset = 0u32;
        for _ in 0..PLAYLIST_SCANS_MAX {
            let (off, lim) = (offset.to_string(), PLAYLIST_ITEMS_PAGE.to_string());
            let mut resp = self.get_authed(
                &url,
                &[
                    ("countryCode", country.as_str()),
                    ("limit", lim.as_str()),
                    ("offset", off.as_str()),
                ],
            )?;
            Self::accepted("read playlist items", &mut resp)?;
            let r: R = resp.body_mut().read_json()?;
            if r.items.is_empty() {
                return Ok(None);
            }
            for (nth, row) in r.items.iter().enumerate() {
                // The kind is checked as well as the id: a video and a track can
                // wear the same number, and they are different rows.
                if row.item.id == track_id && !row.kind.eq_ignore_ascii_case("video") {
                    let seen = u32::try_from(nth).unwrap_or(u32::MAX);
                    return Ok(Some(offset.saturating_add(seen)));
                }
            }
            offset = offset.saturating_add(PLAYLIST_ITEMS_PAGE);
            if offset >= r.total_number_of_items {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Turn a non-success answer into an error carrying the reason.
    ///
    /// Shared by the writes because the reason is always in the body: "not
    /// yours", "no longer exists", and the refusal that means the playlist has
    /// changed since it was read. Reporting only the status would throw away
    /// the one sentence the user could act on.
    fn accepted(what: &str, resp: &mut Response<Body>) -> Result<()> {
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        bail!(
            "{what} -> HTTP {status}: {}",
            resp.body_mut().read_to_string().unwrap_or_default()
        )
    }

    /// Resolve a track to a playable source at the requested quality.
    ///
    /// # Errors
    /// On a transport failure, a non-success status (the response body is
    /// included, since it carries the reason a track is unplayable), or a
    /// manifest that is not valid base64, not a known mime type, or empty.
    pub fn resolve_stream(&mut self, track_id: u64, quality: Quality) -> Result<ResolvedStream> {
        let url = self.url(&format!("/v1/tracks/{track_id}/playbackinfopostpaywall"));
        let mut resp = self.get_authed(
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
                resp.body_mut().read_to_string().unwrap_or_default()
            );
        }
        let s: Stream = resp.body_mut().read_json()?;
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
        replay_gain_db: s.track_replay_gain.unwrap_or(0.0),
        peak: s.track_peak_amplitude.unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    /// A throwaway HTTP origin that serves canned responses in order and records
    /// the request lines it saw. Written against `std::net` on purpose: a stub
    /// server is not worth a dependency in a crate this small.
    struct Stub {
        base: String,
        seen: Receiver<Req>,
    }

    /// One request the stub saw: the request line, the body that followed, and
    /// the concurrency token it carried.
    ///
    /// The body is read rather than skipped for two reasons. A write is only
    /// verifiable if what it carried can be asserted - the id of the track being
    /// favorited travels in the body, not the URL - and closing a socket that
    /// still has unread bytes in it resets the connection, which can lose the
    /// answer the client is in the middle of reading.
    ///
    /// `if_none_match` is here for the same reason the body is. A playlist's
    /// contents may only be changed under the token from a fresh read of it, so
    /// a test that asserted only the verb, the path and the body would pass a
    /// write that had dropped the token - and dropping it is how one client
    /// silently overwrites another's edit.
    struct Req {
        line: String,
        body: String,
        if_none_match: Option<String>,
    }

    /// Serve canned responses in order, tagging each with an `ETag` of its own.
    ///
    /// The tag names the *answer*, not the stub, so a test can say which read a
    /// token came from. A single fixed tag would be satisfied by a client that
    /// had hard-coded one, which is the bug worth catching: the token has to be
    /// the one the service just handed back.
    fn stub(responses: Vec<(u16, String)>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            for (nth, (code, body)) in responses.into_iter().enumerate() {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(sock.try_clone().expect("clone"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut length = 0usize;
                let mut if_none_match = None;
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {
                            let lower = header.to_ascii_lowercase();
                            if let Some(v) = lower.strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                            // Matched on the lowercased name but taken from the
                            // line as sent, since the token's own case matters.
                            if lower.starts_with("if-none-match:")
                                && let Some((_, v)) = header.split_once(':')
                            {
                                if_none_match = Some(v.trim().to_string());
                            }
                        }
                    }
                }
                let mut sent = vec![0u8; length];
                if length > 0 {
                    let _ = reader.read_exact(&mut sent);
                }
                let _ = tx.send(Req {
                    line: line.trim().to_string(),
                    body: String::from_utf8_lossy(&sent).to_string(),
                    if_none_match,
                });
                let resp = format!(
                    "HTTP/1.1 {code} S\r\nContent-Type: application/json\r\n\
                     ETag: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    etag_of(nth),
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });
        Stub { base, seen }
    }

    /// The `ETag` the stub sends with its `nth` answer, counting from zero.
    fn etag_of(nth: usize) -> String {
        format!("answer-{nth}")
    }

    fn client(s: &Stub) -> Client {
        Client::new("tok".into())
            .expect("client")
            .with_base_url(s.base.clone())
    }

    /// A client that has already been through `connect`, so the session-scoped
    /// calls can be exercised without repeating the handshake in every test.
    fn connected(s: &Stub) -> Client {
        let mut c = client(s);
        c.connect().expect("connect");
        c
    }

    const SESSION: &str = r#"{"userId":7,"countryCode":"DE"}"#;

    fn ok(body: &str) -> (u16, String) {
        (200, body.to_string())
    }

    // ---- session ----

    #[test]
    fn connect_caches_the_identity_the_other_calls_need() {
        // Goal: connect both returns and stores the session, since every later
        // request interpolates the user id and country code.
        let s = stub(vec![ok(SESSION)]);
        let mut c = client(&s);
        let sess = c.connect().unwrap();
        assert_eq!(sess.user_id, 7);
        assert_eq!(sess.country_code, "DE");
        assert!(s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"));
    }

    #[test]
    fn an_unauthorised_session_says_the_token_may_have_expired() {
        // Goal: this is the error a user hits most, so the message has to point
        // at the fix rather than just report a status code.
        let s = stub(vec![(401, "no".into())]);
        let err = client(&s).connect().unwrap_err().to_string();
        assert!(err.contains("401"), "should carry the status: {err}");
        assert!(
            err.contains("session expired"),
            "should suggest a cause: {err}"
        );
        assert!(
            err.contains("log in again"),
            "and what to do about it: {err}"
        );
    }

    #[test]
    fn a_refused_session_is_a_fault_the_interface_can_act_on() {
        // Goal: "sign in again" is the one failure a user can fix from the
        // keyboard, and the interface has to recognise it without reading the
        // message. Matching on text is how a reworded sentence silently stops
        // offering the login screen.
        let s = stub(vec![(401, "no".into())]);
        let err = client(&s).connect().unwrap_err();
        assert_eq!(Fault::of(&err), Fault::SignedOut);
    }

    #[test]
    fn a_service_that_cannot_be_reached_is_not_a_refusal() {
        // Goal: a dropped network and a rejected request need different words -
        // one is worth retrying and the other is not - and only the transport
        // knows which happened.
        let mut c = Client::new(String::new())
            .expect("client")
            .with_base_url("http://127.0.0.1:1");
        let err = c.connect().unwrap_err();
        assert_eq!(Fault::of(&err), Fault::Unreachable);
    }

    #[test]
    fn an_answer_we_did_not_like_is_a_refusal() {
        // Goal: the service answered, so the connection is fine and the session
        // is fine. Neither retrying nor signing in again would help.
        let s = stub(vec![(500, "boom".into())]);
        let err = client(&s).connect().unwrap_err();
        assert_eq!(Fault::of(&err), Fault::Refused);
    }

    #[test]
    fn a_session_that_was_never_saved_is_a_signed_out_fault() {
        // Goal: a first run and an expired session lead to the same screen, so
        // they should classify the same way.
        let Err(err) = Client::from_token_file("/nonexistent/priel-token.json") else {
            panic!("a path that does not exist should not load")
        };
        assert_eq!(Fault::of(&err), Fault::SignedOut);
    }

    #[test]
    fn session_scoped_calls_refuse_to_run_before_connect() {
        // Goal: failing with a clear programmer error beats sending a request
        // with an empty user id and getting a confusing 404 back.
        let s = stub(vec![]);
        let err = client(&s).favorite_tracks(0, 10).unwrap_err().to_string();
        assert!(err.contains("connect"), "{err}");
    }

    // ---- listings ----

    #[test]
    fn favorites_are_mapped_and_paged_through_the_query_string() {
        // Goal: the wire shape is nested (items[].item) and the paging arguments
        // must actually reach the URL, or every page returns the same rows.
        let body = r#"{"items":[{"item":{"id":1,"title":"T","duration":100,
            "artists":[{"name":"A"},{"name":"B"}],
            "album":{"title":"Alb","cover":"aaaa-bbbb-cccc"},
            "audioQuality":"LOSSLESS","mediaMetadata":{"tags":["HIRES_LOSSLESS"]}}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let mut c = connected(&s);
        let page = c.favorite_tracks(20, 5).unwrap();

        assert_eq!(page.items.len(), 1);
        let t = &page.items[0];
        assert_eq!((t.id, t.duration_secs), (1, 100));
        assert_eq!(t.artist, "A", "the first artist represents the track");
        assert_eq!(t.album, "Alb");
        assert_eq!(t.cover, "aaaa-bbbb-cccc", "the album art id is kept");
        assert_eq!(t.quality, "HI-RES", "the hi-res tag wins over audioQuality");

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap().line;
        assert!(req.contains("offset=20"), "{req}");
        assert!(req.contains("limit=5"), "{req}");
        assert!(req.contains("countryCode=DE"), "{req}");
    }

    #[test]
    fn a_page_carries_how_many_rows_the_whole_listing_has() {
        // Goal: the end of a listing has to come from the service. Inferring it
        // from a short page is wrong the moment the service caps a page below
        // the limit that was asked for, and the count is already on the wire.
        let body = r#"{"totalNumberOfItems":417,"items":[{"item":{"id":1}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let page = connected(&s).favorite_tracks(0, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, 417, "the count is the end-of-list signal");
    }

    #[test]
    fn a_listing_with_no_count_reports_zero_rather_than_guessing() {
        // Goal: the field is absent from some answers. Defaulting to zero keeps
        // the caller from paging past the end on a made-up number; the caller
        // can still see the rows it was given.
        let s = stub(vec![ok(SESSION), ok(r#"{"items":[{"item":{"id":1}}]}"#)]);
        let page = connected(&s).favorite_tracks(0, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, 0);
    }

    #[test]
    fn absent_optional_track_fields_do_not_fail_the_page() {
        // Goal: the catalogue omits fields freely. One sparse row must not throw
        // away the whole listing.
        let s = stub(vec![ok(SESSION), ok(r#"{"items":[{"item":{"id":9}}]}"#)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        assert_eq!(rows[0].id, 9);
        assert_eq!(rows[0].artist, "");
        assert_eq!(rows[0].quality, "");
    }

    /// A listing row with every field the service was observed to send on one.
    ///
    /// Written from a captured answer rather than from documentation, because
    /// the point of these tests is what actually arrives. See
    /// `docs/track-fields.md` for where the capture came from and which of these
    /// keys are read.
    const FULL_ROW: &str = r#"{"id":1,"title":"Azizam","duration":162,
        "version":"Live at Wembley","explicit":true,"isrc":"GBAHS2500081",
        "copyright":"(P) 2025 A Label","allowStreaming":true,"streamReady":true,
        "artists":[{"name":"A"},{"name":"B"}],"album":{"title":"Alb"},
        "trackNumber":1,"volumeNumber":1,"popularity":80,"replayGain":-9.95,
        "peak":0.988554,"audioQuality":"LOSSLESS",
        "mediaMetadata":{"tags":["LOSSLESS"]}}"#;

    #[test]
    fn a_row_carries_more_about_a_track_than_a_row_can_show() {
        // Goal: the wire has always sent a version, an explicit marker, an isrc
        // and a copyright line on every listing row, and this crate dropped all
        // four on the floor. A frontend cannot show what the library discarded.
        let body = format!(r#"{{"items":[{{"item":{FULL_ROW}}}]}}"#);
        let s = stub(vec![ok(SESSION), ok(&body)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        let t = &rows[0];
        assert_eq!(t.version, "Live at Wembley");
        assert!(t.explicit);
        assert_eq!(t.isrc, "GBAHS2500081");
        assert_eq!(t.copyright, "(P) 2025 A Label");
    }

    #[test]
    fn every_credited_artist_is_kept_and_not_only_the_first() {
        // Goal: a collaboration credits several artists and this crate kept one,
        // so the rest were unrecoverable above it. `artist` stays the first, for
        // the places that have one line to spend.
        let body = format!(r#"{{"items":[{{"item":{FULL_ROW}}}]}}"#);
        let s = stub(vec![ok(SESSION), ok(&body)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        assert_eq!(rows[0].artist, "A");
        assert_eq!(rows[0].artists, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn a_null_where_a_string_was_expected_does_not_fail_the_page() {
        // Goal: the service sends `"version": null` on the majority of tracks -
        // it is how "no version" is spelled, not an omission. A plain `String`
        // field with `#[serde(default)]` does not survive that: serde only
        // defaults a *missing* key and rejects an explicit null, which would
        // turn the commonest row on the service into a failed listing.
        let body = r#"{"items":[{"item":{"id":9,"version":null,"isrc":null,
            "copyright":null,"title":null,"audioQuality":null}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        assert_eq!(rows[0].id, 9);
        assert_eq!(rows[0].version, "");
        assert_eq!(rows[0].isrc, "");
        assert_eq!(rows[0].copyright, "");
        assert_eq!(rows[0].title, "");
    }

    #[test]
    fn a_track_the_service_will_not_stream_is_marked_as_such() {
        // Goal: a row the service has already said it will not play should be
        // knowable before the play fails. Two wire bits say it and both have to
        // hold, since either one alone leaves a track that cannot start.
        let cases = [
            (r#""allowStreaming":false,"streamReady":true"#, false),
            (r#""allowStreaming":true,"streamReady":false"#, false),
            (r#""allowStreaming":true,"streamReady":true"#, true),
        ];
        for (fields, expected) in cases {
            let body = format!(r#"{{"items":[{{"item":{{"id":1,{fields}}}}}]}}"#);
            let s = stub(vec![ok(SESSION), ok(&body)]);
            let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
            assert_eq!(rows[0].streamable, expected, "for {fields}");
        }
    }

    #[test]
    fn a_row_that_says_nothing_about_streaming_is_assumed_playable() {
        // Goal: absence is not a refusal. Defaulting the other way would grey
        // out a whole listing the moment the service trimmed a field, which is
        // a worse failure than letting one play fail with its own message.
        let s = stub(vec![ok(SESSION), ok(r#"{"items":[{"item":{"id":9}}]}"#)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        assert!(rows[0].streamable);
    }

    #[test]
    fn every_row_names_the_radio_mix_built_around_that_track() {
        // Goal: the id under `mixes.TRACK_MIX` is on every listing row and was
        // thrown away here, so nothing above this crate could continue a queue
        // from the track that ended it. Only that one key is taken: the object
        // also carries the ids of other screens, and a mix of a track is the
        // only one anything here can play.
        let body = r#"{"items":[{"item":{"id":1,
            "mixes":{"TRACK_MIX":"0016d","MASTER_TRACK_MIX":"01699"}}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
        assert_eq!(rows[0].mix_id, "0016d");
    }

    #[test]
    fn a_null_mixes_object_does_not_fail_the_page() {
        // Goal: this shipped broken and took the whole favorites listing with
        // it. `#[serde(default)]` fills a *missing* key and rejects an explicit
        // `null`, and this service spells absence as `null`. The guard was on
        // `TRACK_MIX` inside the object and not on the object itself, which is
        // one level too shallow: the whole value comes back `null` on a track
        // the service built no radio for.
        let body = r#"{"items":[{"item":{"id":1,"mixes":null}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let rows = connected(&s)
            .favorite_tracks(0, 1)
            .expect("a null mixes object must not fail the page")
            .items;
        assert_eq!(rows[0].mix_id, "", "no radio, not a broken listing");
    }

    #[test]
    fn a_null_anywhere_the_service_spells_absence_does_not_fail_the_page() {
        // Goal: the same trap, swept for rather than patched once. Every
        // defaulted non-`Option` field had the hole - and a playlist
        // `description` is null in real captures, which #18's rename reads back
        // before writing, so that path would have failed on an ordinary
        // playlist.
        let body = r#"{"items":[{"item":{"id":1,"title":null,"album":null,
            "artists":null,"mediaMetadata":null,"mixes":null,"version":null}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let rows = connected(&s)
            .favorite_tracks(0, 1)
            .expect("nulls must not fail the page")
            .items;
        assert_eq!(rows[0].id, 1, "the row still arrives");
        assert_eq!(rows[0].mix_id, "");
        assert!(rows[0].artists.is_empty());
    }

    #[test]
    fn a_track_with_no_radio_mix_says_so_with_an_empty_id() {
        // Goal: the negative space, and the one a mix's own rows may land in -
        // they are a shorter shape than the other listings send. Absence has to
        // be an ordinary answer rather than a failed page, because the caller
        // has something to say about it and nothing to retry.
        let cases = [
            r#"{"id":9}"#,
            r#"{"id":9,"mixes":{}}"#,
            r#"{"id":9,"mixes":{"TRACK_MIX":null}}"#,
        ];
        for row in cases {
            let body = format!(r#"{{"items":[{{"item":{row}}}]}}"#);
            let s = stub(vec![ok(SESSION), ok(&body)]);
            let rows = connected(&s).favorite_tracks(0, 1).unwrap().items;
            assert_eq!(rows[0].mix_id, "", "for {row}");
        }
    }

    #[test]
    fn playlists_and_their_tracks_are_parsed() {
        // Goal: playlists key off a uuid rather than a numeric id, and the uuid
        // has to land in the path.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"items":[{"uuid":"abc","title":"Mix","numberOfTracks":3,"duration":60}]}"#),
            ok(r#"{"items":[{"id":5,"title":"X"}]}"#),
        ]);
        let mut c = connected(&s);

        let lists = c.user_playlists(0, 10).unwrap();
        assert_eq!(lists.items[0].uuid, "abc");
        assert_eq!(lists.items[0].num_tracks, 3);

        let tracks = c.playlist_tracks("abc", 0, 10).unwrap();
        assert_eq!(tracks.items[0].id, 5);

        let _ = s.seen.recv().unwrap();
        let _ = s.seen.recv().unwrap();
        assert!(
            s.seen
                .recv()
                .unwrap()
                .line
                .contains("/v1/playlists/abc/tracks")
        );
    }

    #[test]
    fn every_listing_reports_the_length_of_the_whole_listing() {
        // Goal: favorites already page off the service's own count, and the
        // other three listings send the same field. A caller that had to guess
        // the end from a short page would stop early on any listing the service
        // caps below the limit it was asked for.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"totalNumberOfItems":40,"items":[{"uuid":"abc"}]}"#),
            ok(r#"{"totalNumberOfItems":312,"items":[{"id":5}]}"#),
            ok(r#"{"tracks":{"totalNumberOfItems":900,"items":[{"id":2}]}}"#),
        ]);
        let mut c = connected(&s);

        assert_eq!(c.user_playlists(0, 10).unwrap().total, 40);
        assert_eq!(c.playlist_tracks("abc", 0, 10).unwrap().total, 312);
        assert_eq!(c.search_tracks("blue", 0, 10).unwrap().total, 900);
    }

    #[test]
    fn a_listing_with_no_count_of_its_own_reports_zero() {
        // Goal: the field is absent from some answers, on every listing. Zero
        // says "unknown" and lets the caller fall back on something it knows;
        // a guessed total would page past the end.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"items":[{"uuid":"abc"}]}"#),
            ok(r#"{"items":[{"id":5}]}"#),
            ok(r#"{"tracks":{"items":[{"id":2}]}}"#),
        ]);
        let mut c = connected(&s);

        assert_eq!(c.user_playlists(0, 10).unwrap().total, 0);
        assert_eq!(c.playlist_tracks("abc", 0, 10).unwrap().total, 0);
        assert_eq!(c.search_tracks("blue", 0, 10).unwrap().total, 0);
    }

    #[test]
    fn a_search_page_asks_for_tracks_at_the_offset_it_was_given() {
        // Goal: search took a limit and no offset at all, so every page was the
        // first one. The offset has to reach the query string, and the request
        // has to name the one result kind the page type can carry.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"tracks":{"items":[{"id":2,"title":"S"}]}}"#),
        ]);
        let page = connected(&s).search_tracks("blue", 60, 30).unwrap();
        assert_eq!(page.items[0].id, 2);

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap().line;
        assert!(req.contains("offset=60"), "{req}");
        assert!(req.contains("limit=30"), "{req}");
        assert!(
            req.contains("types=TRACKS") && !req.contains("PLAYLISTS"),
            "one offset cannot page two result kinds: {req}"
        );
    }

    #[test]
    fn a_search_with_no_hits_is_not_an_error() {
        // Goal: a query with no hits is a normal answer. Treating it as an error
        // would put a scary notice on screen for an ordinary typo.
        let s = stub(vec![ok(SESSION), ok("{}")]);
        assert!(
            connected(&s)
                .search_tracks("zzz", 0, 50)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn a_failed_listing_reports_which_call_failed() {
        // Goal: the worker turns these into a one-line notice, so the message
        // has to identify the request on its own.
        let s = stub(vec![ok(SESSION), (500, "boom".into())]);
        let err = connected(&s).user_playlists(0, 1).unwrap_err().to_string();
        assert!(err.contains("playlists") && err.contains("500"), "{err}");
    }

    // ---- mixes ----

    #[test]
    fn mixes_are_read_out_of_the_page_the_service_answers_with() {
        // Goal: this listing does not answer with a listing. It answers with the
        // rows of a screen, and the mixes are inside the first module that
        // carries a list - so the unwrapping has to happen here rather than in
        // every frontend. The paging arguments still have to reach the URL, and
        // the device type is required or the page comes back empty.
        let body = r#"{"rows":[{"modules":[{"type":"MIX_LIST","title":"My Mixes",
            "pagedList":{"limit":15,"offset":30,"totalNumberOfItems":9,"items":[
              {"id":"0007a","title":"My Mix 1","subTitle":"Miles Davis, Bill Evans",
               "mixType":"DAILY_MIX","master":false,"shortSubtitle":"Created by TIDAL"}]}}]}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let page = connected(&s).user_mixes(30, 15).unwrap();

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "0007a", "the id is opaque, not a uuid");
        assert_eq!(page.items[0].title, "My Mix 1");
        assert_eq!(
            page.items[0].subtitle, "Miles Davis, Bill Evans",
            "the subtitle is the only description of the contents on the wire"
        );
        assert_eq!(page.total, 9, "the count comes from inside the module");

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap().line;
        assert!(req.contains("/v1/pages/my_collection_my_mixes"), "{req}");
        assert!(req.contains("deviceType=BROWSER"), "{req}");
        assert!(req.contains("offset=30"), "{req}");
        assert!(req.contains("limit=15"), "{req}");
        assert!(req.contains("countryCode=DE"), "{req}");
    }

    #[test]
    fn a_mixs_tracks_come_from_the_first_module_that_holds_a_list() {
        // Goal: the page for one mix leads with a header module that carries no
        // list at all, so taking the first module would return nothing. It is
        // the first module *with* a list that holds the tracks, and they arrive
        // in the nested envelope the favorites use rather than the flat one the
        // playlist tracks use.
        let body = r#"{"rows":[
            {"modules":[{"type":"MIX_HEADER","mix":{"id":"0007a","title":"My Mix 1"}}]},
            {"modules":[{"type":"TRACK_LIST","pagedList":{"totalNumberOfItems":42,
              "items":[{"type":"track","item":{"id":5,"title":"So What","duration":545,
                "artists":[{"name":"Miles Davis"}],"album":{"title":"Kind of Blue"},
                "audioQuality":"LOSSLESS"}}]}}]}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let page = connected(&s).mix_tracks("0007a", 100, 50).unwrap();

        assert_eq!(
            page.items.len(),
            1,
            "the header module must be stepped over"
        );
        assert_eq!(page.items[0].id, 5);
        assert_eq!(page.items[0].artist, "Miles Davis");
        assert_eq!(page.items[0].duration_secs, 545);
        assert_eq!(page.total, 42);

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap().line;
        assert!(req.contains("/v1/pages/mix"), "{req}");
        assert!(req.contains("mixId=0007a"), "the mix names itself: {req}");
        assert!(req.contains("offset=100"), "{req}");
        assert!(req.contains("limit=50"), "{req}");
    }

    #[test]
    fn a_page_with_no_list_on_it_is_an_empty_page_rather_than_an_error() {
        // Goal: the negative space, and it is reachable - the service answers an
        // unknown mix with a page that has no list module on it. A listener with
        // no mixes yet gets the same answer. Neither is a failure to report.
        let s = stub(vec![ok(SESSION), ok(r#"{"rows":[]}"#), ok("{}")]);
        let mut c = connected(&s);
        assert!(c.user_mixes(0, 20).unwrap().items.is_empty());
        let page = c.mix_tracks("nope", 0, 20).unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0, "no count is zero, never a guess");
    }

    #[test]
    fn a_mix_missing_its_optional_fields_still_yields_a_row() {
        // Goal: a mix carries no track count and no duration, so the fields that
        // are left are the whole row. One of them being absent must not throw
        // away the listing it arrived in.
        let body = r#"{"rows":[{"modules":[{"pagedList":{"items":[{"id":"z"}]}}]}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let page = connected(&s).user_mixes(0, 20).unwrap();
        assert_eq!(page.items[0].id, "z");
        assert_eq!(page.items[0].title, "");
        assert_eq!(page.items[0].subtitle, "");
    }

    #[test]
    fn a_failed_mix_listing_says_which_of_the_two_calls_failed() {
        // Goal: both mix calls go to a page endpoint, so a message naming only
        // the endpoint would not say whether the listing or one mix's tracks
        // failed - and the interface puts that sentence on the notice line.
        let s = stub(vec![
            ok(SESSION),
            (500, "boom".into()),
            (500, "boom".into()),
        ]);
        let mut c = connected(&s);
        let listing = c.user_mixes(0, 1).unwrap_err().to_string();
        assert!(
            listing.contains("mixes") && listing.contains("500"),
            "{listing}"
        );
        let tracks = c.mix_tracks("0007a", 0, 1).unwrap_err().to_string();
        assert!(tracks.contains("mix tracks"), "{tracks}");
    }

    // ---- writes ----

    #[test]
    fn adding_a_favorite_posts_the_track_to_the_favorites_listing() {
        // Goal: the first request in this crate that changes something. A write
        // is only verifiable if the verb, the path and what it carried can all
        // be read back, and the id of the track travels in the body rather than
        // the URL - so a test that looked only at the path would pass whichever
        // track was sent.
        let s = stub(vec![ok(SESSION), ok("{}")]);
        connected(&s)
            .set_favorite_track(42, true)
            .expect("the add should succeed");

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap();
        assert!(
            req.line.starts_with("POST /v1/users/7/favorites/tracks"),
            "{}",
            req.line
        );
        assert!(req.line.contains("countryCode=DE"), "{}", req.line);
        assert!(
            req.body.contains("42"),
            "the track has to be in the body: {:?}",
            req.body
        );
    }

    #[test]
    fn removing_a_favorite_deletes_that_track_from_the_listing() {
        // Goal: taking a track off is a different verb *and* a different path -
        // the id moves out of the body and into the URL - so the two directions
        // cannot share an assertion.
        let s = stub(vec![ok(SESSION), ok("{}")]);
        connected(&s)
            .set_favorite_track(42, false)
            .expect("the removal should succeed");

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap();
        assert!(
            req.line
                .starts_with("DELETE /v1/users/7/favorites/tracks/42"),
            "{}",
            req.line
        );
    }

    #[test]
    fn a_refused_favorite_change_comes_back_as_an_error() {
        // Goal: the interface fills the heart before this returns and puts it
        // back when the service says no, so a refusal that read as success would
        // leave the user believing something happened that did not.
        let s = stub(vec![ok(SESSION), (403, "not yours".into())]);
        let err = connected(&s).set_favorite_track(42, true).unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert!(err.to_string().contains("not yours"), "{err}");
        assert_eq!(Fault::of(&err), Fault::Refused);
    }

    #[test]
    fn a_favorite_change_refuses_to_run_before_connect() {
        // Goal: the path interpolates the user id, so without a session this
        // would write to /v1/users//favorites/tracks and get a puzzling 404.
        let s = stub(vec![]);
        let err = client(&s)
            .set_favorite_track(1, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("connect"), "{err}");
    }

    // ---- playlist writes ----

    #[test]
    fn creating_a_playlist_names_it_and_hands_back_what_was_made() {
        // Goal: the caller has to be able to open what it just made, so the
        // uuid the service chose has to come back out. The name travels in the
        // query string on this one, not in a body, and it is the whole request -
        // a test that checked only the path would pass an empty playlist.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"data":{"uuid":"new-1","title":"Evening","numberOfTracks":0}}"#),
        ]);
        let made = connected(&s)
            .create_playlist("Evening")
            .expect("the playlist should be created");
        assert_eq!(made.uuid, "new-1", "the caller has to be able to open it");
        assert_eq!(made.title, "Evening");

        let _ = s.seen.recv().expect("session");
        let req = s.seen.recv().expect("create");
        assert!(
            req.line
                .starts_with("PUT /v2/my-collection/playlists/folders/create-playlist"),
            "{}",
            req.line
        );
        assert!(req.line.contains("name=Evening"), "{}", req.line);
        assert!(
            req.line.contains("folderId=root"),
            "a new playlist belongs at the top level: {}",
            req.line
        );
    }

    #[test]
    fn renaming_a_playlist_keeps_the_description_it_already_had() {
        // Goal: the service has no rename. It has an edit that replaces every
        // field it is sent, so a rename that sent only the title would silently
        // erase a description the user had written. The current one has to be
        // read back and sent again, and nothing above this may need to know.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"uuid":"abc","title":"Old","description":"songs for the drive"}"#),
            ok("{}"),
        ]);
        connected(&s)
            .rename_playlist("abc", "New")
            .expect("the rename should succeed");

        let _ = s.seen.recv().expect("session");
        let read = s.seen.recv().expect("read");
        assert!(
            read.line.starts_with("GET /v1/playlists/abc"),
            "{}",
            read.line
        );
        let write = s.seen.recv().expect("write");
        assert!(
            write.line.starts_with("POST /v1/playlists/abc"),
            "{}",
            write.line
        );
        assert!(write.body.contains("title=New"), "{}", write.body);
        assert!(
            write.body.contains("songs+for+the+drive")
                || write.body.contains("songs%20for%20the%20drive"),
            "the description has to survive a rename: {:?}",
            write.body
        );
    }

    #[test]
    fn deleting_a_playlist_asks_for_that_one_playlist() {
        // Goal: the one call here that cannot be taken back. It must name the
        // playlist in the path and must not be confusable with the call that
        // empties one - so the assertion pins the whole line, not a prefix of it.
        let s = stub(vec![ok(SESSION), (204, String::new())]);
        connected(&s)
            .delete_playlist("abc")
            .expect("the delete should succeed");

        let _ = s.seen.recv().expect("session");
        let req = s.seen.recv().expect("delete");
        assert!(
            req.line.starts_with("DELETE /v1/playlists/abc"),
            "{}",
            req.line
        );
        assert!(
            !req.line.contains("/items"),
            "deleting the playlist is not emptying it: {}",
            req.line
        );
    }

    #[test]
    fn adding_a_track_carries_the_token_from_the_read_that_preceded_it() {
        // Goal: a playlist's contents may only be changed under the token from a
        // fresh read of that playlist. The write has to carry the tag the read
        // just answered with - the stub tags each answer differently, so a
        // hard-coded or stale token fails here - and the track travels in the
        // body, which is the same trap the favorites write had.
        let s = stub(vec![ok(SESSION), ok(r#"{"uuid":"abc"}"#), ok("{}")]);
        connected(&s)
            .add_track_to_playlist("abc", 42)
            .expect("the add should succeed");

        let _ = s.seen.recv().expect("session");
        let read = s.seen.recv().expect("read");
        assert!(
            read.line.starts_with("GET /v1/playlists/abc"),
            "{}",
            read.line
        );
        let write = s.seen.recv().expect("write");
        assert!(
            write.line.starts_with("POST /v1/playlists/abc/items"),
            "{}",
            write.line
        );
        assert_eq!(
            write.if_none_match.as_deref(),
            Some(format!("\"{}\"", etag_of(1)).as_str()),
            "the write has to carry the token the read answered with"
        );
        assert!(
            write.body.contains("trackIds=42"),
            "the track has to be in the body: {:?}",
            write.body
        );
    }

    #[test]
    fn removing_a_track_finds_where_it_sits_before_deleting_anything() {
        // Goal: the service removes by *position*, not by track id, so the
        // position has to be looked up first. Getting this wrong deletes a
        // different track than the one asked for, which is the worst failure in
        // this crate - hence a listing where the wanted track is neither first
        // nor last, so an off-by-one cannot pass.
        let items = r#"{"totalNumberOfItems":3,"items":[
            {"type":"track","item":{"id":11}},
            {"type":"track","item":{"id":42}},
            {"type":"track","item":{"id":13}}]}"#;
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"uuid":"abc"}"#),
            ok(items),
            ok("{}"),
        ]);
        connected(&s)
            .remove_track_from_playlist("abc", 42)
            .expect("the removal should succeed");

        let _ = s.seen.recv().expect("session");
        let _ = s.seen.recv().expect("read");
        let scan = s.seen.recv().expect("scan");
        assert!(
            scan.line.starts_with("GET /v1/playlists/abc/items"),
            "positions come from the items listing, which counts videos too: {}",
            scan.line
        );
        let write = s.seen.recv().expect("write");
        assert!(
            write.line.starts_with("DELETE /v1/playlists/abc/items/1 ")
                || write.line.starts_with("DELETE /v1/playlists/abc/items/1?"),
            "the track sits at position 1, not 0 and not 42: {}",
            write.line
        );
        assert_eq!(
            write.if_none_match.as_deref(),
            Some(format!("\"{}\"", etag_of(1)).as_str()),
            "a removal is a change of contents and needs the token too"
        );
    }

    #[test]
    fn removing_a_track_the_playlist_does_not_hold_deletes_nothing() {
        // Goal: the negative space, and it is the dangerous one. Position 0 is
        // what a "not found" would collapse to if the lookup returned a number
        // rather than an absence, and that would delete the wrong track every
        // time. Nothing may go out at all.
        let items = r#"{"totalNumberOfItems":1,"items":[{"type":"track","item":{"id":11}}]}"#;
        let s = stub(vec![ok(SESSION), ok(r#"{"uuid":"abc"}"#), ok(items)]);
        let err = connected(&s)
            .remove_track_from_playlist("abc", 42)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in"), "say what was wrong: {err}");

        let _ = s.seen.recv().expect("session");
        let _ = s.seen.recv().expect("read");
        let _ = s.seen.recv().expect("scan");
        assert!(
            s.seen.try_recv().is_err(),
            "nothing may be deleted when the track was never there"
        );
    }

    #[test]
    fn a_refused_playlist_change_comes_back_as_an_error() {
        // Goal: every one of these is shown to the user as having happened
        // before the reply arrives, so a refusal that read as success would
        // leave the interface believing an edit the account never took.
        let s = stub(vec![ok(SESSION), (403, "not yours".into())]);
        let err = connected(&s).delete_playlist("abc").unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert!(err.to_string().contains("not yours"), "{err}");
        assert_eq!(Fault::of(&err), Fault::Refused);
    }

    #[test]
    fn a_playlist_edit_refuses_to_run_before_connect() {
        // Goal: every one of these interpolates the country code, and creating
        // one interpolates the user's own listing. Without a session they would
        // go out malformed and come back as a puzzling 404.
        let s = stub(vec![]);
        for err in [
            client(&s).create_playlist("x").unwrap_err().to_string(),
            client(&s)
                .rename_playlist("abc", "x")
                .unwrap_err()
                .to_string(),
            client(&s).delete_playlist("abc").unwrap_err().to_string(),
            client(&s)
                .add_track_to_playlist("abc", 1)
                .unwrap_err()
                .to_string(),
            client(&s)
                .remove_track_from_playlist("abc", 1)
                .unwrap_err()
                .to_string(),
        ] {
            assert!(err.contains("connect"), "{err}");
        }
    }

    #[test]
    fn a_title_is_encoded_so_it_cannot_become_a_second_field() {
        // Goal: a playlist title is text the user typed, and it goes into a form
        // body where `&` separates fields and `=` separates a name from a value.
        // Sent raw, a title holding either one arrives as a different title or
        // as an extra field - "Rock & Roll" would set `description` to " Roll"
        // and quietly erase the real one.
        let encoded = form_encode(&[("title", "Rock & Roll = 100%"), ("description", "café")]);
        assert_eq!(
            encoded, "title=Rock+%26+Roll+%3D+100%25&description=caf%C3%A9",
            "separators must not survive, and non-ASCII goes out as UTF-8 bytes"
        );
    }

    // ---- stream resolution ----

    fn manifest(mime: &str, payload: &str) -> String {
        let encoded = STANDARD.encode(payload);
        format!(
            r#"{{"audioQuality":"HI_RES_LOSSLESS","manifestMimeType":"{mime}",
                "manifest":"{encoded}","bitDepth":24,"sampleRate":192000}}"#
        )
    }

    #[test]
    fn a_bts_manifest_resolves_to_a_single_direct_url() {
        // Goal: BTS hands back a ready URL that mpv can fetch itself, so it must
        // not go down the segment path.
        let s = stub(vec![
            ok(SESSION),
            ok(&manifest(
                "application/vnd.tidal.bts",
                r#"{"codecs":"flac","urls":["https://cdn/one.flac"]}"#,
            )),
        ]);
        let r = connected(&s).resolve_stream(1, Quality::HiRes).unwrap();
        match r.source {
            PlayableSource::Direct(u) => assert_eq!(u, "https://cdn/one.flac"),
            PlayableSource::Segments(_) => panic!("BTS must not produce segments"),
        }
        assert_eq!((r.bit_depth, r.sample_rate), (24, 192_000));
    }

    #[test]
    fn a_dash_manifest_resolves_to_ordered_segments() {
        // Goal: DASH has to go through the MPD parser and come back as the list
        // the player concatenates.
        let mpd = r#"<MPD><SegmentTemplate media="https://cdn/$Number$.mp4"/>
                     <S d="1"/></MPD>"#;
        let s = stub(vec![
            ok(SESSION),
            ok(&manifest("application/dash+xml", mpd)),
        ]);
        let r = connected(&s).resolve_stream(1, Quality::HiRes).unwrap();
        match r.source {
            PlayableSource::Segments(u) => {
                assert_eq!(u.len(), 3);
                assert_eq!(u[0], "https://cdn/0.mp4");
            }
            PlayableSource::Direct(_) => panic!("DASH must not produce a direct url"),
        }
    }

    #[test]
    fn the_sample_rate_falls_back_to_the_manifest_when_absent() {
        // Goal: the outer response usually carries the rate, but not always;
        // the MPD's own audioSamplingRate is the backstop the badge relies on.
        let payload = STANDARD.encode(
            r#"<MPD><Representation audioSamplingRate="48000"/>
               <SegmentTemplate media="https://cdn/$Number$.mp4"/></MPD>"#,
        );
        let body =
            format!(r#"{{"manifestMimeType":"application/dash+xml","manifest":"{payload}"}}"#);
        let s = stub(vec![ok(SESSION), ok(&body)]);
        let r = connected(&s).resolve_stream(1, Quality::Lossless).unwrap();
        assert_eq!(r.sample_rate, 48_000);
        assert_eq!(r.bit_depth, 0, "unknown depth reads as zero, not a guess");
    }

    #[test]
    fn the_resolved_stream_carries_the_loudness_the_answer_reported() {
        // Goal: the playback answer has always carried the track's replay gain
        // and its peak, and only the manifest was read out of it. priel will not
        // apply either - scaling the samples is the one thing a bit-perfect path
        // may not do - but a listener can be told what the master measures.
        let payload = STANDARD.encode(r#"{"codecs":"flac","urls":["https://cdn/a.flac"]}"#);
        let body = format!(
            r#"{{"manifestMimeType":"application/vnd.tidal.bts","manifest":"{payload}",
                "trackReplayGain":-9.95,"trackPeakAmplitude":0.988554,
                "albumReplayGain":-12.41,"albumPeakAmplitude":0.999969}}"#
        );
        let s = stub(vec![ok(SESSION), ok(&body)]);
        let r = connected(&s).resolve_stream(1, Quality::Lossless).unwrap();
        assert!(
            (r.replay_gain_db - -9.95).abs() < 0.001,
            "{}",
            r.replay_gain_db
        );
        assert!((r.peak - 0.988_554).abs() < 0.001, "{}", r.peak);
    }

    #[test]
    fn a_playback_answer_with_no_loudness_figures_reports_zero() {
        // Goal: the figures are absent on the tiers that do not measure, and
        // zero reads as "nothing to say" the way the bit depth beside it already
        // does. A made-up gain is worse than none: it is indistinguishable from
        // a real one.
        let s = stub(vec![
            ok(SESSION),
            ok(&manifest(
                "application/vnd.tidal.bts",
                r#"{"codecs":"flac","urls":["https://cdn/a.flac"]}"#,
            )),
        ]);
        let r = connected(&s).resolve_stream(1, Quality::HiRes).unwrap();
        assert!((r.replay_gain_db).abs() < f32::EPSILON);
        assert!((r.peak).abs() < f32::EPSILON);
    }

    #[test]
    fn unusable_manifests_are_rejected_with_a_reason() {
        // Goal: three distinct failures that would otherwise surface as a silent
        // non-playing track: unknown container, empty url list, bad base64.
        let cases: Vec<(String, &str)> =
            vec![
            (manifest("application/octet-stream", "{}"), "manifestMimeType"),
            (
                manifest("application/vnd.tidal.bts", r#"{"urls":[]}"#),
                "no urls",
            ),
            (
                r#"{"manifestMimeType":"application/vnd.tidal.bts","manifest":"!!not base64!!"}"#
                    .to_string(),
                "base64",
            ),
        ];
        for (body, expected) in cases {
            let s = stub(vec![ok(SESSION), ok(&body)]);
            let err = connected(&s)
                .resolve_stream(1, Quality::HiRes)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "expected {expected:?} in {err:?}");
        }
    }

    #[test]
    fn a_rejected_playback_request_includes_the_service_reason() {
        // Goal: the body explains *why* a track is unplayable (region, tier), so
        // it must be carried into the error rather than dropped for the status.
        let s = stub(vec![
            ok(SESSION),
            (403, "not available in your country".into()),
        ]);
        let err = connected(&s)
            .resolve_stream(1, Quality::HiRes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn the_requested_quality_reaches_the_query() {
        // Goal: asking for hi-res and silently receiving lossless is the whole
        // point of the app, so the tier must be on the wire.
        let s = stub(vec![
            ok(SESSION),
            ok(&manifest(
                "application/vnd.tidal.bts",
                r#"{"urls":["https://cdn/a"]}"#,
            )),
        ]);
        let _ = connected(&s).resolve_stream(42, Quality::HiRes);
        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap().line;
        assert!(
            req.contains("/v1/tracks/42/playbackinfopostpaywall"),
            "{req}"
        );
        assert!(req.contains("audioquality=HI_RES_LOSSLESS"), "{req}");
    }

    // ---- pure helpers ----

    #[test]
    fn quality_tiers_map_to_the_api_spelling() {
        // Goal: these strings are protocol, not display text.
        assert_eq!(Quality::Low.as_api_str(), "LOW");
        assert_eq!(Quality::High.as_api_str(), "HIGH");
        assert_eq!(Quality::Lossless.as_api_str(), "LOSSLESS");
        assert_eq!(Quality::HiRes.as_api_str(), "HI_RES_LOSSLESS");
    }

    #[test]
    fn the_quality_label_prefers_the_tag_over_the_field() {
        // Goal: pin the precedence the badge depends on. A hi-res track is often
        // still marked LOSSLESS in audioQuality, and showing that would be wrong.
        assert_eq!(
            quality_label(&["HIRES_LOSSLESS".into()], "LOSSLESS"),
            "HI-RES"
        );
        assert_eq!(quality_label(&["hires".into()], ""), "HI-RES");
        assert_eq!(quality_label(&["LOSSLESS".into()], "HIGH"), "LOSSLESS");
        assert_eq!(quality_label(&[], "lossless"), "LOSSLESS");
        assert_eq!(quality_label(&[], "HIGH"), "HIGH");
        assert_eq!(
            quality_label(&[], "LOW"),
            "LOW",
            "unknown tiers pass through"
        );
        assert_eq!(quality_label(&[], ""), "", "nothing known means no badge");
    }

    #[test]
    fn a_token_file_is_read_or_explained() {
        // Goal: the three startup outcomes. Both failures name the file, because
        // this runs before the UI exists and the message is all the user gets.
        let dir = std::env::temp_dir().join(format!("priel-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"access_token":"abc","other":1}"#).unwrap();
        assert!(Client::from_token_file(good.to_str().unwrap()).is_ok());

        let bad = dir.join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        let err = Client::from_token_file(bad.to_str().unwrap())
            .err()
            .expect("malformed json must fail")
            .to_string();
        assert!(err.contains("parsing token file"), "{err}");

        let err = Client::from_token_file(dir.join("nope.json").to_str().unwrap())
            .err()
            .expect("a missing file must fail")
            .to_string();
        assert!(err.contains("reading token file"), "{err}");
        assert!(
            err.contains("not signed in"),
            "should say how to fix it: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_token_path_is_xdg_shaped() {
        // Goal: priel keeps its session in its own directory. Writing into
        // another application's config would mean mutating its state on every
        // token refresh. Not asserted against a fixed prefix because the base
        // follows the caller's environment.
        let p = Client::default_token_path();
        assert!(p.ends_with("/priel/token.json"), "{p}");
        assert!(p.starts_with('/'), "must be absolute: {p}");
        assert!(
            !p.contains("hiresti"),
            "must not reach into another app: {p}"
        );
        // Asserted against the resolved directories rather than a literal, so
        // it holds however XDG_STATE_HOME is set - and so that a session
        // silently landing back in the config directory fails here. It did
        // once: an edit that looked applied had not been.
        assert!(
            p.starts_with(&auth::state_dir()),
            "a session is state, not configuration: {p}"
        );
        assert!(
            !p.starts_with(&auth::config_dir()),
            "and must not be in the config directory: {p}"
        );
    }

    // ---- self-renewing sessions ----

    /// A token file and a client wired to refresh against `base`.
    fn authed(base: &str, expiry_epoch: i64) -> (Client, String) {
        let dir = std::env::temp_dir().join(format!(
            "priel-auth-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("token.json").to_str().expect("path").to_string();
        auth::StoredToken {
            access_token: "stale".into(),
            refresh_token: "rt".into(),
            token_type: "Bearer".into(),
            expiry_time: auth::format_expiry(expiry_epoch),
            is_pkce: true,
        }
        .save(&path)
        .expect("write token");

        let mut cfg = auth::AuthConfig::new("cid".into(), Some("secret".into()));
        cfg.token_url = format!("{base}/token");
        let client = Client::with_auth(&path, cfg)
            .expect("client")
            .with_base_url(base.to_string());
        (client, path)
    }

    #[test]
    fn an_expired_session_is_renewed_before_the_request_goes_out() {
        // Goal: the whole point of the feature. A token past its expiry must be
        // refreshed *before* it is used, not after a failure.
        let s = stub(vec![
            ok(r#"{"access_token":"fresh","expires_in":3600}"#),
            ok(SESSION),
        ]);
        let (mut c, path) = authed(&s.base, auth::now_epoch() - 10);

        c.connect().expect("connect");

        let first = s.seen.recv().expect("a request should have been made");
        assert!(
            first.line.starts_with("POST /token"),
            "refresh comes first: {}",
            first.line
        );
        assert!(s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"));

        let saved = auth::StoredToken::load(&path).expect("reload");
        assert_eq!(
            saved.access_token, "fresh",
            "the new token must be persisted"
        );
        assert_eq!(
            saved.refresh_token, "rt",
            "the refresh token is carried over"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_session_with_time_left_is_not_refreshed() {
        // Goal: refreshing on every request would be a pointless round trip on
        // the hot path, and would hammer the token endpoint.
        let s = stub(vec![ok(SESSION)]);
        let (mut c, path) = authed(&s.base, auth::now_epoch() + 3_600);

        c.connect().expect("connect");
        assert!(
            s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"),
            "no refresh should have preceded it"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_rejected_request_is_retried_once_after_a_refresh() {
        // Goal: a token can be revoked before its stated expiry - a session
        // ended on another device, say. The stored expiry says it is fine, so
        // only the 401 reveals it.
        let s = stub(vec![
            (401, "expired".into()),
            ok(r#"{"access_token":"fresh","expires_in":3600}"#),
            ok(SESSION),
        ]);
        let (mut c, path) = authed(&s.base, auth::now_epoch() + 3_600);

        let sess = c.connect().expect("the retry should have succeeded");
        assert_eq!(sess.user_id, 7);

        assert!(
            s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"),
            "first attempt"
        );
        assert!(
            s.seen.recv().unwrap().line.starts_with("POST /token"),
            "then a refresh"
        );
        assert!(
            s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"),
            "then a retry"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_write_survives_a_session_rejected_mid_flight_just_as_a_read_does() {
        // Goal: the refresh-and-retry used to be written into the GET path
        // itself. A write bolted on beside it would be the one request that
        // died on a revoked session - and it is the request that changes the
        // user's library, so failing it silently is the worst place for it.
        let s = stub(vec![
            ok(SESSION),
            (401, "expired".into()),
            ok(r#"{"access_token":"fresh","expires_in":3600}"#),
            ok("{}"),
        ]);
        let (mut c, path) = authed(&s.base, auth::now_epoch() + 3_600);
        c.connect().expect("connect");

        c.set_favorite_track(9, true)
            .expect("the retry should have succeeded");

        assert!(
            s.seen.recv().unwrap().line.starts_with("GET /v1/sessions"),
            "the handshake"
        );
        assert!(
            s.seen
                .recv()
                .unwrap()
                .line
                .starts_with("POST /v1/users/7/favorites/tracks"),
            "then the write, on the token that was about to be refused"
        );
        assert!(
            s.seen.recv().unwrap().line.starts_with("POST /token"),
            "then a refresh"
        );
        assert!(
            s.seen
                .recv()
                .unwrap()
                .line
                .starts_with("POST /v1/users/7/favorites/tracks"),
            "then the write again"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_dead_refresh_token_is_reported_as_needing_a_login() {
        // Goal: refresh tokens expire too. The message has to say what the user
        // must actually do, because no amount of retrying will fix it.
        let s = stub(vec![
            (401, "expired".into()),
            (400, r#"{"error":"invalid_grant"}"#.into()),
        ]);
        let (mut c, path) = authed(&s.base, auth::now_epoch() + 3_600);

        let err = c.connect().unwrap_err().to_string();
        assert!(err.contains("log in again"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_client_without_credentials_still_works_but_cannot_renew() {
        // Goal: credentials are optional. Without them priel runs from the
        // stored token, and says plainly that it cannot refresh rather than
        // failing in some confusing way.
        let s = stub(vec![ok(SESSION)]);
        let mut c = client(&s);
        assert!(c.session_expires_in().is_none());
        c.connect().expect("connect");

        let err = c.refresh_session().unwrap_err().to_string();
        assert!(err.contains("no client credentials"), "{err}");
    }

    #[test]
    fn the_stored_expiry_is_visible_for_the_interface_to_show() {
        // Goal: the UI wants to warn before a session dies rather than after.
        let s = stub(vec![]);
        let (c, path) = authed(&s.base, auth::now_epoch() + 1_800);
        let left = c.session_expires_in().expect("a known expiry");
        assert!((1_700..=1_800).contains(&left), "got {left}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn image_bytes_come_back_whole_and_are_capped() {
        // Goal: `fetch_bytes` reads an absolute URL with no auth and no session,
        // and the ceiling bounds what a hostile or broken response can put on
        // the heap. Method: serve a known body from the stub and read it under
        // caps above and below its length.
        let body = "\u{ffff}JPEGISH-BYTES";
        let s = stub(vec![ok(body), ok(body)]);
        let c = client(&s);

        let whole = c.fetch_bytes(&s.base, 4096).expect("a body");
        assert_eq!(whole, body.as_bytes(), "the picture comes back whole");

        let clipped = c.fetch_bytes(&s.base, 4).expect("a capped body");
        assert_eq!(clipped.len(), 4, "the cap is a ceiling on what is read");
    }

    #[test]
    fn the_update_check_reports_a_newer_release_and_stays_quiet_otherwise() {
        // Goal: the whole update check end to end - fetch the release JSON, read
        // its tag, and report it only when it is newer than what is running.
        // Method: serve a release document from the stub and ask against a lower
        // and a higher current version.
        let body = r#"{"tag_name":"v0.2.0","name":"0.2.0"}"#;
        let s = stub(vec![ok(body), ok(body)]);
        let c = client(&s);

        assert_eq!(
            update::newer_release(&c, &s.base, "0.1.0"),
            Some("v0.2.0".to_string()),
            "a newer release on the forge is reported with its tag"
        );
        assert_eq!(
            update::newer_release(&c, &s.base, "0.2.0"),
            None,
            "the same version is nothing to report"
        );
    }

    #[test]
    fn no_releases_yet_is_nothing_to_report_rather_than_an_error() {
        // Goal: before the first release is cut, `/releases/latest` answers 404
        // with a message body. That is "nothing to update to", not a failure the
        // listener has to see.
        let s = stub(vec![(404, r#"{"message":"Not Found"}"#.to_string())]);
        let c = client(&s);
        assert_eq!(update::newer_release(&c, &s.base, "0.1.0"), None);
    }

    #[test]
    fn a_cover_id_becomes_a_square_image_url_and_an_empty_one_becomes_nothing() {
        // Goal: the one transformation `cover_url` makes - dashes to slashes,
        // wrapped in the image host and a square size. Absent art is None rather
        // than a URL that would 404, so the caller draws nothing rather than
        // chasing a dead link. The pattern itself is unverified against a live
        // response; this pins its shape, which is what a fix would keep.
        assert_eq!(
            cover_url("1234-5678-9abc", 80).as_deref(),
            Some("https://resources.tidal.com/images/1234/5678/9abc/80x80.jpg"),
            "dashes become path separators and the size is square"
        );
        assert_eq!(cover_url("", 80), None, "no cover, no URL");
    }
}
