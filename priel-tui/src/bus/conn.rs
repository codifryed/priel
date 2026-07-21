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

//! The connection to the session bus: where it is, how to authenticate to it,
//! and the loop that answers it.
//!
//! This module owns the socket and the bytes on it. It publishes no properties
//! and implements no interface beyond the two the bus itself requires of every
//! peer - what to publish is decided above it.
//!
//! **Nothing here fails the program.** No address in the environment, a socket
//! that will not connect, a refused handshake or a name already owned all leave
//! priel a working player that simply is not on the bus, with the reason in the
//! diagnostic log. That is the whole point of speaking the protocol directly:
//! one binary that runs on a media-server box with no desktop on it.
//!
//! Specification: D-Bus 0.43.

// The connection lands ahead of the interfaces that fill it in, so that the
// handshake and the dispatch loop can be pinned against a scripted session
// before anything depends on them. Until those arrive the only callers are the
// tests.
#![allow(
    dead_code,
    reason = "the interfaces above it are the callers, and land next"
)]

use std::path::PathBuf;

/// Where the session bus listens.
///
/// Only the two forms a session bus is ever published as. A transport priel
/// does not speak is not an error: it is a machine priel is not on the bus of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Address {
    /// `unix:path=` - a socket with an entry in the filesystem.
    Path(PathBuf),
    /// `unix:abstract=` - a Linux abstract socket, which has no filesystem
    /// entry and whose name is sent with a leading nul.
    Abstract(String),
}

/// The first address in `$DBUS_SESSION_BUS_ADDRESS` that priel can speak.
///
/// Spec 0.43: the variable holds `;`-separated addresses in the peer's order of
/// preference, and a client takes the first it supports. `None` means there is
/// no bus here, which is an answer and not a failure.
pub(crate) fn parse_address(spec: &str) -> Option<Address> {
    spec.split(';').find_map(parse_entry)
}

/// One `transport:key=value,key=value` entry.
fn parse_entry(entry: &str) -> Option<Address> {
    let (transport, keys) = entry.split_once(':')?;
    if transport != "unix" {
        return None;
    }
    let mut path = None;
    let mut abstract_name = None;
    for pair in keys.split(',') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "path" => path = Some(unescape(value)?),
            "abstract" => abstract_name = Some(unescape(value)?),
            // Spec 0.43 lets a transport carry keys a client has no use for -
            // `guid` is on every address a bus publishes - and says to ignore
            // them rather than to refuse the address.
            _ => {}
        }
    }
    // Spec 0.43 makes the two mutually exclusive, so an address carrying both
    // is one priel cannot know the meaning of.
    match (path, abstract_name) {
        (Some(path), None) if !path.is_empty() => Some(Address::Path(PathBuf::from(path))),
        (None, Some(name)) if !name.is_empty() => Some(Address::Abstract(name)),
        _ => None,
    }
}

/// Undo the `%xx` escaping spec 0.43 defines for an address value.
///
/// Everything outside `[-0-9A-Za-z_/.\*]` arrives escaped, which is what lets
/// `,` and `;` keep their meaning as separators. A `%` that is not followed by
/// two hex digits is not an escape and the address is declined rather than
/// guessed at.
fn unescape(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = value.get(at + 1..at + 3)?;
            if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal: the ordinary session address resolves to the socket it names.
    /// Method: the exact spelling `dbus-broker` publishes on this machine.
    #[test]
    fn a_path_address_names_its_socket() {
        assert_eq!(
            parse_address("unix:path=/run/user/1000/bus"),
            Some(Address::Path(PathBuf::from("/run/user/1000/bus")))
        );
    }

    /// Goal: an abstract socket stays distinguishable from a filesystem one,
    /// because connecting to it takes a different call and a leading nul.
    /// Method: parse the form `dbus-daemon --session` publishes.
    #[test]
    fn an_abstract_address_is_kept_apart_from_a_path() {
        assert_eq!(
            parse_address("unix:abstract=/tmp/dbus-6PxSNJEC0h"),
            Some(Address::Abstract("/tmp/dbus-6PxSNJEC0h".into()))
        );
    }

    /// Goal: keys priel has no use for do not cost it the address. Method: the
    /// `guid` every bus appends, on both forms.
    #[test]
    fn keys_priel_does_not_read_are_ignored() {
        assert_eq!(
            parse_address("unix:path=/run/user/1000/bus,guid=6b1a1ef6dd"),
            Some(Address::Path(PathBuf::from("/run/user/1000/bus")))
        );
        assert_eq!(
            parse_address("unix:abstract=/tmp/dbus-x,guid=6b1a1ef6dd"),
            Some(Address::Abstract("/tmp/dbus-x".into()))
        );
    }

    /// Goal: an escaped value decodes to the bytes it stands for. Method: a
    /// path whose separators would otherwise end the value, and one whose
    /// escape spells a multi-byte character.
    #[test]
    fn percent_escapes_decode_to_what_they_name() {
        assert_eq!(
            parse_address("unix:path=/tmp/a%2Cb%3Bc"),
            Some(Address::Path(PathBuf::from("/tmp/a,b;c")))
        );
        assert_eq!(
            parse_address("unix:path=/tmp/%C3%A9"),
            Some(Address::Path(PathBuf::from("/tmp/é")))
        );
    }

    /// Goal: a `%` that is not an escape is declined rather than taken
    /// literally, since the far end meant something by it. Method: the three
    /// ways it can be malformed - truncated, non-hex, and the sign
    /// `from_str_radix` would otherwise accept.
    #[test]
    fn a_malformed_escape_is_declined() {
        assert_eq!(parse_address("unix:path=/tmp/a%2"), None);
        assert_eq!(parse_address("unix:path=/tmp/a%zz"), None);
        assert_eq!(parse_address("unix:path=/tmp/a%+f"), None);
    }

    /// Goal: an escape that decodes to something that is not text is declined,
    /// because every path priel is given has to be one it can log. Method: a
    /// lone continuation byte.
    #[test]
    fn an_escape_that_is_not_text_is_declined() {
        assert_eq!(parse_address("unix:path=/tmp/%80"), None);
    }

    /// Goal: the first address priel can speak wins, not the first address.
    /// Method: put a transport priel does not speak in front of one it does.
    #[test]
    fn the_first_address_priel_speaks_wins() {
        assert_eq!(
            parse_address("tcp:host=localhost,port=1;unix:path=/run/user/1000/bus"),
            Some(Address::Path(PathBuf::from("/run/user/1000/bus")))
        );
    }

    /// Goal: nothing usable in the environment is a clean absence rather than a
    /// failure - it is how priel runs on a box with no desktop. Method: the
    /// shapes that carry no socket priel can open.
    #[test]
    fn an_address_priel_cannot_use_is_simply_absent() {
        assert_eq!(parse_address(""), None);
        assert_eq!(parse_address("tcp:host=localhost,port=44444"), None);
        assert_eq!(parse_address("unixpath=/run/user/1000/bus"), None);
        assert_eq!(parse_address("unix:nonce-tcp"), None);
        assert_eq!(parse_address("unix:tmpdir=/tmp"), None);
    }

    /// Goal: an empty socket name is not a socket. Method: both forms, spelled
    /// with the key present and the value missing.
    #[test]
    fn an_empty_socket_name_is_declined() {
        assert_eq!(parse_address("unix:path="), None);
        assert_eq!(parse_address("unix:abstract="), None);
    }

    /// Goal: an address carrying both forms is declined, because spec 0.43
    /// makes them mutually exclusive and priel would have to guess which the
    /// peer meant. Method: give it both.
    #[test]
    fn an_address_that_is_both_forms_at_once_is_declined() {
        assert_eq!(parse_address("unix:path=/run/bus,abstract=/tmp/bus"), None);
    }
}
