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
use ureq::http::Response;
use ureq::{Agent, Body};

pub mod auth;
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

/// An HTTP agent configured the way this crate expects.
///
/// Exposed so callers that need a one-off request - fetching a client identity
/// before any `Client` exists, say - do not have to reinvent the settings.
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

    /// A GET that renews the session when it has to.
    ///
    /// Proactive refresh handles the common case; the retry covers a token the
    /// service rejected earlier than its stated expiry, which happens when a
    /// session is revoked elsewhere.
    fn get_authed(&mut self, url: &str, query: &[(&str, &str)]) -> Result<Response<Body>> {
        self.ensure_fresh()?;
        let resp = self.get(url, query)?;
        if resp.status() != 401 || self.auth.is_none() {
            return Ok(resp);
        }
        drop(resp);
        self.refresh_session()
            .context("the session was rejected and could not be renewed; log in again")?;
        self.get(url, query)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<Response<Body>> {
        let mut req = self
            .http
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token));
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        Ok(req.call()?)
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
            bail!(
                "GET /v1/sessions -> HTTP {} (session expired? log in again)",
                resp.status()
            );
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
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn favorite_tracks(&mut self, offset: u32, limit: u32) -> Result<Vec<Track>> {
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
        Ok(fr.items.into_iter().map(|i| i.item.into_track()).collect())
    }

    /// The user's own playlists.
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status.
    pub fn user_playlists(&mut self, offset: u32, limit: u32) -> Result<Vec<Playlist>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            items: Vec<PlaylistBrief>,
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
    pub fn playlist_tracks(&mut self, uuid: &str, offset: u32, limit: u32) -> Result<Vec<Track>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            items: Vec<TrackBrief>,
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
        Ok(r.items.into_iter().map(TrackBrief::into_track).collect())
    }

    /// Multi-type search (tracks + playlists).
    ///
    /// # Errors
    /// If [`Self::connect`] has not run, on a transport failure, or on a
    /// non-success status. An empty result set is `Ok`, not an error.
    pub fn search(&mut self, query: &str, limit: u32) -> Result<SearchResults> {
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

        let sess = self.session()?.clone();
        let url = self.url("/v1/search");
        let lim = limit.to_string();
        let mut resp = self.get_authed(
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
        let r: R = resp.body_mut().read_json()?;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    /// A throwaway HTTP origin that serves canned responses in order and records
    /// the request lines it saw. Written against `std::net` on purpose: a stub
    /// server is not worth a dependency in a crate this small.
    struct Stub {
        base: String,
        seen: Receiver<String>,
    }

    fn stub(responses: Vec<(u16, String)>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            for (code, body) in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(sock.try_clone().expect("clone"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                let _ = tx.send(line.trim().to_string());
                let resp = format!(
                    "HTTP/1.1 {code} S\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });
        Stub { base, seen }
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
        assert!(s.seen.recv().unwrap().starts_with("GET /v1/sessions"));
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
            "artists":[{"name":"A"},{"name":"B"}],"album":{"title":"Alb"},
            "audioQuality":"LOSSLESS","mediaMetadata":{"tags":["HIRES_LOSSLESS"]}}}]}"#;
        let s = stub(vec![ok(SESSION), ok(body)]);
        let mut c = connected(&s);
        let tracks = c.favorite_tracks(20, 5).unwrap();

        assert_eq!(tracks.len(), 1);
        let t = &tracks[0];
        assert_eq!((t.id, t.duration), (1, 100));
        assert_eq!(t.artist, "A", "the first artist represents the track");
        assert_eq!(t.album, "Alb");
        assert_eq!(t.quality, "HI-RES", "the hi-res tag wins over audioQuality");

        let _ = s.seen.recv().unwrap();
        let req = s.seen.recv().unwrap();
        assert!(req.contains("offset=20"), "{req}");
        assert!(req.contains("limit=5"), "{req}");
        assert!(req.contains("countryCode=DE"), "{req}");
    }

    #[test]
    fn absent_optional_track_fields_do_not_fail_the_page() {
        // Goal: the catalogue omits fields freely. One sparse row must not throw
        // away the whole listing.
        let s = stub(vec![ok(SESSION), ok(r#"{"items":[{"item":{"id":9}}]}"#)]);
        let tracks = connected(&s).favorite_tracks(0, 1).unwrap();
        assert_eq!(tracks[0].id, 9);
        assert_eq!(tracks[0].artist, "");
        assert_eq!(tracks[0].quality, "");
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
        assert_eq!(lists[0].uuid, "abc");
        assert_eq!(lists[0].num_tracks, 3);

        let tracks = c.playlist_tracks("abc", 0, 10).unwrap();
        assert_eq!(tracks[0].id, 5);

        let _ = s.seen.recv().unwrap();
        let _ = s.seen.recv().unwrap();
        assert!(s.seen.recv().unwrap().contains("/v1/playlists/abc/tracks"));
    }

    #[test]
    fn search_returns_both_kinds_and_an_empty_result_is_not_an_error() {
        // Goal: a query with no hits is a normal answer. Treating it as an error
        // would put a scary notice on screen for an ordinary typo.
        let s = stub(vec![
            ok(SESSION),
            ok(r#"{"tracks":{"items":[{"id":2,"title":"S"}]},
                  "playlists":{"items":[{"uuid":"u","title":"P"}]}}"#),
            ok("{}"),
        ]);
        let mut c = connected(&s);

        let hits = c.search("blue", 50).unwrap();
        assert_eq!(hits.tracks.len(), 1);
        assert_eq!(hits.playlists.len(), 1);

        let empty = c.search("zzz", 50).unwrap();
        assert!(empty.tracks.is_empty() && empty.playlists.is_empty());
    }

    #[test]
    fn a_failed_listing_reports_which_call_failed() {
        // Goal: the worker turns these into a one-line notice, so the message
        // has to identify the request on its own.
        let s = stub(vec![ok(SESSION), (500, "boom".into())]);
        let err = connected(&s).user_playlists(0, 1).unwrap_err().to_string();
        assert!(err.contains("playlists") && err.contains("500"), "{err}");
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
        let req = s.seen.recv().unwrap();
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
            first.starts_with("POST /token"),
            "refresh comes first: {first}"
        );
        assert!(s.seen.recv().unwrap().starts_with("GET /v1/sessions"));

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
            s.seen.recv().unwrap().starts_with("GET /v1/sessions"),
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
            s.seen.recv().unwrap().starts_with("GET /v1/sessions"),
            "first attempt"
        );
        assert!(
            s.seen.recv().unwrap().starts_with("POST /token"),
            "then a refresh"
        );
        assert!(
            s.seen.recv().unwrap().starts_with("GET /v1/sessions"),
            "then a retry"
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
}
