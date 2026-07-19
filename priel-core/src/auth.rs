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

//! OAuth 2.0 authorization code flow with PKCE (RFC 7636).
//!
//! **priel ships no client credentials.** [`AuthConfig`] carries the client
//! identity and every caller must supply one; there is no default in this
//! repository and none should be added without a deliberate decision. The
//! endpoints are public URLs and are defaulted, the credentials are not.
//!
//! The redirect target is a page on the service's own site rather than a
//! loopback address priel could listen on, so the flow cannot complete on its
//! own: the user logs in, lands on a page, and hands the resulting URL back.
//! [`code_from_redirect`] is that step.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use ureq::Agent;

/// RFC 3986 unreserved characters stay literal; everything else is escaped.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Refresh this long before the token actually expires, so a request is never
/// racing the clock.
pub const REFRESH_MARGIN_SECS: i64 = 5 * 60;

/// Where to authenticate and as whom.
///
/// `client_id` and `client_secret` have no defaults on purpose - see the module
/// documentation.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub client_id: String,
    /// Some clients require one at the token endpoint even under PKCE.
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub token_url: String,
    pub scope: String,
}

impl AuthConfig {
    /// The service's published endpoints, with the client identity supplied by
    /// the caller.
    #[must_use]
    pub fn new(client_id: String, client_secret: Option<String>) -> Self {
        Self {
            client_id,
            client_secret,
            redirect_uri: "https://tidal.com/android/login/auth".to_string(),
            authorize_url: "https://login.tidal.com/authorize".to_string(),
            token_url: "https://auth.tidal.com/v1/oauth2/token".to_string(),
            scope: "r_usr+w_usr+w_sub".to_string(),
        }
    }
}

/// A PKCE verifier and its S256 challenge.
///
/// The verifier must survive from building the authorize URL until the code is
/// exchanged, which is a separate user-driven step, so it is a value the caller
/// holds rather than something regenerated per request.
#[derive(Clone, Debug)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// # Errors
    /// If the system random source is unavailable.
    pub fn generate() -> Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| anyhow!("no system randomness: {e}"))?;
        let verifier = URL_SAFE_NO_PAD.encode(seed);
        // The challenge hashes the verifier *string*, not the bytes behind it.
        // Hashing the raw seed instead produces a challenge the server rejects,
        // and only at the final exchange, long after the mistake.
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest.as_ref());
        Ok(Self {
            verifier,
            challenge,
        })
    }

    #[must_use]
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// A random per-login identifier the service expects alongside the challenge.
///
/// # Errors
/// If the system random source is unavailable.
pub fn client_unique_key() -> Result<String> {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("no system randomness: {e}"))?;
    Ok(bytes.iter().fold(String::with_capacity(16), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    }))
}

/// Build the URL the user must open in a browser.
#[must_use]
pub fn authorize_url(cfg: &AuthConfig, pkce: &Pkce, unique_key: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("redirect_uri", cfg.redirect_uri.as_str()),
        ("client_id", cfg.client_id.as_str()),
        ("lang", "EN"),
        ("appMode", "android"),
        ("client_unique_key", unique_key),
        ("code_challenge", pkce.challenge()),
        ("code_challenge_method", "S256"),
        ("restrict_signup", "true"),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, QUERY)))
        .collect();
    format!("{}?{}", cfg.authorize_url, query.join("&"))
}

/// Pull the authorization code out of the URL the user pasted back.
///
/// # Errors
/// If the URL carries no `code`, or carries an `error` instead - the service
/// reports a refusal that way, and reporting "no code found" for it would send
/// the user looking in the wrong place.
pub fn code_from_redirect(url: &str) -> Result<String> {
    let query = url
        .split_once('?')
        .map(|(_, q)| q)
        .ok_or_else(|| anyhow!("that does not look like a redirect URL: it has no query string"))?;

    let mut code = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_encoding::percent_decode_str(value)
            .decode_utf8_lossy()
            .into_owned();
        match key {
            "code" => code = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(e) = error {
        bail!("the login was refused: {e}");
    }
    code.filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("no authorization code in that URL"))
}

/// Tokens as returned by the token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// A saved session, in the on-disk shape hiresTI already writes.
///
/// Deliberately compatible: an existing login keeps working, and a token this
/// crate refreshes stays readable by that tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    /// Naive UTC, `YYYY-MM-DDTHH:MM:SS.ffffff`.
    pub expiry_time: String,
    pub is_pkce: bool,
}

impl StoredToken {
    /// Seconds until expiry, negative once past it. `None` if the stored stamp
    /// cannot be read, which the caller should treat as "refresh now".
    #[must_use]
    pub fn seconds_remaining(&self, now_epoch: i64) -> Option<i64> {
        parse_expiry(&self.expiry_time).map(|t| t - now_epoch)
    }

    /// Should this token be refreshed before use?
    #[must_use]
    pub fn needs_refresh(&self, now_epoch: i64) -> bool {
        self.seconds_remaining(now_epoch)
            .is_none_or(|left| left <= REFRESH_MARGIN_SECS)
    }
}

fn build(resp: TokenResponse, fallback_refresh: &str, now_epoch: i64) -> StoredToken {
    let lifetime = resp.expires_in.unwrap_or(0);
    StoredToken {
        access_token: resp.access_token,
        // A refresh response need not repeat the refresh token; keeping the old
        // one is what lets a session survive more than one refresh.
        refresh_token: resp
            .refresh_token
            .unwrap_or_else(|| fallback_refresh.to_string()),
        token_type: resp.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expiry_time: format_expiry(now_epoch + lifetime),
        is_pkce: true,
    }
}

fn post_form(agent: &Agent, url: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
    // Take the response even when the status is an error: the body is where the
    // reason lives, and `invalid_grant` versus `invalid_client` is the
    // difference between "log in again" and "these credentials are wrong".
    // Overridden per request so this does not depend on how the caller built
    // the agent.
    let mut resp = agent
        .post(url)
        .config()
        .http_status_as_error(false)
        .build()
        .content_type("application/x-www-form-urlencoded")
        .send(encode_form(form))
        .context("the token endpoint could not be reached")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        bail!("token endpoint -> HTTP {status}: {body}");
    }
    resp.body_mut()
        .read_json::<TokenResponse>()
        .context("the token endpoint returned something unexpected")
}

fn encode_form(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, QUERY)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Trade an authorization code for tokens.
///
/// # Errors
/// On a transport failure, a non-success status (the body is included, since it
/// carries the reason), or a response that is not the expected JSON.
pub fn exchange_code(
    agent: &Agent,
    cfg: &AuthConfig,
    code: &str,
    pkce: &Pkce,
    unique_key: &str,
    now_epoch: i64,
) -> Result<StoredToken> {
    let resp = post_form(
        agent,
        &cfg.token_url,
        &[
            ("code", code),
            ("client_id", &cfg.client_id),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &cfg.redirect_uri),
            ("scope", &cfg.scope),
            ("code_verifier", pkce.verifier()),
            ("client_unique_key", unique_key),
        ],
    )?;
    Ok(build(resp, "", now_epoch))
}

/// Exchange a refresh token for a fresh access token.
///
/// # Errors
/// As [`exchange_code`]. A refused refresh usually means the refresh token has
/// expired too and a full login is required.
pub fn refresh(
    agent: &Agent,
    cfg: &AuthConfig,
    refresh_token: &str,
    now_epoch: i64,
) -> Result<StoredToken> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", cfg.client_id.as_str()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret.as_str()));
    }
    let resp = post_form(agent, &cfg.token_url, &form)?;
    Ok(build(resp, refresh_token, now_epoch))
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---- naive-UTC timestamps ----
//
// The stored format is Python's `datetime.utcnow().isoformat()`. Converting it
// by hand rather than taking a date-time dependency for one field each way.

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render an epoch second as `YYYY-MM-DDTHH:MM:SS.ffffff`.
#[must_use]
pub fn format_expiry(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.000000")
}

/// Read a `YYYY-MM-DDTHH:MM:SS[.ffffff]` stamp as an epoch second.
#[must_use]
pub fn parse_expiry(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut date = date.split('-');
    let y: i64 = date.next()?.parse().ok()?;
    let m: i64 = date.next()?.parse().ok()?;
    let d: i64 = date.next()?.parse().ok()?;
    // Trailing fractional seconds are dropped: sub-second precision is noise
    // against a token lifetime measured in hours.
    let time = time.split('.').next()?;
    let mut time = time.split(':');
    let hh: i64 = time.next()?.parse().ok()?;
    let mm: i64 = time.next()?.parse().ok()?;
    let ss: i64 = time.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    fn cfg() -> AuthConfig {
        AuthConfig::new("test-client".into(), Some("test-secret".into()))
    }

    /// A one-shot token endpoint that records the form body it was posted.
    struct Stub {
        url: String,
        seen: Receiver<String>,
    }

    fn stub(status: u16, body: &'static str) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(sock.try_clone().expect("clone"));
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                match reader.read_line(&mut h) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if h == "\r\n" => break,
                    Ok(_) => {
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
            let mut form = vec![0u8; len];
            let _ = reader.read_exact(&mut form);
            let _ = tx.send(String::from_utf8_lossy(&form).into_owned());
            let resp = format!(
                "HTTP/1.1 {status} S\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        Stub { url, seen }
    }

    // ---- PKCE ----

    #[test]
    fn the_challenge_is_the_sha256_of_the_verifier_string() {
        // Goal: the one detail that fails late and silently. Hashing the random
        // bytes instead of the verifier *text* yields a well-formed challenge
        // the server rejects only at the final exchange, after the user has
        // already logged in. Checked against RFC 7636's own test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(digest.as_ref()),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "this is the appendix B vector from RFC 7636"
        );
    }

    #[test]
    fn each_login_gets_a_fresh_verifier_and_matching_challenge() {
        // Goal: reusing a verifier across logins would let a captured challenge
        // be replayed, and the pair must actually correspond.
        let a = Pkce::generate().expect("generate");
        let b = Pkce::generate().expect("generate");
        assert_ne!(a.verifier(), b.verifier(), "verifiers must not repeat");

        let digest = ring::digest::digest(&ring::digest::SHA256, a.verifier().as_bytes());
        assert_eq!(a.challenge(), URL_SAFE_NO_PAD.encode(digest.as_ref()));
        assert_eq!(a.verifier().len(), 43, "32 bytes, base64url, unpadded");
        assert!(
            !a.verifier().contains('=') && !a.challenge().contains('='),
            "padding is not allowed in a PKCE parameter"
        );
    }

    #[test]
    fn the_unique_key_is_random_hex() {
        // Goal: the service expects a per-login identifier; a constant would
        // make every login look like the same one.
        let a = client_unique_key().expect("key");
        let b = client_unique_key().expect("key");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    // ---- the authorize URL ----

    #[test]
    fn the_authorize_url_carries_everything_the_server_needs() {
        // Goal: this URL is handed to a browser, so a missing or unescaped
        // parameter surfaces as an opaque login failure.
        let pkce = Pkce::generate().expect("generate");
        let url = authorize_url(&cfg(), &pkce, "abc123");
        assert!(
            url.starts_with("https://login.tidal.com/authorize?"),
            "{url}"
        );
        for expected in [
            "response_type=code",
            "client_id=test-client",
            "code_challenge_method=S256",
            "client_unique_key=abc123",
        ] {
            assert!(url.contains(expected), "missing {expected} in {url}");
        }
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge())));
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Ftidal.com%2Fandroid%2Flogin%2Fauth"),
            "the redirect must be escaped, not pasted raw: {url}"
        );
    }

    // ---- the pasted redirect ----

    #[test]
    fn the_code_is_lifted_out_of_the_pasted_url() {
        // Goal: the user pastes a whole address bar. The code may sit anywhere
        // among the parameters.
        assert_eq!(
            code_from_redirect("https://tidal.com/android/login/auth?code=XYZ789&state=1").unwrap(),
            "XYZ789"
        );
        assert_eq!(
            code_from_redirect("https://tidal.com/x?foo=bar&code=A%2FB").unwrap(),
            "A/B",
            "percent escapes must be decoded"
        );
    }

    #[test]
    fn a_refusal_is_reported_as_a_refusal() {
        // Goal: the service reports a declined login with `error`. Saying "no
        // code found" would send the user hunting for a copy-paste mistake they
        // did not make.
        let err = code_from_redirect("https://tidal.com/x?error=access_denied")
            .unwrap_err()
            .to_string();
        assert!(err.contains("refused"), "{err}");
        assert!(err.contains("access_denied"), "{err}");
    }

    #[test]
    fn obviously_wrong_pastes_are_explained() {
        // Goal: the likeliest user error is pasting the login page rather than
        // the page they were redirected to.
        let err = code_from_redirect("https://login.tidal.com/authorize")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no query string"), "{err}");
        assert!(
            code_from_redirect("https://tidal.com/x?code=").is_err(),
            "empty"
        );
    }

    // ---- token exchange ----

    #[test]
    fn a_code_is_exchanged_for_a_stored_session() {
        // Goal: the whole point of the flow. The verifier must be sent (that is
        // what proves we started the login) and the expiry must be recorded as
        // an absolute time, not a duration.
        let s = stub(
            200,
            r#"{"access_token":"at","refresh_token":"rt","token_type":"Bearer","expires_in":3600}"#,
        );
        let mut cfg = cfg();
        cfg.token_url = s.url.clone();
        let pkce = Pkce::generate().expect("generate");

        let token = exchange_code(
            &Agent::new_with_defaults(),
            &cfg,
            "CODE",
            &pkce,
            "key",
            1_000,
        )
        .expect("exchange");
        assert_eq!(token.access_token, "at");
        assert_eq!(token.refresh_token, "rt");
        assert!(token.is_pkce);
        assert_eq!(token.expiry_time, format_expiry(4_600));

        let form = s.seen.recv().expect("the endpoint should have been posted");
        assert!(form.contains("grant_type=authorization_code"), "{form}");
        assert!(form.contains("code=CODE"), "{form}");
        assert!(
            form.contains(&format!("code_verifier={}", pkce.verifier())),
            "the verifier proves this is the same session: {form}"
        );
    }

    #[test]
    fn a_refresh_keeps_the_old_refresh_token_when_none_is_returned() {
        // Goal: the service returns only a new access token. Overwriting the
        // refresh token with nothing would make every session survive exactly
        // one refresh and then die.
        let s = stub(200, r#"{"access_token":"new","expires_in":7200}"#);
        let mut cfg = cfg();
        cfg.token_url = s.url.clone();

        let token =
            refresh(&Agent::new_with_defaults(), &cfg, "old-refresh", 500).expect("refresh");
        assert_eq!(token.access_token, "new");
        assert_eq!(
            token.refresh_token, "old-refresh",
            "the old one must be kept"
        );
        assert_eq!(token.token_type, "Bearer", "defaulted when unstated");

        let form = s.seen.recv().expect("posted");
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains("client_secret=test-secret"), "{form}");
    }

    #[test]
    fn a_client_without_a_secret_does_not_send_an_empty_one() {
        // Goal: sending `client_secret=` is not the same as omitting it, and
        // some servers reject the empty form.
        let s = stub(200, r#"{"access_token":"a","expires_in":60}"#);
        let mut cfg = AuthConfig::new("public-client".into(), None);
        cfg.token_url = s.url.clone();

        refresh(&Agent::new_with_defaults(), &cfg, "r", 0).expect("refresh");
        let form = s.seen.recv().expect("posted");
        assert!(!form.contains("client_secret"), "{form}");
    }

    #[test]
    fn a_rejected_exchange_carries_the_reason() {
        // Goal: "invalid_grant" versus "invalid_client" is the difference
        // between "log in again" and "your credentials are wrong". Losing the
        // body would lose that.
        let s = stub(
            401,
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
        );
        let mut cfg = cfg();
        cfg.token_url = s.url.clone();

        let err = refresh(&Agent::new_with_defaults(), &cfg, "stale", 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("invalid_grant"), "{err}");
    }

    // ---- expiry bookkeeping ----

    #[test]
    fn timestamps_round_trip_through_the_stored_format() {
        // Goal: the on-disk stamp is written by another tool in Python's naive
        // UTC isoformat. Getting this wrong means either refreshing constantly
        // or never refreshing at all.
        assert_eq!(format_expiry(0), "1970-01-01T00:00:00.000000");
        assert_eq!(format_expiry(1_700_000_000), "2023-11-14T22:13:20.000000");
        assert_eq!(
            parse_expiry("2023-11-14T22:13:20.000000"),
            Some(1_700_000_000)
        );
        assert_eq!(
            parse_expiry("2023-11-14T22:13:20.123456"),
            Some(1_700_000_000),
            "sub-second precision is noise against an hours-long lifetime"
        );
        assert_eq!(
            parse_expiry("2024-02-29T12:00:00"),
            Some(1_709_208_000),
            "leap day"
        );
        for t in [0i64, 951_782_400, 1_700_000_000, 2_500_000_000] {
            assert_eq!(parse_expiry(&format_expiry(t)), Some(t), "round trip {t}");
        }
    }

    #[test]
    fn an_unreadable_stamp_means_refresh_rather_than_trust() {
        // Goal: a stamp we cannot parse must never read as "still valid". The
        // safe failure is one wasted refresh, not an expired request.
        for bad in [
            "",
            "never",
            "2023-13-01T00:00:00",
            "2023-11-14 22:13:20",
            "x-y-zTa:b:c",
        ] {
            assert_eq!(parse_expiry(bad), None, "{bad:?} should not parse");
        }
        let token = StoredToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            token_type: "Bearer".into(),
            expiry_time: "not a date".into(),
            is_pkce: true,
        };
        assert!(token.seconds_remaining(0).is_none());
        assert!(
            token.needs_refresh(0),
            "unknown expiry must force a refresh"
        );
    }

    #[test]
    fn a_token_is_refreshed_before_it_actually_expires() {
        // Goal: refreshing at the moment of expiry races the request. The
        // margin is what stops a long track resolve failing mid-flight.
        let token = StoredToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            token_type: "Bearer".into(),
            expiry_time: format_expiry(10_000),
            is_pkce: true,
        };
        assert!(
            !token.needs_refresh(10_000 - REFRESH_MARGIN_SECS - 60),
            "plenty of time"
        );
        assert!(
            token.needs_refresh(10_000 - REFRESH_MARGIN_SECS + 1),
            "inside the margin"
        );
        assert!(token.needs_refresh(10_001), "already expired");
        assert_eq!(token.seconds_remaining(9_000), Some(1_000));
    }

    #[test]
    fn a_stored_session_survives_a_round_trip_through_the_file_format() {
        // Goal: the on-disk shape is shared with hiresTI, so an existing login
        // keeps working and a token priel refreshes stays readable there.
        let json = r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer",
                       "expiry_time":"2026-01-01T00:00:00.000000","is_pkce":true}"#;
        let token: StoredToken = serde_json::from_str(json).expect("parse");
        assert!(token.is_pkce);
        let again: StoredToken =
            serde_json::from_str(&serde_json::to_string(&token).unwrap()).unwrap();
        assert_eq!(again.expiry_time, token.expiry_time);
        assert_eq!(again.access_token, token.access_token);
    }
}
