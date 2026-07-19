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
use base64::engine::general_purpose::STANDARD;
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

/// The client identity, read from the user's own configuration.
///
/// Kept out of the repository deliberately: priel ships no credentials, so this
/// is a file each user fills in for themselves. See the module documentation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl Credentials {
    /// `$XDG_CONFIG_HOME/priel/credentials.json`, falling back to `~/.config`.
    #[must_use]
    pub fn default_path() -> String {
        let base = std::env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
        format!("{base}/priel/credentials.json")
    }

    /// Read the client identity from disk.
    ///
    /// # Errors
    /// If the file is absent or malformed. Absence is a normal state - priel
    /// runs without it, just without the ability to refresh or log in - so
    /// callers should treat the error as informational rather than fatal.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading {path} (no client credentials configured)"))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {path}"))
    }

    /// Write the identity out, owner-readable only.
    ///
    /// # Errors
    /// If the directory cannot be created or the file cannot be written.
    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialising the credentials")?;
        std::fs::write(path, json).with_context(|| format!("writing {path}"))?;
        restrict_permissions(path);
        Ok(())
    }

    #[must_use]
    pub fn into_config(self) -> AuthConfig {
        AuthConfig::new(self.client_id, self.client_secret)
    }
}

/// Where the upstream project publishes the source, tried in order.
///
/// The moving branches come first so a rotated client identity is picked up
/// without waiting for priel to release. The pinned commit is last and is the
/// safety net: it is immutable, it is verified to contain a usable identity, and
/// it keeps first-time login working if the branches are renamed or the file is
/// restructured. Its risk is staleness rather than failure - and if the service
/// ever rotates the identity, every native client breaks together until the
/// upstream package is updated, so a stale pin is no worse off than the rest.
///
/// The branch name is not knowable in advance, so both conventions are tried.
pub const UPSTREAM_SOURCES: &[&str] = &[
    "https://raw.githubusercontent.com/EbbLabs/python-tidal/main/tidalapi/session.py",
    "https://raw.githubusercontent.com/EbbLabs/python-tidal/master/tidalapi/session.py",
    // v0.8.11, verified to yield the same identity a working install carries.
    "https://raw.githubusercontent.com/EbbLabs/python-tidal/899e6b3c9485559d5b0e1a5702a5912de0ce43cf/tidalapi/session.py",
];

/// Where a client identity came from, so the interface can say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    /// The user's own `credentials.json`.
    Configured,
    /// A Python `tidalapi` installed on this machine.
    LocalPackage,
    /// Downloaded from the upstream project.
    Upstream,
}

/// A client identity from the user's config, or from a local install.
///
/// Never touches the network: fetching is a separate action the user has to ask
/// for, so priel does not quietly reach out on first start.
#[must_use]
pub fn local_credentials(path: &str) -> Option<(Credentials, CredentialSource)> {
    if let Ok(creds) = Credentials::load(path) {
        return Some((creds, CredentialSource::Configured));
    }
    discover_credentials().map(|c| (c, CredentialSource::LocalPackage))
}

/// Download a client identity from the upstream project's source.
///
/// **Performs a network request, and should only run when the user has asked for
/// it.** priel ships no credentials and most users will have no Python package
/// installed to read them from, so this is the practical bootstrap - but it
/// fetches from a repository priel does not control, and reaching out without
/// being asked would be the wrong default.
///
/// The result is meant to be written to the user's credentials file so this
/// happens once rather than on every start.
///
/// # Errors
/// If none of the sources can be reached, or none contains a recognisable
/// client identity - most likely because the upstream file was restructured.
pub fn fetch_credentials(agent: &Agent, urls: &[&str]) -> Result<Credentials> {
    let mut last: Option<String> = None;
    for url in urls {
        match agent.get(*url).call() {
            Ok(mut resp) => {
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                if let Some(creds) = extract_credentials(&body) {
                    return Ok(creds);
                }
                last = Some(format!("{url}: no client identity found in that source"));
            }
            Err(e) => last = Some(format!("{url}: {e}")),
        }
    }
    bail!(
        "could not obtain a client identity ({}). Configure one at {} instead.",
        last.unwrap_or_else(|| "no sources tried".to_string()),
        Credentials::default_path()
    )
}

/// Recover a client identity from a locally installed Python `tidalapi`.
///
/// Opportunistic only: most users will have no Python package installed, and one
/// installed as a Flatpak is inside a sandbox where this will not find it. When
/// it does hit, it saves a network round trip and is free; when it misses, the
/// caller falls back to [`fetch_credentials`].
#[must_use]
pub fn discover_credentials() -> Option<Credentials> {
    for dir in python_site_packages() {
        let path = format!("{dir}/tidalapi/session.py");
        if let Ok(source) = std::fs::read_to_string(&path)
            && let Some(creds) = extract_credentials(&source)
        {
            return Some(creds);
        }
    }
    None
}

/// Pull the PKCE client identity out of `tidalapi`'s session source.
///
/// The values are stored double-base64 encoded, which is obfuscation rather
/// than encryption - it keeps them out of a plain grep. Returns `None` on
/// anything unexpected: a wrong guess here would produce a client identity that
/// fails at login with no useful message.
#[must_use]
pub fn extract_credentials(source: &str) -> Option<Credentials> {
    let client_id = extract_b64_pair(source, "client_id_pkce")?;
    // A missing secret is not a failure: not every client has one.
    let client_secret = extract_b64_pair(source, "client_secret_pkce");
    Some(Credentials {
        client_id,
        client_secret,
    })
}

fn extract_b64_pair(source: &str, name: &str) -> Option<String> {
    let pattern = format!(
        r#"(?s){name}\s*=\s*base64\.b64decode\(\s*base64\.b64decode\(b"([^"]+)"\)\s*\+\s*base64\.b64decode\(b"([^"]+)"\)\s*\)"#
    );
    let re = regex::Regex::new(&pattern).ok()?;
    let caps = re.captures(source)?;
    let outer: Vec<u8> = STANDARD
        .decode(caps.get(1)?.as_str())
        .ok()?
        .into_iter()
        .chain(STANDARD.decode(caps.get(2)?.as_str()).ok()?)
        .collect();
    String::from_utf8(STANDARD.decode(outer).ok()?).ok()
}

/// Plausible `site-packages` directories, most specific first.
fn python_site_packages() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        format!("{home}/.local/lib"),
        "/usr/lib".to_string(),
        "/usr/lib64".to_string(),
        "/usr/local/lib".to_string(),
    ];
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("python3") {
                found.push(format!("{root}/{name}/site-packages"));
            }
        }
    }
    found
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

impl StoredToken {
    /// Read a saved session.
    ///
    /// # Errors
    /// If the file is unreadable or is not the expected JSON.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading token file {path} (not logged in?)"))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing token file {path}"))
    }

    /// Write the session back, creating the directory if needed.
    ///
    /// # Errors
    /// If the directory cannot be created or the file cannot be written.
    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialising the token")?;
        std::fs::write(path, json).with_context(|| format!("writing token file {path}"))?;
        restrict_permissions(path);
        Ok(())
    }
}

/// Make a credential file readable only by its owner. Best effort: a failure
/// here is not worth refusing to save a token the user just earned.
#[cfg(unix)]
fn restrict_permissions(path: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &str) {}

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

    // ---- obtaining a client identity ----

    /// The shape `tidalapi` stores its client identity in: double base64, split
    /// across two literals so it does not appear in a plain grep.
    fn fake_session_py(id_a: &str, id_b: &str, secret: bool) -> String {
        let mut out = format!(
            r#"
        self.client_unique_key = format(random.getrandbits(64), "02x")
        self.client_id_pkce = base64.b64decode(
            base64.b64decode(b"{id_a}")
            + base64.b64decode(b"{id_b}")
        ).decode("utf-8")
"#
        );
        if secret {
            use std::fmt::Write as _;
            let (a, b) = double_encode("secret");
            let _ = write!(
                out,
                r#"
        self.client_secret_pkce = base64.b64decode(
            base64.b64decode(b"{a}")
            + base64.b64decode(b"{b}")
        ).decode("utf-8")
"#
            );
        }
        out
    }

    /// Double-encode a value the way the upstream source stores it, split in two.
    fn double_encode(value: &str) -> (String, String) {
        let inner = STANDARD.encode(value);
        // Split the way upstream does: two independently decodable halves whose
        // plaintexts concatenate back into the inner encoding.
        let half = inner.len() / 2;
        (
            STANDARD.encode(&inner[..half]),
            STANDARD.encode(&inner[half..]),
        )
    }

    #[test]
    fn a_client_identity_is_recovered_from_the_upstream_shape() {
        // Goal: the value is deliberately obfuscated, split across two literals
        // and encoded twice. Getting the layering wrong yields a plausible
        // string that fails only at login.
        let (a, b) = double_encode("my-client-id");
        let creds = extract_credentials(&fake_session_py(&a, &b, true))
            .expect("should recover the identity");
        assert_eq!(creds.client_id, "my-client-id");
        assert_eq!(creds.client_secret.as_deref(), Some("secret"));
    }

    #[test]
    fn a_client_without_a_published_secret_is_still_usable() {
        // Goal: not every client has a secret, and its absence must not throw
        // away the id alongside it.
        let (a, b) = double_encode("only-an-id");
        let creds =
            extract_credentials(&fake_session_py(&a, &b, false)).expect("id alone is enough");
        assert_eq!(creds.client_id, "only-an-id");
        assert!(creds.client_secret.is_none());
    }

    #[test]
    fn unrecognisable_source_yields_nothing_rather_than_a_guess() {
        // Goal: the upstream file can be restructured at any time. A wrong guess
        // produces a client identity that fails at login with no useful message,
        // so no answer is better than a bad one.
        assert!(extract_credentials("").is_none());
        assert!(extract_credentials("client_id_pkce = 'plaintext'").is_none());
        assert!(
            extract_credentials(r#"self.client_id_pkce = base64.b64decode(b"!!!")"#).is_none(),
            "a single-layer form is not the shape we know"
        );
    }

    #[test]
    fn the_configured_file_wins_over_a_local_package() {
        // Goal: an explicit choice by the user must never be silently overridden
        // by whatever happens to be installed on the machine.
        let dir = std::env::temp_dir().join(format!("priel-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"client_id":"configured"}"#).expect("write");

        let (creds, source) =
            local_credentials(path.to_str().expect("path")).expect("should find the file");
        assert_eq!(creds.client_id, "configured");
        assert_eq!(source, CredentialSource::Configured);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fetch_falls_through_the_sources_and_explains_a_total_failure() {
        // Goal: the branch name upstream uses is not knowable, so several URLs
        // are tried. If none works the message has to point at the manual
        // alternative rather than leaving the user stuck.
        let good = stub_source(&{
            let (a, b) = double_encode("fetched-id");
            fake_session_py(&a, &b, false)
        });
        let agent = Agent::new_with_defaults();

        let creds = fetch_credentials(&agent, &["http://127.0.0.1:1/nope", &good])
            .expect("the second source should answer");
        assert_eq!(creds.client_id, "fetched-id");

        let err = fetch_credentials(&agent, &["http://127.0.0.1:1/a"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not obtain"), "{err}");
        assert!(
            err.contains("credentials.json"),
            "should name the fallback: {err}"
        );
    }

    /// Serve one body over HTTP and return its URL.
    fn stub_source(body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/session.py", listener.local_addr().expect("addr"));
        let body = body.to_string();
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let resp = format!(
                    "HTTP/1.1 200 S\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        url
    }

    #[test]
    fn the_last_upstream_source_is_an_immutable_pin() {
        // Goal: the moving branches can be renamed or restructured at any time.
        // The final entry is the safety net for a first-time login, so it must
        // be a commit hash rather than another branch that can move under us.
        let last = UPSTREAM_SOURCES.last().expect("at least one source");
        let sha = last
            .strip_prefix("https://raw.githubusercontent.com/EbbLabs/python-tidal/")
            .and_then(|rest| rest.split('/').next())
            .expect("the ref sits after the repository");
        assert_eq!(sha.len(), 40, "a full commit hash, not a tag: {sha}");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "not a commit hash: {sha}"
        );
        assert!(
            UPSTREAM_SOURCES.len() >= 2,
            "the pin is a fallback, not the only source"
        );
    }
}
