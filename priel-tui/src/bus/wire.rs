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

use std::fmt;

#[cfg(test)]
mod vectors;

/// Every offset in a message is counted from byte 0 of that message, so the
/// whole thing is built in one buffer and there is no second origin to get
/// wrong.
const FIXED_HEADER_LEN: usize = 16;

/// Spec 0.43: the protocol version priel speaks, and the only one it accepts.
const PROTOCOL_VERSION: u8 = 1;

/// Spec 0.43: no message may exceed 2^27 bytes.
const MAX_MESSAGE_LEN: usize = 134_217_728;

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

/// A whole message: the fixed header, the header fields and the body.
///
/// The fields are a list in wire order and not a set of named options, which is
/// what makes a parsed message re-marshal to the bytes it arrived as. Every
/// implementation orders them differently and the specification blesses all of
/// them, so there is no canonical order to normalise to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    pub(crate) kind: MessageType,
    pub(crate) flags: u8,
    pub(crate) serial: u32,
    pub(crate) fields: Vec<Field>,
    pub(crate) body: Vec<Arg>,
}

impl Message {
    pub(crate) fn method_call(
        serial: u32,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
    ) -> Self {
        Self {
            kind: MessageType::MethodCall,
            flags: 0,
            serial,
            fields: vec![
                Field::Path(path.to_owned()),
                Field::Interface(interface.to_owned()),
                Field::Member(member.to_owned()),
                Field::Destination(destination.to_owned()),
            ],
            body: Vec::new(),
        }
    }

    /// The answer to `call`. Its serial and its sender are what address the
    /// reply, so they are read off the call rather than passed in again - a
    /// reply correlated by anything else is a reply to the wrong caller.
    pub(crate) fn method_return(serial: u32, call: &Self) -> Self {
        let mut fields = vec![Field::ReplySerial(call.serial)];
        if let Some(sender) = call.sender() {
            fields.push(Field::Destination(sender.to_owned()));
        }
        Self {
            kind: MessageType::MethodReturn,
            flags: NO_REPLY_EXPECTED,
            serial,
            fields,
            body: Vec::new(),
        }
    }

    pub(crate) fn error(serial: u32, call: &Self, name: &str, text: &str) -> Self {
        let mut fields = vec![
            Field::ReplySerial(call.serial),
            Field::ErrorName(name.to_owned()),
        ];
        if let Some(sender) = call.sender() {
            fields.push(Field::Destination(sender.to_owned()));
        }
        Self {
            kind: MessageType::Error,
            flags: NO_REPLY_EXPECTED,
            serial,
            fields,
            body: Vec::new(),
        }
        .with_body(vec![Arg::Value(Value::Str(text.to_owned()))])
    }

    #[allow(
        dead_code,
        reason = "the interfaces above the connection are the callers, and land next"
    )]
    pub(crate) fn signal(serial: u32, path: &str, interface: &str, member: &str) -> Self {
        Self {
            kind: MessageType::Signal,
            flags: NO_REPLY_EXPECTED,
            serial,
            fields: vec![
                Field::Path(path.to_owned()),
                Field::Interface(interface.to_owned()),
                Field::Member(member.to_owned()),
            ],
            body: Vec::new(),
        }
    }

    /// Sets the body *and* the SIGNATURE field that describes it, together, so
    /// the two cannot be left disagreeing. [`marshal`] refuses a message where
    /// they do, which is the second half of the same guard.
    pub(crate) fn with_body(mut self, body: Vec<Arg>) -> Self {
        self.fields
            .retain(|field| !matches!(field, Field::Signature(_)));
        let signature = body_signature(&body);
        if !signature.is_empty() {
            self.fields.push(Field::Signature(signature));
        }
        self.body = body;
        self
    }

    pub(crate) fn path(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::Path(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub(crate) fn interface(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::Interface(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub(crate) fn member(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::Member(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub(crate) fn error_name(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::ErrorName(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub(crate) fn sender(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::Sender(value) => Some(value.as_str()),
            _ => None,
        })
    }

    #[allow(
        dead_code,
        reason = "the interfaces above the connection are the callers, and land next"
    )]
    pub(crate) fn destination(&self) -> Option<&str> {
        self.fields.iter().find_map(|field| match field {
            Field::Destination(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub(crate) fn reply_serial(&self) -> Option<u32> {
        self.fields.iter().find_map(|field| match field {
            Field::ReplySerial(value) => Some(*value),
            _ => None,
        })
    }

    /// The declared body signature. Absent and empty mean the same thing on the
    /// wire - both peers in the captures spell an empty body both ways.
    pub(crate) fn signature(&self) -> &str {
        self.fields
            .iter()
            .find_map(|field| match field {
                Field::Signature(value) => Some(value.as_str()),
                _ => None,
            })
            .unwrap_or_default()
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

/// Is this an object path?
///
/// Spec 0.43: begins with `/`, no trailing `/` unless it is the root, no empty
/// element, and every other character from `[A-Za-z0-9_]`. Checked on the way
/// out as well as on the way in, because a bad path here is not our error to
/// see: the peer answers a malformed message by hanging up without a word.
fn is_object_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    path.split('/').skip(1).all(|element| {
        !element.is_empty()
            && element
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
}

/// Builds a message in one buffer.
///
/// Every alignment is measured against `buf.len()`, which is the offset from
/// byte 0 of the message, because that is what the specification counts from.
/// Building a body in its own buffer and splicing it in would be the same thing
/// only while nothing in front of it changed length - so there is one buffer and
/// no second origin.
struct Writer {
    buf: Vec<u8>,
    endian: Endian,
}

impl Writer {
    fn new(endian: Endian) -> Self {
        Self {
            buf: Vec::new(),
            endian,
        }
    }

    /// Nul bytes up to the next multiple of `alignment`. The specification
    /// requires the padding to be nul, and the parser here enforces that.
    fn pad_to(&mut self, alignment: usize) {
        while !self.buf.len().is_multiple_of(alignment) {
            self.buf.push(0);
        }
    }

    fn byte(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.pad_to(4);
        self.buf.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn i32(&mut self, value: i32) {
        self.pad_to(4);
        self.buf.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn i64(&mut self, value: i64) {
        self.pad_to(8);
        self.buf.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn f64(&mut self, value: f64) {
        self.pad_to(8);
        self.buf.extend_from_slice(&match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }

    fn patch_u32(&mut self, at: usize, value: u32) {
        let bytes = match self.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        self.buf[at..at + 4].copy_from_slice(&bytes);
    }

    /// `s` and `o`: a 4-aligned length in bytes, the bytes, then a nul that the
    /// length does not count.
    fn string(&mut self, value: &str) -> Result<()> {
        if value.as_bytes().contains(&0) {
            return Err(bad("a string may not contain a nul"));
        }
        let len =
            u32::try_from(value.len()).map_err(|_| bad("a string longer than a message may be"))?;
        self.u32(len);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    /// `g`: a one-byte length, so it aligns to 1 - but whatever follows it does
    /// not, which is the trap the variant writer below is careful about.
    fn signature(&mut self, value: &str) -> Result<()> {
        let len = u8::try_from(value.len())
            .map_err(|_| bad(format!("a signature longer than {MAX_SIGNATURE_LEN} bytes")))?;
        self.byte(len);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    /// Any array: a 4-aligned length, padding out to the *element's* alignment,
    /// then the elements.
    ///
    /// Two rules live here and both fail silently when they are got wrong. The
    /// length counts only the elements, never the padding in front of them. And
    /// that padding is written even when there are no elements at all, which is
    /// why an empty `a{sv}` is not simply four zero bytes.
    fn array(
        &mut self,
        element_alignment: usize,
        fill: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        self.u32(0);
        let length_at = self.buf.len() - 4;
        self.pad_to(element_alignment);
        let first = self.buf.len();
        fill(self)?;
        let len = self.buf.len() - first;
        if len > MAX_ARRAY_LEN {
            return Err(bad("an array longer than the specification allows"));
        }
        let len = u32::try_from(len).map_err(|_| bad("an array longer than a message may be"))?;
        self.patch_u32(length_at, len);
        Ok(())
    }

    fn object_path(&mut self, path: &str) -> Result<()> {
        if !is_object_path(path) {
            return Err(bad(format!("\"{path}\" is not an object path")));
        }
        self.string(path)
    }

    fn value(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::Bool(flag) => {
                self.u32(u32::from(*flag));
                Ok(())
            }
            Value::Int32(number) => {
                self.i32(*number);
                Ok(())
            }
            Value::Uint32(number) => {
                self.u32(*number);
                Ok(())
            }
            Value::Int64(number) => {
                self.i64(*number);
                Ok(())
            }
            Value::Double(number) => {
                self.f64(*number);
                Ok(())
            }
            Value::Str(text) => self.string(text),
            Value::Path(path) => self.object_path(path),
            Value::Strings(list) => self.array(4, |writer| {
                for text in list {
                    writer.string(text)?;
                }
                Ok(())
            }),
        }
    }

    /// `v`: the contained type's signature, then the value at *its* own
    /// alignment. The signature aligns to 1; what follows it does not.
    fn variant(&mut self, variant: &Variant) -> Result<()> {
        self.signature(variant.signature())?;
        match variant {
            Variant::Value(value) => self.value(value),
            Variant::Dict(entries) => self.metadata(entries),
        }
    }

    /// `a{sv}` whose members are variants. A dict entry is a struct, so it
    /// aligns to 8 whatever is in it.
    fn dict(&mut self, entries: &[(String, Variant)]) -> Result<()> {
        self.array(8, |writer| {
            for (key, value) in entries {
                writer.pad_to(8);
                writer.string(key)?;
                writer.variant(value)?;
            }
            Ok(())
        })
    }

    /// `a{sv}` one level down, where a member is always a leaf. Spelled out
    /// rather than shared with [`Writer::dict`] so that the nesting has a floor
    /// in the type system instead of a depth counter.
    fn metadata(&mut self, entries: &[(String, Value)]) -> Result<()> {
        self.array(8, |writer| {
            for (key, value) in entries {
                writer.pad_to(8);
                writer.string(key)?;
                writer.signature(value.signature())?;
                writer.value(value)?;
            }
            Ok(())
        })
    }

    fn arg(&mut self, arg: &Arg) -> Result<()> {
        match arg {
            Arg::Value(value) => self.value(value),
            Arg::Variant(variant) => self.variant(variant),
            Arg::Dict(entries) => self.dict(entries),
        }
    }

    fn field(&mut self, field: &Field) -> Result<()> {
        self.pad_to(8);
        self.byte(field.code());
        match field {
            Field::Path(path) => {
                self.signature("o")?;
                self.object_path(path)
            }
            Field::Interface(text)
            | Field::Member(text)
            | Field::ErrorName(text)
            | Field::Destination(text)
            | Field::Sender(text) => {
                self.signature("s")?;
                self.string(text)
            }
            Field::Signature(text) => {
                self.signature("g")?;
                self.signature(text)
            }
            Field::ReplySerial(number) | Field::UnixFds(number) => {
                self.signature("u")?;
                self.u32(*number);
                Ok(())
            }
        }
    }
}

/// Turn a message into the bytes that go on the socket.
///
/// `endian` is the sender's choice; priel uses [`Endian::NATIVE`] so this is a
/// copy rather than a byte swap, and takes the other only to reproduce a
/// message it parsed.
pub(crate) fn marshal(message: &Message, endian: Endian) -> Result<Vec<u8>> {
    if message.serial == 0 {
        return Err(bad("serial 0 is reserved and identifies no call"));
    }
    let declared = message.signature();
    let actual = body_signature(&message.body);
    if declared != actual {
        return Err(bad(format!(
            "SIGNATURE says \"{declared}\" but the body is \"{actual}\""
        )));
    }

    let mut writer = Writer::new(endian);
    writer.byte(endian.marker());
    writer.byte(message.kind.code());
    writer.byte(message.flags);
    writer.byte(PROTOCOL_VERSION);
    // Both lengths are patched once they are known, which is what keeps every
    // offset measured from byte 0 of this same buffer.
    writer.u32(0);
    writer.u32(message.serial);
    writer.array(8, |writer| {
        for field in &message.fields {
            writer.field(field)?;
        }
        Ok(())
    })?;
    // The body begins on an 8-byte boundary whatever the fields came to, and
    // that padding belongs to neither length.
    writer.pad_to(8);
    let body_at = writer.buf.len();
    for arg in &message.body {
        writer.arg(arg)?;
    }
    let body_len = u32::try_from(writer.buf.len() - body_at)
        .map_err(|_| bad("a body longer than a message may be"))?;
    writer.patch_u32(4, body_len);

    if writer.buf.len() > MAX_MESSAGE_LEN {
        return Err(bad("a message longer than the specification allows"));
    }
    Ok(writer.buf)
}

/// Reads a message out of the buffer it arrived in.
///
/// `at` is an offset into the whole frame and never into a sub-slice, for the
/// same reason the writer builds into one buffer: alignment is measured from
/// byte 0 of the message, so a reader over a slice of it would be padding
/// against the wrong origin.
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
    endian: Endian,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], at: usize, endian: Endian) -> Self {
        Self { buf, at, endian }
    }

    /// Steps over alignment padding, which the specification requires to be
    /// nul. Enforcing that turns a marshaller that padded wrongly into an error
    /// here instead of a value read from the wrong offset.
    fn pad_to(&mut self, alignment: usize) -> Result<()> {
        while !self.at.is_multiple_of(alignment) {
            if self.byte()? != 0 {
                return Err(bad("alignment padding that is not nul"));
            }
        }
        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| bad("a length that overflows the buffer"))?;
        let slice = self
            .buf
            .get(self.at..end)
            .ok_or_else(|| bad("a value that runs past the end of the message"))?;
        self.at = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn four(&mut self) -> Result<[u8; 4]> {
        self.pad_to(4)?;
        let bytes = self.take(4)?;
        Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn eight(&mut self) -> Result<[u8; 8]> {
        self.pad_to(8)?;
        let bytes = self.take(8)?;
        Ok([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.four()?;
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        })
    }

    fn i32(&mut self) -> Result<i32> {
        let bytes = self.four()?;
        Ok(match self.endian {
            Endian::Little => i32::from_le_bytes(bytes),
            Endian::Big => i32::from_be_bytes(bytes),
        })
    }

    fn i64(&mut self) -> Result<i64> {
        let bytes = self.eight()?;
        Ok(match self.endian {
            Endian::Little => i64::from_le_bytes(bytes),
            Endian::Big => i64::from_be_bytes(bytes),
        })
    }

    fn f64(&mut self) -> Result<f64> {
        let bytes = self.eight()?;
        Ok(match self.endian {
            Endian::Little => f64::from_le_bytes(bytes),
            Endian::Big => f64::from_be_bytes(bytes),
        })
    }

    /// Spec 0.43: a boolean is a `u32` that is 0 or 1, and no other value is
    /// valid. Reading 2 as true would be a guess.
    fn boolean(&mut self) -> Result<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(bad(format!("{other} is not a boolean"))),
        }
    }

    fn length(&mut self) -> Result<usize> {
        usize::try_from(self.u32()?).map_err(|_| bad("a length this machine cannot address"))
    }

    fn string(&mut self) -> Result<String> {
        let len = self.length()?;
        let bytes = self.take(len)?;
        if bytes.contains(&0) {
            return Err(bad("a string containing a nul"));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| bad("a string that is not utf-8"))?
            .to_owned();
        if self.byte()? != 0 {
            return Err(bad("a string not terminated by a nul"));
        }
        Ok(text)
    }

    fn object_path(&mut self) -> Result<String> {
        let path = self.string()?;
        if !is_object_path(&path) {
            return Err(bad(format!("\"{path}\" is not an object path")));
        }
        Ok(path)
    }

    /// `g`: a one-byte length, so it needs no alignment - but what follows it
    /// does, which is why nothing here pads and every caller does.
    fn signature(&mut self) -> Result<String> {
        let len = usize::from(self.byte()?);
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| bad("a signature that is not utf-8"))?
            .to_owned();
        if self.byte()? != 0 {
            return Err(bad("a signature not terminated by a nul"));
        }
        Ok(text)
    }

    /// Any array: the length, the padding out to the element's alignment that
    /// the length does not count, then elements until the length runs out.
    ///
    /// The loop is bounded by that length and additionally by progress: an
    /// element that consumes nothing would otherwise spin on a crafted message.
    fn array<T>(
        &mut self,
        element_alignment: usize,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let len = self.length()?;
        if len > MAX_ARRAY_LEN {
            return Err(bad("an array longer than the specification allows"));
        }
        self.pad_to(element_alignment)?;
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| bad("an array length that overflows the buffer"))?;
        // Bounded twice over: by the declared length, and by every element
        // consuming at least one byte of a buffer that is already at most one
        // message long. An array running past the end fails in `take`.
        let mut items = Vec::new();
        while self.at < end {
            let before = self.at;
            items.push(read(self)?);
            if self.at <= before {
                return Err(bad("an array element that consumed no bytes"));
            }
        }
        if self.at != end {
            return Err(bad("an array element that ran past the array's own length"));
        }
        Ok(items)
    }

    fn value(&mut self, kind: Kind) -> Result<Value> {
        match kind {
            Kind::Bool => Ok(Value::Bool(self.boolean()?)),
            Kind::Int32 => Ok(Value::Int32(self.i32()?)),
            Kind::Uint32 => Ok(Value::Uint32(self.u32()?)),
            Kind::Int64 => Ok(Value::Int64(self.i64()?)),
            Kind::Double => Ok(Value::Double(self.f64()?)),
            Kind::Str => Ok(Value::Str(self.string()?)),
            Kind::Path => Ok(Value::Path(self.object_path()?)),
            Kind::Strings => Ok(Value::Strings(self.array(4, Self::string)?)),
            Kind::Variant | Kind::Dict => Err(bad("a container where a leaf value was declared")),
        }
    }

    fn variant(&mut self) -> Result<Variant> {
        let signature = self.signature()?;
        match one_kind(&signature)? {
            Kind::Dict => Ok(Variant::Dict(self.metadata()?)),
            Kind::Variant => Err(bad("a variant directly inside a variant")),
            leaf => Ok(Variant::Value(self.value(leaf)?)),
        }
    }

    /// A `v` that has to hold a leaf. `Metadata`'s members are leaves, so a
    /// dictionary three deep is a shape MPRIS does not define, and refusing it
    /// is what keeps the nesting bounded without a depth counter.
    fn leaf_variant(&mut self) -> Result<Value> {
        let signature = self.signature()?;
        self.value(one_kind(&signature)?)
    }

    fn dict(&mut self) -> Result<Vec<(String, Variant)>> {
        self.array(8, |reader| {
            reader.pad_to(8)?;
            let key = reader.string()?;
            Ok((key, reader.variant()?))
        })
    }

    fn metadata(&mut self) -> Result<Vec<(String, Value)>> {
        self.array(8, |reader| {
            reader.pad_to(8)?;
            let key = reader.string()?;
            Ok((key, reader.leaf_variant()?))
        })
    }

    fn arg(&mut self, kind: Kind) -> Result<Arg> {
        match kind {
            Kind::Variant => Ok(Arg::Variant(self.variant()?)),
            Kind::Dict => Ok(Arg::Dict(self.dict()?)),
            leaf => Ok(Arg::Value(self.value(leaf)?)),
        }
    }

    /// One `(yv)`.
    ///
    /// `None` is a field code priel does not know, which spec 0.43 says a
    /// receiver must ignore. That is honoured for the four types the standard
    /// fields use; a peer that invents a field of some other type costs itself
    /// one message rather than getting it guessed at. No implementation does.
    fn header_field(&mut self) -> Result<Option<Field>> {
        self.pad_to(8)?;
        let code = self.byte()?;
        let signature = self.signature()?;
        let field = match (code, signature.as_str()) {
            (1, "o") => Field::Path(self.object_path()?),
            (2, "s") => Field::Interface(self.string()?),
            (3, "s") => Field::Member(self.string()?),
            (4, "s") => Field::ErrorName(self.string()?),
            (5, "u") => Field::ReplySerial(self.u32()?),
            (6, "s") => Field::Destination(self.string()?),
            (7, "s") => Field::Sender(self.string()?),
            (8, "g") => Field::Signature(self.signature()?),
            (9, "u") => Field::UnixFds(self.u32()?),
            (1..=9, other) => {
                return Err(bad(format!(
                    "header field {code} carries a \"{other}\", not the type it is defined as"
                )));
            }
            (_, "s") => {
                self.string()?;
                return Ok(None);
            }
            (_, "o") => {
                self.object_path()?;
                return Ok(None);
            }
            (_, "g") => {
                self.signature()?;
                return Ok(None);
            }
            (_, "u") => {
                self.u32()?;
                return Ok(None);
            }
            (_, other) => {
                return Err(bad(format!(
                    "header field {code} carries a \"{other}\", which cannot be stepped over"
                )));
            }
        };
        Ok(Some(field))
    }
}

/// A signature that names exactly one type, which is what a variant carries.
fn one_kind(signature: &str) -> Result<Kind> {
    match parse_signature(signature)?[..] {
        [kind] => Ok(kind),
        _ => Err(bad(format!(
            "a variant declared \"{signature}\", which is not one type"
        ))),
    }
}

/// Length of the whole message beginning at `buf[0]`.
///
/// `None` means the buffer does not hold enough to say yet, which is the answer
/// the receive loop needs: it accumulates across read timeouts and only parses
/// complete frames, so a timeout landing mid-message is not a special case.
///
/// The errors here are [`WireError::Framing`] and cost the connection, because
/// a fixed header that is not one leaves no way to find where the next message
/// starts.
pub(crate) fn frame_len(buf: &[u8]) -> Result<Option<usize>> {
    if buf.len() < FIXED_HEADER_LEN {
        return Ok(None);
    }
    let endian = Endian::from_marker(buf[0])?;
    if buf[3] != PROTOCOL_VERSION {
        return Err(WireError::Framing("protocol version is not 1"));
    }
    let read = |at: usize| {
        let bytes = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        match endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        }
    };
    let body_len = usize::try_from(read(4))
        .map_err(|_| WireError::Framing("a body length this machine cannot address"))?;
    let fields_len = usize::try_from(read(12))
        .map_err(|_| WireError::Framing("a field length this machine cannot address"))?;
    if fields_len > MAX_ARRAY_LEN {
        return Err(WireError::Framing(
            "header fields longer than an array may be",
        ));
    }
    // The body starts on an 8-byte boundary however long the fields came to.
    let total = FIXED_HEADER_LEN
        .checked_add(fields_len)
        .and_then(|head| head.checked_next_multiple_of(8))
        .and_then(|head| head.checked_add(body_len))
        .ok_or(WireError::Framing("a length that overflows"))?;
    if total > MAX_MESSAGE_LEN {
        return Err(WireError::Framing(
            "a message longer than the specification allows",
        ));
    }
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some(total))
}

/// Turn one whole frame - exactly what [`frame_len`] measured, no more - into a
/// message.
///
/// Everything outside the closed set of signatures is an error and never a
/// guess: a body read as the wrong shape is silent, where an error is not.
pub(crate) fn parse(frame: &[u8]) -> Result<Message> {
    match frame_len(frame)? {
        None => return Err(WireError::Framing("less than a whole message")),
        Some(total) if total != frame.len() => {
            return Err(WireError::Framing("more than one whole message"));
        }
        Some(_) => {}
    }
    let endian = Endian::from_marker(frame[0])?;
    let kind = MessageType::from_code(frame[1])?;
    let flags = frame[2];

    let mut reader = Reader::new(frame, 8, endian);
    let serial = reader.u32()?;
    if serial == 0 {
        return Err(bad("serial 0 identifies no call"));
    }
    let fields: Vec<Field> = reader
        .array(8, Reader::header_field)?
        .into_iter()
        .flatten()
        .collect();
    reader.pad_to(8)?;

    // Assembled before the body so that the header checks and the signature can
    // be read through the same accessors everything above this uses.
    let message = Message {
        kind,
        flags,
        serial,
        fields,
        body: Vec::new(),
    };
    require_fields(&message)?;

    let mut body = Vec::new();
    for declared in parse_signature(message.signature())? {
        body.push(reader.arg(declared)?);
    }
    if reader.at != frame.len() {
        return Err(bad("a body that does not fill its own declared length"));
    }
    Ok(Message { body, ..message })
}

/// Spec 0.43 makes some header fields required per message type, and a peer
/// that leaves one out has sent something priel cannot answer. Checked once
/// here so that everything above can read them without a second opinion.
fn require_fields(message: &Message) -> Result<()> {
    let complete = match message.kind {
        MessageType::MethodCall => message.path().is_some() && message.member().is_some(),
        MessageType::MethodReturn => message.reply_serial().is_some(),
        MessageType::Error => message.reply_serial().is_some() && message.error_name().is_some(),
        MessageType::Signal => {
            message.path().is_some() && message.interface().is_some() && message.member().is_some()
        }
    };
    if !complete {
        return Err(bad("a message missing a header field its type requires"));
    }
    Ok(())
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
    use super::vectors;
    use super::*;

    fn str_value(text: &str) -> Value {
        Value::Str(text.to_owned())
    }

    fn strings(list: &[&str]) -> Value {
        Value::Strings(list.iter().map(|text| (*text).to_owned()).collect())
    }

    fn entry(key: &str, value: Value) -> (String, Variant) {
        (key.to_owned(), Variant::Value(value))
    }

    fn leaf(key: &str, value: Value) -> (String, Value) {
        (key.to_owned(), value)
    }

    /// The `PropertiesChanged` gdbus emitted, rebuilt as a message.
    fn properties_changed() -> Message {
        Message {
            kind: MessageType::Signal,
            flags: NO_REPLY_EXPECTED,
            serial: 2,
            fields: vec![
                Field::Path("/org/mpris/MediaPlayer2".into()),
                Field::Interface("org.freedesktop.DBus.Properties".into()),
                Field::Signature("sa{sv}as".into()),
                Field::Member("PropertiesChanged".into()),
                Field::Sender(":1.8621".into()),
            ],
            body: vec![
                Arg::Value(str_value("org.mpris.MediaPlayer2.Player")),
                Arg::Dict(vec![
                    entry("PlaybackStatus", str_value("Playing")),
                    (
                        "Metadata".into(),
                        Variant::Dict(vec![
                            leaf("mpris:trackid", Value::Path("/priel/track/17".into())),
                            leaf("mpris:length", Value::Int64(245_000_000)),
                            leaf("xesam:title", str_value("Bloom")),
                            leaf("xesam:artist", strings(&["Radiohead", "Nigel"])),
                            leaf("xesam:trackNumber", Value::Int32(1)),
                        ]),
                    ),
                ]),
                Arg::Value(strings(&[])),
            ],
        }
    }

    /// Every full message captured off a real bus, named for the assertion
    /// failures.
    fn captures() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("GETALL_ROOT_REPLY", vectors::GETALL_ROOT_REPLY),
            ("GET_POSITION_REPLY", vectors::GET_POSITION_REPLY),
            ("PING_REPLY", vectors::PING_REPLY),
            ("GETALL_CALL", vectors::GETALL_CALL),
            ("GET_CALL", vectors::GET_CALL),
            ("PING_CALL", vectors::PING_CALL),
            ("SET_CALL", vectors::SET_CALL),
            ("ERROR_REPLY", vectors::ERROR_REPLY),
            ("SEEK_CALL", vectors::SEEK_CALL),
            ("SETPOSITION_CALL", vectors::SETPOSITION_CALL),
            ("SEEKED_SIGNAL", vectors::SEEKED_SIGNAL),
            ("PROPERTIES_CHANGED", vectors::PROPERTIES_CHANGED),
            (
                "PROPERTIES_CHANGED_EMPTY_DICT",
                vectors::PROPERTIES_CHANGED_EMPTY_DICT,
            ),
            (
                "PROPERTIES_CHANGED_PADDED_DICT",
                vectors::PROPERTIES_CHANGED_PADDED_DICT,
            ),
            (
                "PROPERTIES_CHANGED_PADDED_ENTRIES",
                vectors::PROPERTIES_CHANGED_PADDED_ENTRIES,
            ),
        ]
    }

    /// The body gdbus emitted for the alignment family, behind a leading string
    /// of `pad` characters.
    fn alignment_body(pad: usize) -> Vec<Arg> {
        vec![
            Arg::Value(str_value(&"a".repeat(pad))),
            Arg::Value(Value::Int64(-8)),
            Arg::Value(Value::Double(1.5)),
            Arg::Value(Value::Uint32(7)),
            Arg::Value(Value::Bool(true)),
            Arg::Value(strings(&[])),
            Arg::Value(strings(&["one", "two"])),
            Arg::Dict(Vec::new()),
            Arg::Dict(vec![entry("k", Value::Int64(5))]),
            Arg::Variant(Variant::Value(Value::Int64(9))),
        ]
    }

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

    /// Goal: the richest body priel sends marshals to the bytes a real
    /// implementation produced for the same value. Method: rebuild the
    /// `PropertiesChanged` gdbus put on this machine's bus and compare byte for
    /// byte. It carries every container at once - `a{sv}` holding a variant
    /// holding another `a{sv}` holding an `as` - which is where the padding
    /// rules interact.
    #[test]
    fn a_whole_track_change_marshals_to_the_bytes_glib_produced() {
        let bytes = marshal(&properties_changed(), Endian::Little);
        assert_eq!(bytes.as_deref(), Ok(vectors::PROPERTIES_CHANGED));
    }

    /// Goal: a `GetAll` reply matches what a shipping media player answers with.
    /// Method: rebuild Chromium's reply on the root interface. Its two empty
    /// `as` values are the case that looks like it needs no bytes and does.
    #[test]
    fn a_property_dictionary_marshals_to_the_bytes_a_real_player_produced() {
        let message = Message {
            kind: MessageType::MethodReturn,
            flags: NO_REPLY_EXPECTED,
            serial: 2471,
            fields: vec![
                Field::Destination(":1.8614".into()),
                Field::ReplySerial(3),
                Field::Signature("a{sv}".into()),
                Field::Sender(":1.150".into()),
            ],
            body: vec![Arg::Dict(vec![
                entry("CanQuit", Value::Bool(false)),
                entry("CanRaise", Value::Bool(false)),
                entry("HasTrackList", Value::Bool(false)),
                entry("Identity", str_value("Chrome")),
                entry("SupportedMimeTypes", strings(&[])),
                entry("SupportedUriSchemes", strings(&[])),
            ])],
        };
        let bytes = marshal(&message, Endian::Little);
        assert_eq!(bytes.as_deref(), Ok(vectors::GETALL_ROOT_REPLY));
    }

    /// Goal: an `ox` body and a header field priel never writes both come out
    /// right. Method: rebuild the `SetPosition` call gdbus sent, `UNIX_FDS` and
    /// all. The `x` needs padding after the path, and the field list is in the
    /// order the peer chose rather than any order of priel's.
    #[test]
    fn a_seek_to_a_track_marshals_to_the_bytes_glib_produced() {
        let message = Message {
            kind: MessageType::MethodCall,
            flags: 0,
            serial: 3,
            fields: vec![
                Field::Path("/org/mpris/MediaPlayer2".into()),
                Field::Interface("org.mpris.MediaPlayer2.Player".into()),
                Field::Destination("org.priel.Absent".into()),
                Field::Signature("ox".into()),
                Field::Member("SetPosition".into()),
                Field::UnixFds(0),
                Field::Sender(":1.8628".into()),
            ],
            body: vec![
                Arg::Value(Value::Path("/priel/track/17".into())),
                Arg::Value(Value::Int64(42_000_000)),
            ],
        };
        let bytes = marshal(&message, Endian::Little);
        assert_eq!(bytes.as_deref(), Ok(vectors::SETPOSITION_CALL));
    }

    /// Goal: an empty `a{sv}` still writes the padding its length does not
    /// count. Method: rebuild the `PropertiesChanged` gdbus emitted behind a
    /// 25-character interface name, which is the length that puts the
    /// dictionary's length word on an 8-byte boundary. Four nul bytes follow it
    /// that the length reads as zero, and a marshaller that skips them produces
    /// a message the bus answers by hanging up without a word.
    #[test]
    fn an_empty_dictionary_still_pads_to_its_element_alignment() {
        let message = Message {
            kind: MessageType::Signal,
            flags: NO_REPLY_EXPECTED,
            serial: 2,
            fields: vec![
                Field::Path("/org/mpris/MediaPlayer2".into()),
                Field::Interface("org.freedesktop.DBus.Properties".into()),
                Field::Signature("sa{sv}as".into()),
                Field::Member("PropertiesChanged".into()),
                Field::Sender(":1.8630".into()),
            ],
            body: vec![
                Arg::Value(str_value("org.freedesktop.DBus.Peer")),
                Arg::Dict(Vec::new()),
                Arg::Value(strings(&["Metadata"])),
            ],
        };
        let bytes = marshal(&message, Endian::Little);
        assert_eq!(
            bytes.as_deref(),
            Ok(vectors::PROPERTIES_CHANGED_PADDED_DICT)
        );
    }

    /// Goal: every value lands at the right offset whatever precedes it.
    /// Method: gdbus emitted one body of ten values at all eight offsets modulo
    /// 8, behind a leading string of length 0 to 7. Marshal the same value at
    /// each phase and compare the body bytes.
    ///
    /// Round-tripping cannot replace this. A marshaller and a parser that share
    /// the same wrong padding rule agree with each other perfectly, and the
    /// only thing that catches them is a third party that already works.
    #[test]
    fn every_value_lands_at_its_alignment_at_all_eight_phases() {
        for (pad, expected) in vectors::ALIGNMENT_BODIES.iter().enumerate() {
            let message = Message::signal(2, "/priel", "org.priel.Wire", "Sample")
                .with_body(alignment_body(pad));
            let bytes = marshal(&message, Endian::Little).expect("the sample body marshals");
            let body = &bytes[bytes.len() - expected.len()..];
            assert_eq!(
                body, *expected,
                "body differs from the capture with {pad} characters in front of it"
            );
            // The tail comparison above only means something if the body really
            // is that long, which the header's own length field is the record
            // of.
            let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            assert_eq!(declared as usize, expected.len());
        }
    }

    /// Goal: big-endian output is the same message with the numbers turned
    /// round, and nothing else moves. Method: marshal one body both ways and
    /// compare lengths and the bytes that are not numbers.
    #[test]
    fn the_other_byte_order_changes_the_numbers_and_not_the_layout() {
        let message = properties_changed();
        let little = marshal(&message, Endian::Little).expect("little-endian marshals");
        let big = marshal(&message, Endian::Big).expect("big-endian marshals");
        assert_eq!(little.len(), big.len());
        assert_eq!(little[0], b'l');
        assert_eq!(big[0], b'B');
        assert_eq!(&little[4..8], &[0x34, 0x01, 0x00, 0x00]);
        assert_eq!(&big[4..8], &[0x00, 0x00, 0x01, 0x34]);
    }

    /// Goal: a message priel could not have meant is refused here rather than
    /// on the socket. Method: one case per guard. The specification's answer to
    /// a malformed message is to drop the connection without notice, so a
    /// mistake that leaves this function is a bus that goes quiet.
    #[test]
    fn a_message_that_cannot_be_meant_is_refused() {
        let ok = Message::signal(1, "/priel", "org.priel.Wire", "Sample");
        assert!(marshal(&ok, Endian::Little).is_ok());

        let mut zero_serial = ok.clone();
        zero_serial.serial = 0;
        assert!(marshal(&zero_serial, Endian::Little).is_err());

        let mut lying_signature = ok.clone();
        lying_signature.fields.push(Field::Signature("s".into()));
        assert!(marshal(&lying_signature, Endian::Little).is_err());

        let mut nul_in_a_string = ok.clone();
        nul_in_a_string.fields.push(Field::Sender("a\0b".into()));
        assert!(marshal(&nul_in_a_string, Endian::Little).is_err());

        let not_a_path = ok
            .clone()
            .with_body(vec![Arg::Value(Value::Path("priel/track/1".into()))]);
        assert!(marshal(&not_a_path, Endian::Little).is_err());

        // A signature field is one byte long, so a body of more arguments than
        // that can describe has no way onto the wire.
        let too_many = ok.with_body(
            (0..=MAX_SIGNATURE_LEN)
                .map(|_| Arg::Value(str_value("a")))
                .collect(),
        );
        assert!(marshal(&too_many, Endian::Little).is_err());
    }

    /// Goal: `mpris:trackid` is checked before it reaches a consumer that type
    /// checks it. Method: a table of the paths the specification allows and the
    /// ones it does not.
    #[test]
    fn object_paths_are_checked_against_the_grammar() {
        for good in ["/", "/priel", "/priel/track/17", "/a_b/C9"] {
            assert!(is_object_path(good), "\"{good}\" is a valid object path");
        }
        for poor in [
            "",
            "priel",
            "/priel/",
            "/priel//track",
            "/priel-track",
            "//",
            "/a.b",
        ] {
            assert!(!is_object_path(poor), "\"{poor}\" is not an object path");
        }
    }

    /// Goal: the two constructors set the fields that address a reply, so a
    /// caller cannot answer the wrong peer. Method: build a call with a sender
    /// and check what the reply and the error carry back.
    #[test]
    fn a_reply_is_addressed_from_the_call_it_answers() {
        let mut call = Message::method_call(
            7,
            "org.priel.Absent",
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            "Next",
        );
        call.fields.push(Field::Sender(":1.42".into()));

        let reply = Message::method_return(1, &call);
        assert_eq!(reply.reply_serial(), Some(7));
        assert_eq!(reply.destination(), Some(":1.42"));
        assert_eq!(reply.flags, NO_REPLY_EXPECTED);

        let failed = Message::error(2, &call, "org.freedesktop.DBus.Error.Failed", "no");
        assert_eq!(failed.reply_serial(), Some(7));
        assert_eq!(failed.destination(), Some(":1.42"));
        assert_eq!(
            failed.error_name(),
            Some("org.freedesktop.DBus.Error.Failed")
        );
        assert_eq!(failed.signature(), "s");
    }

    /// Goal: parsing and marshalling are inverses over every shape that has
    /// been seen on a real bus. Method: parse each capture and marshal it
    /// straight back, and require the same bytes. Field order comes back too,
    /// which is why the fields are a list and not a set of named options.
    #[test]
    fn every_captured_message_survives_a_round_trip_unchanged() {
        for (name, bytes) in captures() {
            let message = parse(bytes).unwrap_or_else(|error| panic!("{name} parses: {error}"));
            let again = marshal(&message, Endian::Little)
                .unwrap_or_else(|error| panic!("{name} marshals back: {error}"));
            assert_eq!(again, bytes, "{name} did not survive the round trip");
        }
    }

    /// Goal: every supported shape survives a round trip at every offset
    /// modulo 8, in both byte orders. Method: the ten-value sample body behind
    /// a leading string of length 0 to 7 - which is the same body the golden
    /// comparison pins the bytes of, so this adds the reader to a layout that
    /// is already known to be right rather than testing the two against each
    /// other alone.
    #[test]
    fn every_shape_round_trips_at_all_eight_phases() {
        for pad in 0..8 {
            let message = Message::signal(1, "/priel", "org.priel.Wire", "Sample")
                .with_body(alignment_body(pad));
            for endian in [Endian::Little, Endian::Big] {
                let bytes = marshal(&message, endian)
                    .unwrap_or_else(|error| panic!("phase {pad} marshals: {error}"));
                assert_eq!(
                    parse(&bytes),
                    Ok(message.clone()),
                    "phase {pad} did not survive the round trip"
                );
            }
        }
    }

    /// Goal: parsing means what it says, not merely something reversible.
    /// Method: read the header and the body out of three captures and check
    /// them against what the tools reported at capture time. A round trip alone
    /// would pass with the whole message read at a constant offset.
    #[test]
    fn a_parsed_message_carries_what_the_capture_showed() {
        let call = parse(vectors::GET_CALL).expect("the Get call parses");
        assert_eq!(call.kind, MessageType::MethodCall);
        assert_eq!(call.interface(), Some("org.freedesktop.DBus.Properties"));
        assert_eq!(call.member(), Some("Get"));
        assert_eq!(call.path(), Some("/org/mpris/MediaPlayer2"));
        assert_eq!(call.signature(), "ss");
        assert_eq!(
            call.body,
            vec![
                Arg::Value(str_value("org.mpris.MediaPlayer2.Player")),
                Arg::Value(str_value("Position")),
            ]
        );

        let position = parse(vectors::GET_POSITION_REPLY).expect("the Position reply parses");
        assert_eq!(position.kind, MessageType::MethodReturn);
        assert_eq!(position.reply_serial(), Some(3));
        assert_eq!(
            position.body,
            vec![Arg::Variant(Variant::Value(Value::Int64(31_785_771)))]
        );

        let changed = parse(vectors::PROPERTIES_CHANGED).expect("the signal parses");
        assert_eq!(changed, properties_changed());

        let failed = parse(vectors::ERROR_REPLY).expect("the error parses");
        assert_eq!(failed.kind, MessageType::Error);
        assert_eq!(
            failed.error_name(),
            Some("org.freedesktop.DBus.Error.ServiceUnknown")
        );
    }

    /// Goal: a peer's byte order is honoured rather than assumed. Method:
    /// marshal one message big-endian and parse it back. Both orders are legal
    /// and a receiver does not get to pick.
    #[test]
    fn a_big_endian_message_parses_to_the_same_value() {
        let message = properties_changed();
        let bytes = marshal(&message, Endian::Big).expect("big-endian marshals");
        assert_eq!(bytes[0], b'B');
        assert_eq!(parse(&bytes), Ok(message));
    }

    /// Goal: a message that arrives in pieces is only parsed once it is whole.
    /// Method: offer every prefix of a capture and require `None` until the
    /// last byte, then the exact length. The receive loop accumulates across
    /// read timeouts, so a timeout landing mid-message must not be a case.
    #[test]
    fn a_partial_message_is_not_a_message_yet() {
        let whole = vectors::PROPERTIES_CHANGED;
        for short in 0..whole.len() {
            assert_eq!(
                frame_len(&whole[..short]),
                Ok(None),
                "{short} bytes is not yet a whole message"
            );
        }
        assert_eq!(frame_len(whole), Ok(Some(whole.len())));

        // A second message behind the first is measured, not swallowed - and
        // `parse` takes one frame and nothing else, in either direction.
        let mut two = whole.to_vec();
        two.extend_from_slice(vectors::SEEKED_SIGNAL);
        assert_eq!(frame_len(&two), Ok(Some(whole.len())));
        assert_eq!(
            frame_len(&two[whole.len()..]),
            Ok(Some(vectors::SEEKED_SIGNAL.len()))
        );
        // Both of these are the caller handing over the wrong slice rather than
        // a peer sending something wrong, so both are framing errors: they say
        // the stream is not where it was thought to be, which is what decides
        // whether the connection can carry on.
        assert!(
            matches!(parse(&two), Err(WireError::Framing(_))),
            "two messages are not one message"
        );
        assert!(
            matches!(parse(&whole[..whole.len() - 1]), Err(WireError::Framing(_))),
            "most of a message is not a message"
        );
    }

    /// Goal: a body that stops short of its own declared length is refused.
    /// Method: pad a real `s` body out with eight bytes the signature does not
    /// account for. Left alone, those bytes are whatever the next value would
    /// have been read as.
    #[test]
    fn a_body_shorter_than_its_declared_length_is_refused() {
        let mut padded = vectors::GETALL_CALL.to_vec();
        let body_len = u32::from_le_bytes([padded[4], padded[5], padded[6], padded[7]]);
        padded[4..8].copy_from_slice(&(body_len + 8).to_le_bytes());
        padded.extend_from_slice(&[0; 8]);
        assert_eq!(frame_len(&padded), Ok(Some(padded.len())));
        assert!(parse(&padded).is_err());
    }

    /// Goal: a variant carries exactly one type, and a signature naming two is
    /// refused rather than half-read. Method: the tokeniser directly, since a
    /// variant that read only its first type would leave the rest of the body
    /// to be read as something else entirely.
    #[test]
    fn a_variant_declaring_more_than_one_type_is_refused() {
        assert_eq!(one_kind("x"), Ok(Kind::Int64));
        assert_eq!(one_kind("a{sv}"), Ok(Kind::Dict));
        assert!(one_kind("").is_err(), "a variant with no type");
        assert!(one_kind("xx").is_err(), "a variant with two types");
        assert!(one_kind("sa{sv}").is_err(), "a variant with two types");
        assert!(one_kind("ao").is_err(), "a variant outside the closed set");
    }

    /// Goal: a header that is not one costs the connection, and a message that
    /// is merely wrong costs one message. Method: one corruption per rule,
    /// checking which half of [`WireError`] comes back - the receive loop
    /// branches on exactly that.
    #[test]
    fn a_broken_header_and_a_broken_message_are_told_apart() {
        let mut wrong_order = vectors::SEEKED_SIGNAL.to_vec();
        wrong_order[0] = b'x';
        assert!(matches!(
            frame_len(&wrong_order),
            Err(WireError::Framing(_))
        ));

        let mut wrong_version = vectors::SEEKED_SIGNAL.to_vec();
        wrong_version[3] = 2;
        assert!(matches!(
            frame_len(&wrong_version),
            Err(WireError::Framing(_))
        ));

        // A field array claiming more than an array may hold.
        let mut absurd_fields = vectors::SEEKED_SIGNAL.to_vec();
        absurd_fields[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            frame_len(&absurd_fields),
            Err(WireError::Framing(_))
        ));

        // A body claiming more than a message may hold.
        let mut absurd_body = vectors::SEEKED_SIGNAL.to_vec();
        absurd_body[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            frame_len(&absurd_body),
            Err(WireError::Framing(_))
        ));

        // The type byte is the message's problem, not the stream's.
        let mut unknown_type = vectors::SEEKED_SIGNAL.to_vec();
        unknown_type[1] = 9;
        assert!(matches!(parse(&unknown_type), Err(WireError::Message(_))));
    }

    /// Goal: a body that does not match its own signature is refused rather
    /// than misread. Method: corrupt one rule at a time in a real message and
    /// require an error each time. Silence is the failure mode being designed
    /// against here: the specification's answer to a malformed message is to
    /// hang up without a word, so anything read at the wrong offset would
    /// simply take the bus away.
    #[test]
    fn a_body_that_contradicts_its_signature_is_refused() {
        // A real `x` body, with the signature relabelled as something wider.
        let seeked = vectors::SEEKED_SIGNAL;
        assert!(parse(seeked).is_ok());

        let mut wrong_signature = seeked.to_vec();
        let signature_at = wrong_signature
            .windows(4)
            .position(|window| window == [0x08, 0x01, 0x67, 0x00])
            .expect("the SIGNATURE field is in the capture")
            + 5;
        wrong_signature[signature_at] = b'o';
        assert!(parse(&wrong_signature).is_err(), "an `x` read as an `o`");

        // A signature naming a container priel deliberately does not implement.
        let mut out_of_scope = seeked.to_vec();
        out_of_scope[signature_at - 1] = 2;
        out_of_scope[signature_at] = b'a';
        out_of_scope[signature_at + 1] = b'o';
        assert!(parse(&out_of_scope).is_err(), "an `ao` body");

        // Padding that is not nul: the four bytes an empty `a{sv}` writes after
        // its length and does not count. A reader that steps over them blindly
        // reads the same message; one that checks them says so.
        let mut dirty_padding = vectors::PROPERTIES_CHANGED_PADDED_DICT.to_vec();
        let body_len = usize::from(dirty_padding[4]);
        let body_at = dirty_padding.len() - body_len;
        dirty_padding[body_at + 36] = 0x41;
        assert!(parse(&dirty_padding).is_err(), "padding that is not nul");

        // A string that is not terminated.
        let mut unterminated = vectors::PROPERTIES_CHANGED_PADDED_DICT.to_vec();
        let last = unterminated.len() - 1;
        unterminated[last] = 0x41;
        assert!(parse(&unterminated).is_err(), "a string without its nul");
    }

    /// Goal: the values inside a body are checked, not merely stepped over.
    /// Method: build each malformed value with the marshaller's own layout and
    /// require the reader to refuse it. These are the ones a lenient parser
    /// waves through and hands upwards as a wrong answer.
    #[test]
    fn values_that_are_not_their_own_type_are_refused() {
        let make = |body: Vec<Arg>, edit: fn(&mut Vec<u8>)| {
            let message = Message::signal(1, "/priel", "org.priel.Wire", "Sample").with_body(body);
            let mut bytes = marshal(&message, Endian::Little).expect("the sample marshals");
            edit(&mut bytes);
            parse(&bytes)
        };

        // A boolean that is neither 0 nor 1.
        assert!(
            make(vec![Arg::Value(Value::Bool(true))], |bytes| {
                let last = bytes.len() - 4;
                bytes[last] = 2;
            })
            .is_err()
        );

        // A string whose bytes are not utf-8.
        assert!(
            make(vec![Arg::Value(str_value("ok"))], |bytes| {
                let last = bytes.len() - 3;
                bytes[last] = 0xff;
            })
            .is_err()
        );

        // An object path that is not one.
        assert!(
            make(
                vec![Arg::Value(Value::Path("/priel/track".into()))],
                |bytes| {
                    let last = bytes.len() - 2;
                    bytes[last] = b'-';
                }
            )
            .is_err()
        );

        // An array claiming more bytes than the message holds.
        assert!(
            make(vec![Arg::Value(strings(&["one"]))], |bytes| {
                let at = bytes.len() - 12;
                bytes[at..at + 4].copy_from_slice(&1000u32.to_le_bytes());
            })
            .is_err()
        );
    }

    /// Replace the four prologue bytes of a header field - its code, and the
    /// one-character signature that follows - in a capture.
    fn retype_field(bytes: &[u8], from: [u8; 4], to: [u8; 4]) -> Vec<u8> {
        let mut out = bytes.to_vec();
        let at = out
            .windows(4)
            .position(|window| window == from)
            .expect("the field is in the capture");
        out[at..at + 4].copy_from_slice(&to);
        out
    }

    /// Goal: a header field priel does not know is stepped over rather than
    /// refused, which spec 0.43 requires of every receiver, and a field whose
    /// type is not the one it is defined as is refused rather than read.
    /// Method: rewrite one field's code and type in real captures.
    ///
    /// The ignored field does not come back out again, so such a message is the
    /// one case that does not re-marshal to the bytes it arrived as. No
    /// implementation on this machine's bus sends one.
    #[test]
    fn a_header_field_priel_does_not_know_is_stepped_over() {
        // SENDER, renumbered to a code nobody has defined. Still a string, so
        // its length is known and it can be skipped.
        let unknown = retype_field(vectors::SEEKED_SIGNAL, [7, 1, b's', 0], [20, 1, b's', 0]);
        let message = parse(&unknown).expect("an unknown field is not an error");
        assert_eq!(message.sender(), None);
        assert_eq!(message.member(), Some("Seeked"));

        // The same code carrying a type that cannot be stepped over at all.
        let unsteppable = retype_field(vectors::SEEKED_SIGNAL, [7, 1, b's', 0], [20, 1, b'x', 0]);
        assert!(parse(&unsteppable).is_err());

        // A field priel does know, carrying the wrong type. Reading a `u` where
        // the specification says `s` is how a reply gets correlated to the
        // wrong call.
        let mistyped = retype_field(vectors::SEEK_CALL, [9, 1, b'u', 0], [9, 1, b's', 0]);
        assert!(parse(&mistyped).is_err());
    }

    /// Goal: a variant nested deeper than MPRIS defines is refused, not
    /// followed. Method: relabel a variant's own signature in two real
    /// captures - one at the top of a body, one inside `Metadata` - leaving
    /// every length intact so that only the type has changed.
    #[test]
    fn a_variant_nested_deeper_than_metadata_is_refused() {
        let mut inside_a_variant = vectors::GET_POSITION_REPLY.to_vec();
        // The body is the variant's own signature and then its `x`.
        let body_at = inside_a_variant.len() - usize::from(inside_a_variant[4]);
        assert_eq!(inside_a_variant[body_at + 1], b'x');
        inside_a_variant[body_at + 1] = b'v';
        assert!(parse(&inside_a_variant).is_err(), "a `v` inside a `v`");

        let mut inside_metadata = vectors::PROPERTIES_CHANGED.to_vec();
        let at = inside_metadata
            .windows(12)
            .position(|window| window.starts_with(b"xesam:title\0"))
            .expect("the title is in the capture")
            + 13;
        assert_eq!(inside_metadata[at], b's');
        inside_metadata[at] = b'v';
        assert!(parse(&inside_metadata).is_err(), "a `v` inside `Metadata`");
    }

    /// Goal: a wire error says which half it is when it is written down, since
    /// that is the difference between a lost connection and a lost message.
    #[test]
    fn a_wire_error_says_which_half_it_is() {
        assert_eq!(WireError::Framing("no").to_string(), "bus framing: no");
        assert_eq!(bad("no").to_string(), "bus message: no");
    }

    /// Goal: a message missing a field its type requires is refused once, here,
    /// rather than by whatever reads it later. Method: drop the required field
    /// from a real call and a real reply.
    #[test]
    fn a_message_missing_a_required_field_is_refused() {
        let mut call = parse(vectors::GETALL_CALL).expect("the call parses");
        call.fields
            .retain(|field| !matches!(field, Field::Member(_)));
        let bytes = marshal(&call, Endian::Little).expect("it still marshals");
        assert!(parse(&bytes).is_err(), "a method call with no member");

        let mut reply = parse(vectors::PING_REPLY).expect("the reply parses");
        reply
            .fields
            .retain(|field| !matches!(field, Field::ReplySerial(_)));
        let bytes = marshal(&reply, Endian::Little).expect("it still marshals");
        assert!(parse(&bytes).is_err(), "a reply answering nothing");
    }

    /// Goal: no corruption of a real message makes the parser panic. Method:
    /// sweep every byte of every capture, set it to three values that are not
    /// what was there, and require a verdict rather than an unwind. Anything
    /// the parser reads is a peer's bytes, so a panic here is a crash any
    /// process on the bus could cause.
    #[test]
    fn no_corruption_of_a_real_message_can_panic() {
        for (name, bytes) in captures() {
            for at in 0..bytes.len() {
                for replacement in [0x00, 0x01, 0xff] {
                    let mut broken = bytes.to_vec();
                    if broken[at] == replacement {
                        continue;
                    }
                    broken[at] = replacement;
                    // Only that it returns. Most of these are still valid
                    // messages saying something different.
                    let _ = frame_len(&broken);
                    let _ = parse(&broken);
                    assert!(!name.is_empty());
                }
            }
            // And every truncation, which is what a short read looks like.
            for short in 0..bytes.len() {
                let _ = frame_len(&bytes[..short]);
                let _ = parse(&bytes[..short]);
            }
        }
    }

    /// Goal: the body and the signature that describes it are set together, so
    /// they cannot drift. Method: set a body twice and check only one SIGNATURE
    /// field survives, and that an empty body writes none.
    #[test]
    fn setting_a_body_sets_the_signature_that_describes_it() {
        let message = Message::signal(1, "/priel", "org.priel.Wire", "Sample")
            .with_body(vec![Arg::Value(Value::Int64(1))])
            .with_body(vec![Arg::Value(str_value("a")), Arg::Dict(Vec::new())]);
        assert_eq!(message.signature(), "sa{sv}");
        assert_eq!(
            message
                .fields
                .iter()
                .filter(|field| matches!(field, Field::Signature(_)))
                .count(),
            1
        );

        let emptied = message.with_body(Vec::new());
        assert_eq!(emptied.signature(), "");
        assert!(
            !emptied
                .fields
                .iter()
                .any(|field| matches!(field, Field::Signature(_)))
        );
    }
}
