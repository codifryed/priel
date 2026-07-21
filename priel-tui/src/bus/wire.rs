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

//! The D-Bus wire format: message headers, marshalling and unmarshalling.
//!
//! Pure functions over bytes. Nothing here opens a socket, reads a clock or
//! touches player state - the connection is stage two and lives next door.
//!
//! **This is built for a closed set of signatures and must not be generalised.**
//! `TrackList` and `Playlists` are permanently out of scope, which is what
//! keeps the set closed: the write side needs `a{sv}` with an `as` inside and
//! the leaf types, and never `ao`, `aa{sv}`, `(oss)` or `a(oss)`. Every
//! container that is not implemented is a padding bug that cannot happen - and
//! a padding bug is invisible, because the specification's prescribed answer to
//! a malformed message is to drop the connection without notice. There is
//! nothing coming that needs the general case.
//!
//! The read side is smaller still. priel is only ever a server, so the bodies
//! it parses are `s`, `ss`, `ssv`, `x`, `ox` and the empty one, plus `u` and
//! `s` for the two replies the handshake waits on.
//!
//! Specification: D-Bus 0.43.

// The wire format is landing on its own, ahead of the connection that speaks it
// and the interfaces that fill it in, so that the padding rules can be pinned
// against captures from a real bus before anything depends on them. Until those
// arrive the only callers are the tests.
#![allow(
    dead_code,
    reason = "the connection and the interfaces above it are the callers, and land next"
)]

use std::fmt;

/// Every offset in a message is counted from byte 0 of that message, so the
/// whole thing is built in one buffer and there is no second origin to get
/// wrong.
const FIXED_HEADER_LEN: usize = 16;

/// Spec 0.43: the protocol version priel speaks, and the only one it accepts.
const PROTOCOL_VERSION: u8 = 1;

/// Spec 0.43: no message may exceed 2^27 bytes.
const MAX_MESSAGE_LEN: u64 = 134_217_728;

/// Spec 0.43: no array may exceed 2^26 bytes.
const MAX_ARRAY_LEN: usize = 67_108_864;

/// Spec 0.43: a signature is at most 255 bytes, because its length is one byte.
const MAX_SIGNATURE_LEN: usize = 255;

/// The message flag priel sets on everything that is not a call awaiting an
/// answer. Replies and signals never get one.
pub(crate) const NO_REPLY_EXPECTED: u8 = 0x1;

/// Errors from the wire, split by what the caller can still do about it.
///
/// The receive loop needs to branch on this, so it is a type and not a string:
/// one of these costs the connection and the other costs one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WireError {
    /// The fixed header is not a D-Bus message header. Nothing downstream of
    /// this point can be trusted to start on a message boundary, so the stream
    /// cannot be resynchronised and the connection has to go.
    Framing(&'static str),
    /// One message is malformed, or is shaped outside the closed set of
    /// signatures priel implements. `frame_len` has already said how long it
    /// is, so the caller skips those bytes and keeps the connection.
    Message(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framing(what) => write!(f, "bus framing: {what}"),
            Self::Message(what) => write!(f, "bus message: {what}"),
        }
    }
}

impl std::error::Error for WireError {}

pub(crate) type Result<T> = std::result::Result<T, WireError>;

fn bad(what: impl Into<String>) -> WireError {
    WireError::Message(what.into())
}

/// Byte order of a message.
///
/// Both are read, because a peer picks its own; priel writes [`Endian::NATIVE`]
/// so that marshalling is a copy rather than a byte swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

impl Endian {
    pub(crate) const NATIVE: Self = if cfg!(target_endian = "little") {
        Self::Little
    } else {
        Self::Big
    };

    fn marker(self) -> u8 {
        match self {
            Self::Little => b'l',
            Self::Big => b'B',
        }
    }

    fn from_marker(byte: u8) -> Result<Self> {
        match byte {
            b'l' => Ok(Self::Little),
            b'B' => Ok(Self::Big),
            _ => Err(WireError::Framing("first byte is neither 'l' nor 'B'")),
        }
    }
}

/// The four message types. Spec 0.43 reserves 0 and says a receiver must ignore
/// types it does not know, which is what the [`WireError::Message`] here buys:
/// the frame length is already known, so an unknown type costs one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageType {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

impl MessageType {
    fn code(self) -> u8 {
        match self {
            Self::MethodCall => 1,
            Self::MethodReturn => 2,
            Self::Error => 3,
            Self::Signal => 4,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::MethodCall),
            2 => Ok(Self::MethodReturn),
            3 => Ok(Self::Error),
            4 => Ok(Self::Signal),
            other => Err(bad(format!("message type {other} is not one priel knows"))),
        }
    }
}

/// One `(yv)` in the header field array.
///
/// Held as a list in wire order rather than as named options, so a message that
/// was parsed re-marshals to the bytes it arrived as. Field order is the
/// peer's choice and no two implementations agree on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Field {
    Path(String),
    Interface(String),
    Member(String),
    ErrorName(String),
    ReplySerial(u32),
    Destination(String),
    Sender(String),
    Signature(String),
    /// priel sends no file descriptors and never writes this, but peers do
    /// send it and a parsed message has to be able to carry it back out.
    UnixFds(u32),
}

impl Field {
    fn code(&self) -> u8 {
        match self {
            Self::Path(_) => 1,
            Self::Interface(_) => 2,
            Self::Member(_) => 3,
            Self::ErrorName(_) => 4,
            Self::ReplySerial(_) => 5,
            Self::Destination(_) => 6,
            Self::Sender(_) => 7,
            Self::Signature(_) => 8,
            Self::UnixFds(_) => 9,
        }
    }
}

/// A leaf value, or `as`.
///
/// The four leaf types MPRIS never uses - `n`, `q`, `t` and `h` - are absent on
/// purpose. `y` and `g` are absent too: the only `g` in a message priel handles
/// is a signature, which the marshaller writes itself.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    Int32(i32),
    Uint32(u32),
    Int64(i64),
    Double(f64),
    Str(String),
    /// `o`. Distinct from `Str` because GNOME type-checks `mpris:trackid`, and
    /// a bare string there is the bug that segfaults playerctl.
    Path(String),
    /// `as`. `xesam:artist` and `xesam:albumArtist` are arrays, not strings.
    Strings(Vec<String>),
}

impl Value {
    fn signature(&self) -> &'static str {
        match self {
            Self::Bool(_) => "b",
            Self::Int32(_) => "i",
            Self::Uint32(_) => "u",
            Self::Int64(_) => "x",
            Self::Double(_) => "d",
            Self::Str(_) => "s",
            Self::Path(_) => "o",
            Self::Strings(_) => "as",
        }
    }
}

/// What a `v` carries.
///
/// `a{sv}` is the one container MPRIS puts inside a variant - `Metadata` - and
/// its members are leaves, so the nesting stops here by construction. That is
/// deliberate: a recursive value type would admit depths that never occur and
/// would need a depth bound to be safe.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Variant {
    Value(Value),
    Dict(Vec<(String, Value)>),
}

impl Variant {
    fn signature(&self) -> &'static str {
        match self {
            Self::Value(v) => v.signature(),
            Self::Dict(_) => "a{sv}",
        }
    }
}

/// One top-level item in a message body.
///
/// The bodies this composes are exactly the ones the interfaces need: `s`,
/// `ss`, `ssv`, `x`, `ox`, `u`, `v`, `a{sv}`, `sa{sv}as` and the empty one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    Value(Value),
    Variant(Variant),
    /// `a{sv}`, whose members are variants - what `GetAll` returns and what
    /// `PropertiesChanged` carries in `changed_properties`.
    Dict(Vec<(String, Variant)>),
}

impl Arg {
    fn signature(&self) -> &'static str {
        match self {
            Self::Value(v) => v.signature(),
            Self::Variant(_) => "v",
            Self::Dict(_) => "a{sv}",
        }
    }
}

/// The signature of a whole body, which is what the SIGNATURE header field has
/// to say for the peer to be able to parse it.
pub(crate) fn body_signature(body: &[Arg]) -> String {
    let mut out = String::new();
    for arg in body {
        out.push_str(arg.signature());
    }
    out
}

/// One top-level type in a signature, which is what drives the parse of a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int32,
    Uint32,
    Int64,
    Double,
    Str,
    Path,
    Strings,
    Variant,
    Dict,
}

/// Split a signature into the closed set of types priel implements.
///
/// Anything outside it - `ao`, `aa{sv}`, a struct, a leaf priel has no use for -
/// is an error rather than a guess. Guessing is how a body gets misparsed, and
/// a misparse is silent where an error is not.
fn parse_signature(signature: &str) -> Result<Vec<Kind>> {
    let bytes = signature.as_bytes();
    let mut kinds = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (kind, width) = match bytes[at] {
            b'b' => (Kind::Bool, 1),
            b'i' => (Kind::Int32, 1),
            b'u' => (Kind::Uint32, 1),
            b'x' => (Kind::Int64, 1),
            b'd' => (Kind::Double, 1),
            b's' => (Kind::Str, 1),
            b'o' => (Kind::Path, 1),
            b'v' => (Kind::Variant, 1),
            b'a' if bytes[at..].starts_with(b"as") => (Kind::Strings, 2),
            b'a' if bytes[at..].starts_with(b"a{sv}") => (Kind::Dict, 5),
            _ => {
                return Err(bad(format!(
                    "signature \"{signature}\" is outside the set priel implements"
                )));
            }
        };
        kinds.push(kind);
        at += width;
    }
    Ok(kinds)
}

/// Serial numbers for outbound messages.
///
/// Spec 0.43: a serial identifies a call until its reply arrives and must never
/// be zero, which is what the wrap skips.
#[derive(Debug, Default)]
pub(crate) struct Serials(u32);

impl Serials {
    pub(crate) fn mint(&mut self) -> u32 {
        self.0 = self.0.checked_add(1).unwrap_or(1);
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal: every value knows the signature character the peer will read it
    /// by. Method: name each one, since a wrong character is a misparse at the
    /// far end rather than a local failure.
    #[test]
    fn each_value_names_its_own_type() {
        assert_eq!(Value::Bool(true).signature(), "b");
        assert_eq!(Value::Int32(1).signature(), "i");
        assert_eq!(Value::Uint32(1).signature(), "u");
        assert_eq!(Value::Int64(1).signature(), "x");
        assert_eq!(Value::Double(1.0).signature(), "d");
        assert_eq!(Value::Str(String::new()).signature(), "s");
        assert_eq!(Value::Path("/a".into()).signature(), "o");
        assert_eq!(Value::Strings(Vec::new()).signature(), "as");
    }

    /// Goal: the two containers priel writes report the signatures MPRIS names.
    /// Method: a variant is `v` whatever it holds, and a dictionary is `a{sv}`
    /// at either level of nesting.
    #[test]
    fn containers_name_their_own_type() {
        assert_eq!(Variant::Value(Value::Int64(0)).signature(), "x");
        assert_eq!(Variant::Dict(Vec::new()).signature(), "a{sv}");
        assert_eq!(Arg::Variant(Variant::Dict(Vec::new())).signature(), "v");
        assert_eq!(Arg::Dict(Vec::new()).signature(), "a{sv}");
    }

    /// Goal: the bodies the interfaces actually send get the signatures the
    /// specification names for them. Method: build each one and read its
    /// signature back, `PropertiesChanged` included.
    #[test]
    fn bodies_carry_the_signatures_the_interfaces_declare() {
        assert_eq!(body_signature(&[]), "");
        assert_eq!(body_signature(&[Arg::Value(Value::Str("a".into()))]), "s");
        assert_eq!(
            body_signature(&[
                Arg::Value(Value::Str("a".into())),
                Arg::Value(Value::Str("b".into())),
                Arg::Variant(Variant::Value(Value::Double(1.0))),
            ]),
            "ssv"
        );
        assert_eq!(
            body_signature(&[
                Arg::Value(Value::Path("/t/1".into())),
                Arg::Value(Value::Int64(0)),
            ]),
            "ox"
        );
        assert_eq!(
            body_signature(&[
                Arg::Value(Value::Str("i".into())),
                Arg::Dict(Vec::new()),
                Arg::Value(Value::Strings(Vec::new())),
            ]),
            "sa{sv}as"
        );
    }

    /// Goal: a signature splits back into the same types it was built from.
    /// Method: round-trip each body shape through the tokeniser.
    #[test]
    fn signatures_split_into_the_types_they_name() {
        assert_eq!(parse_signature(""), Ok(Vec::new()));
        assert_eq!(parse_signature("s"), Ok(vec![Kind::Str]));
        assert_eq!(parse_signature("ss"), Ok(vec![Kind::Str, Kind::Str]));
        assert_eq!(
            parse_signature("ssv"),
            Ok(vec![Kind::Str, Kind::Str, Kind::Variant])
        );
        assert_eq!(parse_signature("ox"), Ok(vec![Kind::Path, Kind::Int64]));
        assert_eq!(
            parse_signature("sa{sv}as"),
            Ok(vec![Kind::Str, Kind::Dict, Kind::Strings])
        );
        assert_eq!(
            parse_signature("bidu"),
            Ok(vec![Kind::Bool, Kind::Int32, Kind::Double, Kind::Uint32])
        );
    }

    /// Goal: the containers deliberately left out are refused, not guessed at.
    /// Method: hand the tokeniser the `TrackList` and `Playlists` signatures
    /// and the leaf types MPRIS never uses. Each is a padding rule priel does
    /// not implement, so reading one as something else would be silent.
    #[test]
    fn signatures_outside_the_closed_set_are_refused() {
        for outside in [
            "ao", "aa{sv}", "(oss)", "a(oss)", "at", "ay", "n", "q", "t", "h", "a",
        ] {
            assert!(
                parse_signature(outside).is_err(),
                "\"{outside}\" is outside the closed set and must not tokenise"
            );
        }
    }

    /// Goal: a serial is never zero, which the specification forbids. Method:
    /// mint from a fresh counter and from one about to wrap.
    #[test]
    fn serials_start_at_one_and_never_reach_zero() {
        let mut serials = Serials::default();
        assert_eq!(serials.mint(), 1);
        assert_eq!(serials.mint(), 2);

        let mut wrapping = Serials(u32::MAX);
        assert_eq!(wrapping.mint(), 1);
    }
}
