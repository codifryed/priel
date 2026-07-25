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

//! Checking priel's own forge for a newer release.
//!
//! This is the one thing in this crate that talks to the code host rather than
//! to the streaming service, and it is here because this crate owns the HTTP
//! agent and the blocking fetch. It reads one field of one public JSON document
//! and compares two version strings; it never downloads or installs anything -
//! that is the installer's job, and the interface only ever *tells* the listener
//! a newer version exists.
//!
//! Deliberately quiet on every doubt. A response that will not parse, a version
//! that will not compare, a forge that is unreachable - all of them mean "no
//! update to report" rather than an error on screen, because a failed check for
//! a new version is not a problem the listener has to see.

use crate::Client;

/// The forge endpoint that names the most recent published release.
///
/// A public, unauthenticated read: the `/releases/latest` route answers with the
/// newest non-draft, non-prerelease release, and the one field taken from it is
/// `tag_name`. `404` here means there are no releases yet, which is not an error
/// - it is "nothing to update to".
pub const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/codifryed/priel/releases/latest";

/// A release document is small, but a cap keeps a wrong or hostile response off
/// the heap all the same. A release with a long body and many assets is still
/// far inside this.
const RESPONSE_CAP: usize = 256 * 1024;

/// The tag of the latest release if it is newer than `current`, else `None`.
///
/// The whole update check in one call: fetch the latest release, read its tag,
/// and compare. **Infallible on purpose.** Every way it can fail - the forge
/// unreachable, a `404` because there are no releases yet, a body that will not
/// parse, a tag that is not a version - means the same thing to the one caller:
/// nothing to tell the listener. Returning a `Result` would only offer a way to
/// turn a failed background check into an error on screen, which is exactly what
/// it must not do.
///
/// `url` is passed in rather than read from [`LATEST_RELEASE_URL`] so a test can
/// point it at a stub; production passes the constant.
#[must_use]
pub fn newer_release(client: &Client, url: &str, current: &str) -> Option<String> {
    let bytes = client.fetch_bytes(url, RESPONSE_CAP).ok()?;
    let tag = tag_from_json(&bytes)?;
    is_newer(&tag, current).then_some(tag)
}

/// The `tag_name` of a release JSON document, if it has one.
fn tag_from_json(bytes: &[u8]) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    doc.get("tag_name")?.as_str().map(str::to_owned)
}

/// Is `latest` a newer version than `current`?
///
/// A plain `major.minor.patch` comparison, which is all priel's own versions
/// are. A leading `v` is tolerated on either side, and any pre-release or build
/// suffix after a `-` is dropped before comparing - a check that treated
/// `0.2.0-rc1` as newer than `0.2.0` would nag about a release that is not out.
///
/// Anything that will not parse compares as **not newer**: the check exists to
/// point at a real newer release, and a version string it cannot read is not
/// evidence of one.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Parse `v1.2.3`, `1.2.3` or `1.2` into a comparable triple, dropping any
/// `-suffix`. A missing patch counts as zero.
fn parse(version: &str) -> Option<(u32, u32, u32)> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::{is_newer, tag_from_json};

    #[test]
    fn a_higher_version_in_any_position_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"), "a higher minor");
        assert!(
            is_newer("1.0.0", "0.9.9"),
            "a higher major beats everything"
        );
        assert!(is_newer("0.1.1", "0.1.0"), "a higher patch");
        assert!(!is_newer("0.1.0", "0.1.0"), "the same version is not newer");
        assert!(!is_newer("0.1.0", "0.2.0"), "an older version is not newer");
    }

    #[test]
    fn a_leading_v_and_a_missing_patch_are_tolerated() {
        assert!(is_newer("v0.2.0", "0.1.0"), "the tag's own v prefix");
        assert!(
            is_newer("0.2", "0.1.9"),
            "a two-part version reads as x.y.0"
        );
        assert!(!is_newer("v0.1", "0.1.0"), "0.1 is 0.1.0, not newer");
    }

    #[test]
    fn a_prerelease_suffix_does_not_count_as_the_release() {
        // The rc must not nag about a final release that is not out yet.
        assert!(
            !is_newer("0.2.0-rc1", "0.2.0"),
            "a release candidate is not newer than the release"
        );
        assert!(
            is_newer("0.2.0-rc1", "0.1.0"),
            "but it is still newer than an older release"
        );
    }

    #[test]
    fn a_version_that_will_not_parse_is_never_newer() {
        // The check points at real releases; a string it cannot read is not one.
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("0.2.0", "unknown"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn the_tag_is_read_from_the_release_shape_and_absent_from_others() {
        assert_eq!(
            tag_from_json(br#"{"tag_name":"v0.2.0","name":"0.2.0"}"#).as_deref(),
            Some("v0.2.0")
        );
        // A 404 body, or anything without the field, is no tag rather than a panic.
        assert_eq!(tag_from_json(br#"{"message":"Not Found"}"#), None);
        assert_eq!(tag_from_json(b"not json at all"), None);
    }
}
