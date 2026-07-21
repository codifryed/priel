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

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;

use super::wire::{
    Arg, Endian, Message, MessageType, Serials, Value, WireError, frame_len, marshal, parse,
};

/// The bus's own name, path and interface. Every call in this module is to
/// these three, because the bus is the only peer priel talks to unprompted.
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

/// Spec 0.43 `DBUS_NAME_FLAG_DO_NOT_QUEUE`.
///
/// priel wants the name now or wants to be told it cannot have it. Standing in
/// the queue would mean the name arriving whenever the current owner exits,
/// with nothing watching for the signal that says so.
const NAME_FLAG_DO_NOT_QUEUE: u32 = 0x4;

/// The most reads one reply may take. At the socket's read timeout this bounds
/// a bus that accepted a call and then went quiet.
const MAX_REPLY_READS: usize = 100;

/// How much of one read is taken at a time. A message priel is sent is a few
/// hundred bytes, so this is a ceiling rather than a size to tune.
const READ_CHUNK: usize = 4096;

/// The most unread bytes priel will hold for a peer.
///
/// Spec 0.43 allows a message of 2^27 bytes, and nothing on the interfaces
/// priel implements is within four orders of magnitude of that: the longest
/// call it is sent is a `Set` carrying a string. The cap is what stops a peer -
/// hostile, or merely broken - from making priel allocate without limit, and a
/// frame that will not fit under it costs the connection, because there is no
/// way to skip past a message whose end has not arrived.
const MAX_INBOX: usize = 65_536;

/// The most bytes one line of the handshake may take. priel's own are under
/// forty; the cap is what stops a peer that never sends a terminator.
const MAX_AUTH_LINE: usize = 1024;

/// The most reads one line of the handshake may take. At the socket's read
/// timeout this bounds a peer that dribbles a byte at a time or goes silent.
const MAX_AUTH_READS: usize = 100;

/// The most lines the whole handshake may take. EXTERNAL settles in one, or in
/// two when the bus asks for the identity a second time.
const MAX_AUTH_LINES: usize = 8;

/// Why priel is not on the bus.
///
/// Every one of these leaves a working player. The split is by what the caller
/// can say about it, not by what it can do: they all end the same way.
#[derive(Debug)]
pub(crate) enum BusError {
    /// The far end said no, or said something that is not the protocol.
    Refused(String),
    /// The stream itself failed.
    Io(io::Error),
    /// The bytes are not a message priel can go on reading past. Only
    /// [`WireError::Framing`] reaches here - a single malformed message is
    /// skipped and never costs the connection.
    Wire(WireError),
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(what) => write!(f, "{what}"),
            Self::Io(error) => write!(f, "the bus socket failed: {error}"),
            Self::Wire(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BusError {}

impl From<io::Error> for BusError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<WireError> for BusError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

type Result<T> = std::result::Result<T, BusError>;

fn refused(what: impl Into<String>) -> BusError {
    BusError::Refused(what.into())
}

/// What one read off the socket produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Bytes arrived, which may still be less than a whole message.
    Bytes,
    /// The read timeout expired with nothing on the socket. Not an event: the
    /// buffer keeps what it has and the caller goes round again, which is what
    /// lets one thread serve both directions without a self-pipe.
    Idle,
    /// The peer hung up.
    Closed,
}

/// What the bus said about a name priel asked for.
///
/// `Taken` is not an error: spec 0.43 provides a second spelling for exactly
/// this case, and a player that is not on the bus is still a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameOutcome {
    Acquired,
    Taken,
}

/// A connection to the session bus.
///
/// Generic over the stream and never over a socket, so a scripted in-memory
/// duplex replays a whole session - the same seam `Client::with_base_url` and
/// `EventSource` already are. Only [`connect`] names a real socket.
pub(crate) struct Connection<S> {
    stream: S,
    /// Bytes read but not yet consumed. Holds at most [`MAX_INBOX`], and
    /// accumulates across read timeouts so a frame arriving in pieces is not a
    /// special case.
    inbox: Vec<u8>,
    serials: Serials,
    /// What the bus called priel when it answered `Hello`. Empty until then.
    unique_name: String,
    machine_id: Option<String>,
}

impl<S: Read + Write> Connection<S> {
    /// Wraps a stream that is already open. `machine_id` is what
    /// `org.freedesktop.DBus.Peer.GetMachineId` answers with; `None` answers
    /// that call with an error rather than inventing one.
    pub(crate) fn new(stream: S, machine_id: Option<String>) -> Self {
        Self {
            stream,
            inbox: Vec::new(),
            serials: Serials::default(),
            unique_name: String::new(),
            machine_id,
        }
    }

    /// The SASL EXTERNAL handshake: a leading nul, the identity, then `BEGIN`.
    ///
    /// `uid` is this process's user id, which EXTERNAL proves possession of.
    /// `None` sends the identity-less form, which asks the bus to read the
    /// credentials off the socket instead - both are answered `OK` by
    /// `dbus-broker`, the second after one further round trip.
    ///
    /// A bus that will not have EXTERNAL is declined here and now. priel speaks
    /// no other mechanism, so waiting to see whether it changes its mind is a
    /// hang rather than a retry.
    pub(crate) fn authenticate(&mut self, uid: Option<u32>) -> Result<()> {
        // The nul carries no information and is not part of the line: it is
        // there so the kernel has a byte to attach the sender's credentials to.
        let mut greeting = vec![0];
        greeting.extend_from_slice(b"AUTH EXTERNAL");
        if let Some(uid) = uid {
            greeting.push(b' ');
            greeting.extend_from_slice(hex(uid.to_string().as_bytes()).as_bytes());
        }
        greeting.extend_from_slice(b"\r\n");
        self.write(&greeting)?;

        for _ in 0..MAX_AUTH_LINES {
            let line = self.read_line()?;
            let (keyword, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match keyword {
                "OK" => return self.write(b"BEGIN\r\n"),
                // The bus wants the identity again, which is the second half of
                // the identity-less form: an empty response asks it to take the
                // credentials it already has off the socket.
                "DATA" => self.write(b"DATA\r\n")?,
                "REJECTED" => {
                    return Err(refused(format!(
                        "the bus offers only {rest}, and priel speaks EXTERNAL"
                    )));
                }
                "ERROR" => return Err(refused(format!("the bus refused the handshake: {rest}"))),
                other => {
                    return Err(refused(format!(
                        "the bus answered \"{other}\", which is not part of the handshake"
                    )));
                }
            }
        }
        Err(refused("the handshake did not settle"))
    }

    /// One `\r\n`-terminated line of the handshake.
    ///
    /// Reads through the inbox rather than off the socket a byte at a time, so
    /// whatever arrived past the terminator is still there for the next line -
    /// and, once `BEGIN` is sent, for the first message.
    fn read_line(&mut self) -> Result<String> {
        for _ in 0..MAX_AUTH_READS {
            if let Some(at) = self.inbox.windows(2).position(|pair| pair == b"\r\n") {
                let mut line: Vec<u8> = self.inbox.drain(..at + 2).collect();
                line.truncate(at);
                return String::from_utf8(line)
                    .map_err(|_| refused("the bus sent a handshake line that is not text"));
            }
            if self.inbox.len() > MAX_AUTH_LINE {
                return Err(refused("the bus sent a handshake line longer than any"));
            }
            if self.fill()? == Fill::Closed {
                return Err(refused("the bus closed the connection during the handshake"));
            }
        }
        Err(refused("the bus did not finish a line of the handshake"))
    }

    /// The name the bus gave this connection, which is empty until
    /// [`Connection::hello`] has been answered.
    pub(crate) fn unique_name(&self) -> &str {
        &self.unique_name
    }

    /// `Hello`, which is the first message on every connection and the one that
    /// makes the bus willing to route any other.
    pub(crate) fn hello(&mut self) -> Result<()> {
        let reply = self.call(Self::to_bus("Hello"))?;
        match reply.body.first() {
            Some(Arg::Value(Value::Str(name))) => {
                self.unique_name.clone_from(name);
                Ok(())
            }
            _ => Err(refused("the bus answered Hello with something but a name")),
        }
    }

    /// Ask the bus for a well-known name.
    pub(crate) fn request_name(&mut self, name: &str) -> Result<NameOutcome> {
        let call = Self::to_bus("RequestName").with_body(vec![
            Arg::Value(Value::Str(name.to_owned())),
            Arg::Value(Value::Uint32(NAME_FLAG_DO_NOT_QUEUE)),
        ]);
        let reply = self.call(call)?;
        match reply.body.first() {
            // Spec 0.43: 1 is primary owner and 4 is already the owner, which
            // both leave priel holding the name.
            Some(Arg::Value(Value::Uint32(1 | 4))) => Ok(NameOutcome::Acquired),
            // 3 is someone else's; 2 is a place in the queue, which
            // `NAME_FLAG_DO_NOT_QUEUE` rules out and which would be no use
            // anyway - both leave priel without the name.
            Some(Arg::Value(Value::Uint32(2 | 3))) => Ok(NameOutcome::Taken),
            _ => Err(refused(
                "the bus answered RequestName with something but a code",
            )),
        }
    }

    /// The well-known name priel ended up with, or `None` if it ended up with
    /// none - which is a working player that is not on the bus, never a
    /// failure.
    ///
    /// Two attempts and no loop: the name, then the per-instance spelling spec
    /// 0.43 provides for exactly this. That suffix adds no further dot, because
    /// playerctl derives a player's name by splitting on the first one and
    /// `playerctl -p priel` has to match both spellings.
    pub(crate) fn acquire_name(&mut self, base: &str) -> Result<Option<String>> {
        if self.request_name(base)? == NameOutcome::Acquired {
            return Ok(Some(base.to_owned()));
        }
        let instance = format!("{base}.instance{}", std::process::id());
        log::info!("the bus name {base} is already owned, so priel asks for {instance} instead");
        if self.request_name(&instance)? == NameOutcome::Acquired {
            return Ok(Some(instance));
        }
        Ok(None)
    }

    /// A call to the bus's own object, which is the only peer this module
    /// addresses. The serial is minted by [`Connection::call`].
    fn to_bus(member: &str) -> Message {
        Message::method_call(0, BUS_NAME, BUS_PATH, BUS_NAME, member)
    }

    /// Send a call and wait for the reply that answers it.
    ///
    /// The serial is minted here, and the reply is matched on it rather than on
    /// arrival order: the bus is entitled to put a signal in between, and a
    /// reply correlated by order is a reply to the wrong call.
    fn call(&mut self, mut message: Message) -> Result<Message> {
        let serial = self.serials.mint();
        message.serial = serial;
        let member = message.member().unwrap_or_default().to_owned();
        let bytes = marshal(&message, Endian::NATIVE)?;
        self.write(&bytes)?;

        for _ in 0..MAX_REPLY_READS {
            while let Some(reply) = self.take_message()? {
                if reply.reply_serial() != Some(serial) {
                    continue;
                }
                return match reply.kind {
                    MessageType::MethodReturn => Ok(reply),
                    MessageType::Error => Err(refused(format!(
                        "the bus answered {member} with {}",
                        error_text(&reply)
                    ))),
                    _ => Err(refused(format!(
                        "the bus answered {member} with something that is not a reply"
                    ))),
                };
            }
            if self.fill()? == Fill::Closed {
                return Err(refused(format!(
                    "the bus closed the connection without answering {member}"
                )));
            }
        }
        Err(refused(format!("the bus never answered {member}")))
    }

    /// The next whole message the inbox holds.
    ///
    /// A frame that is well-framed but malformed costs itself and nothing more:
    /// [`crate::bus::wire::frame_len`] has already said how long it is, so the
    /// bytes are dropped and the connection carries on. That split is the whole
    /// reason [`WireError`] has two variants, and honouring it is what stops one
    /// peer's bad message from taking priel off the bus.
    fn take_message(&mut self) -> Result<Option<Message>> {
        while let Some(frame) = self.take_frame()? {
            match parse(&frame) {
                Ok(message) => return Ok(Some(message)),
                Err(WireError::Message(what)) => {
                    log::warn!("the bus sent a message priel skipped: {what}");
                }
                Err(error) => return Err(BusError::Wire(error)),
            }
        }
        Ok(None)
    }

    /// The bytes of the next whole frame, or `None` while the inbox holds less
    /// than one - which is the ordinary answer between reads.
    fn take_frame(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(total) = frame_len(&self.inbox)? else {
            return Ok(None);
        };
        Ok(Some(self.inbox.drain(..total).collect()))
    }

    /// One read off the socket into the inbox.
    fn fill(&mut self) -> Result<Fill> {
        if self.inbox.len() >= MAX_INBOX {
            return Err(BusError::Wire(WireError::Framing(
                "a message larger than priel accepts",
            )));
        }
        let mut chunk = [0u8; READ_CHUNK];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(Fill::Closed),
            Ok(count) => {
                self.inbox.extend_from_slice(&chunk[..count]);
                Ok(Fill::Bytes)
            }
            Err(error) if is_idle(&error) => Ok(Fill::Idle),
            Err(error) => Err(BusError::Io(error)),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()?;
        Ok(())
    }
}

/// A read that produced nothing but is not a failure.
///
/// The read timeout is what makes one thread able to serve both directions, so
/// its expiry is the ordinary case and not an error. `Interrupted` is here for
/// the same reason: a signal landing mid-read costs nothing.
fn is_idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

/// An error reply, spelled for the log.
///
/// The name is what a reader can act on and the text is only ever displayed, so
/// the name comes first and is never left out.
fn error_text(reply: &Message) -> String {
    let name = reply.error_name().unwrap_or("an error with no name");
    match reply.body.first() {
        Some(Arg::Value(Value::Str(text))) => format!("{name}: {text}"),
        _ => name.to_owned(),
    }
}

/// The lowercase hex an auth line carries its payload as.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Both indices are masked to a nibble, so neither can be out of range.
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

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
    use std::collections::VecDeque;

    use super::*;
    use crate::bus::wire::{Field, NO_REPLY_EXPECTED, frame_len};

    /// What the far end does next.
    #[derive(Debug, Clone)]
    enum Turn {
        /// Bytes arriving. A turn is not a message: a frame may be split across
        /// several and several may share one.
        Bytes(Vec<u8>),
        /// A read that reached its timeout with nothing on the socket.
        Idle,
    }

    /// A scripted session in memory.
    ///
    /// The connection is written against `Read + Write` precisely so this can
    /// stand in for the socket, which is what keeps everything but the one line
    /// that opens a real one testable with no bus present.
    struct Duplex {
        turns: VecDeque<Turn>,
        /// The remainder of the turn a short read left behind.
        pending: Vec<u8>,
        written: Vec<u8>,
    }

    impl Duplex {
        fn new(turns: Vec<Turn>) -> Self {
            Self {
                turns: turns.into(),
                pending: Vec::new(),
                written: Vec::new(),
            }
        }

        fn text(bytes: &[u8]) -> Self {
            Self::new(vec![Turn::Bytes(bytes.to_vec())])
        }
    }

    impl Read for Duplex {
        /// A script that has run out reads as a peer that hung up, so no test
        /// can loop forever waiting for a turn that is not coming.
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pending.is_empty() {
                match self.turns.pop_front() {
                    Some(Turn::Bytes(bytes)) => self.pending = bytes,
                    Some(Turn::Idle) => return Err(io::Error::from(ErrorKind::WouldBlock)),
                    None => return Ok(0),
                }
            }
            let count = buf.len().min(self.pending.len());
            buf[..count].copy_from_slice(&self.pending[..count]);
            self.pending.drain(..count);
            Ok(count)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The line a real `dbus-broker` answers a good identity with.
    const OK_LINE: &[u8] = b"OK cc77a0b34c4180aa095b63544170c683\r\n";

    /// The bytes of a reply from the bus, the way the bus spells one: the
    /// serial it chose, the serial it is answering, and its own name.
    fn bus_reply(serial: u32, answering: u32, body: Vec<Arg>) -> Vec<u8> {
        let reply = Message {
            kind: MessageType::MethodReturn,
            flags: NO_REPLY_EXPECTED,
            serial,
            fields: vec![
                Field::ReplySerial(answering),
                Field::Sender(BUS_NAME.to_owned()),
                Field::Destination(":1.42".to_owned()),
            ],
            body: Vec::new(),
        }
        .with_body(body);
        marshal(&reply, Endian::NATIVE).expect("a reply the tests built")
    }

    /// The bytes of an error from the bus.
    fn bus_error(serial: u32, answering: u32, name: &str, text: &str) -> Vec<u8> {
        let error = Message {
            kind: MessageType::Error,
            flags: NO_REPLY_EXPECTED,
            serial,
            fields: vec![
                Field::ReplySerial(answering),
                Field::ErrorName(name.to_owned()),
                Field::Sender(BUS_NAME.to_owned()),
            ],
            body: Vec::new(),
        }
        .with_body(vec![Arg::Value(Value::Str(text.to_owned()))]);
        marshal(&error, Endian::NATIVE).expect("an error the tests built")
    }

    /// A connection that has already authenticated, so a test scripts what
    /// comes after the handshake and nothing else.
    fn greeted(duplex: &mut Duplex) -> Connection<&mut Duplex> {
        let mut conn = Connection::new(duplex, None);
        conn.authenticate(Some(1000))
            .expect("the scripted handshake");
        conn
    }

    /// A script that begins with the handshake the bus always answers.
    fn after_handshake(turns: Vec<Turn>) -> Duplex {
        let mut all = vec![Turn::Bytes(OK_LINE.to_vec())];
        all.extend(turns);
        Duplex::new(all)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// Every message priel wrote, once the handshake's own lines are past.
    fn sent(duplex: &Duplex) -> Vec<Message> {
        let begin = b"BEGIN\r\n";
        let at = find(&duplex.written, begin).map_or(0, |at| at + begin.len());
        let mut rest = &duplex.written[at..];
        let mut out = Vec::new();
        while let Some(total) = frame_len(rest).expect("priel writes well-framed messages") {
            out.push(parse(&rest[..total]).expect("priel writes messages it can read back"));
            rest = &rest[total..];
        }
        out
    }

    /// Goal: `Hello` goes to the bus's own object and the name that comes back
    /// is kept, because every later message is addressed from it. Method: the
    /// reply a real bus sends, and read the call back off the wire.
    #[test]
    fn hello_is_addressed_to_the_bus_and_its_answer_is_kept() {
        let mut duplex = after_handshake(vec![Turn::Bytes(bus_reply(
            1,
            1,
            vec![Arg::Value(Value::Str(":1.4242".into()))],
        ))]);
        {
            let mut conn = greeted(&mut duplex);
            conn.hello().expect("the scripted Hello");
            assert_eq!(conn.unique_name(), ":1.4242");
        }
        let calls = sent(&duplex);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].destination(), Some(BUS_NAME));
        assert_eq!(calls[0].path(), Some(BUS_PATH));
        assert_eq!(calls[0].interface(), Some(BUS_NAME));
        assert_eq!(calls[0].member(), Some("Hello"));
        assert_eq!(calls[0].serial, 1);
    }

    /// Goal: `RequestName` asks for the name with the flag that says priel will
    /// not queue for it. Method: read the body back off the wire - a queued
    /// request succeeds later, with nothing watching, which is worse than a
    /// clean no.
    #[test]
    fn request_name_asks_without_queueing() {
        let mut duplex = after_handshake(vec![Turn::Bytes(bus_reply(
            1,
            1,
            vec![Arg::Value(Value::Uint32(1))],
        ))]);
        {
            let mut conn = greeted(&mut duplex);
            let outcome = conn
                .request_name("org.mpris.MediaPlayer2.priel")
                .expect("the scripted RequestName");
            assert_eq!(outcome, NameOutcome::Acquired);
        }
        let calls = sent(&duplex);
        assert_eq!(calls[0].member(), Some("RequestName"));
        assert_eq!(calls[0].signature(), "su");
        assert_eq!(
            calls[0].body,
            vec![
                Arg::Value(Value::Str("org.mpris.MediaPlayer2.priel".into())),
                Arg::Value(Value::Uint32(NAME_FLAG_DO_NOT_QUEUE)),
            ]
        );
    }

    /// Goal: each code spec 0.43 defines is read as the answer it is. Method: a
    /// table - 1 and 4 both mean priel owns the name, and 2 and 3 both mean it
    /// does not.
    #[test]
    fn every_name_code_is_read_as_what_it_means() {
        for (code, expected) in [
            (1, NameOutcome::Acquired),
            (4, NameOutcome::Acquired),
            (2, NameOutcome::Taken),
            (3, NameOutcome::Taken),
        ] {
            let mut duplex = after_handshake(vec![Turn::Bytes(bus_reply(
                1,
                1,
                vec![Arg::Value(Value::Uint32(code))],
            ))]);
            let mut conn = greeted(&mut duplex);
            assert_eq!(
                conn.request_name("org.mpris.MediaPlayer2.priel").ok(),
                Some(expected),
                "code {code}"
            );
        }
    }

    /// Goal: a name someone else owns is retried once under the per-instance
    /// spelling spec 0.43 provides, and that spelling carries no further dot -
    /// playerctl derives the player's name by splitting on the first one, so
    /// `-p priel` has to match both. Method: refuse the first, grant the
    /// second, and read the name back off the wire.
    #[test]
    fn a_name_already_owned_falls_back_to_the_instance_spelling() {
        let mut duplex = after_handshake(vec![
            Turn::Bytes(bus_reply(1, 1, vec![Arg::Value(Value::Uint32(3))])),
            Turn::Bytes(bus_reply(2, 2, vec![Arg::Value(Value::Uint32(1))])),
        ]);
        let acquired = {
            let mut conn = greeted(&mut duplex);
            conn.acquire_name("org.mpris.MediaPlayer2.priel")
                .expect("the scripted names")
        };
        let expected = format!(
            "org.mpris.MediaPlayer2.priel.instance{}",
            std::process::id()
        );
        assert_eq!(acquired, Some(expected.clone()));
        assert_eq!(
            expected.strip_prefix("org.mpris.MediaPlayer2.").map(|tail| tail.split('.').next()),
            Some(Some("priel")),
            "playerctl splits on the first dot, so the suffix may not add one"
        );
        let calls = sent(&duplex);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].body.first(),
            Some(&Arg::Value(Value::Str(expected)))
        );
    }

    /// Goal: both spellings being owned leaves priel a working player rather
    /// than an error to report. Method: refuse both, and check the result is
    /// an absence and not a failure.
    #[test]
    fn both_spellings_owned_leaves_priel_off_the_bus() {
        let mut duplex = after_handshake(vec![
            Turn::Bytes(bus_reply(1, 1, vec![Arg::Value(Value::Uint32(3))])),
            Turn::Bytes(bus_reply(2, 2, vec![Arg::Value(Value::Uint32(3))])),
        ]);
        let mut conn = greeted(&mut duplex);
        assert_eq!(conn.acquire_name("org.mpris.MediaPlayer2.priel").ok(), Some(None));
    }

    /// Goal: an error reply is reported with the name the bus put on it, since
    /// that name is the only part of it a reader can act on. Method: answer
    /// `Hello` with the error a bus sends a connection that already said it.
    #[test]
    fn an_error_reply_is_reported_with_its_name() {
        let mut duplex = after_handshake(vec![Turn::Bytes(bus_error(
            1,
            1,
            "org.freedesktop.DBus.Error.Failed",
            "Already handled an Hello message",
        ))]);
        let mut conn = greeted(&mut duplex);
        let error = conn.hello().expect_err("an error reply");
        assert!(
            error.to_string().contains("org.freedesktop.DBus.Error.Failed"),
            "the log has to carry the error name: {error}"
        );
    }

    /// Goal: a reply is matched by the serial it answers and never by arrival
    /// order, so a message landing in between cannot be mistaken for the
    /// answer. Method: put a reply to a serial priel never sent in front of the
    /// real one.
    #[test]
    fn a_reply_is_matched_by_serial_and_not_by_order() {
        let mut duplex = after_handshake(vec![
            Turn::Bytes(bus_reply(9, 77, vec![Arg::Value(Value::Str(":1.9".into()))])),
            Turn::Bytes(bus_reply(1, 1, vec![Arg::Value(Value::Str(":1.4242".into()))])),
        ]);
        let mut conn = greeted(&mut duplex);
        conn.hello().expect("the scripted Hello");
        assert_eq!(conn.unique_name(), ":1.4242");
    }

    /// Goal: a bus that takes the call and then hangs up is reported rather
    /// than waited on. Method: nothing after the handshake.
    #[test]
    fn a_bus_that_hangs_up_before_answering_is_reported() {
        let mut duplex = after_handshake(Vec::new());
        let mut conn = greeted(&mut duplex);
        let error = conn.hello().expect_err("a bus that hung up");
        assert!(
            error.to_string().contains("closed"),
            "the log has to say the bus hung up: {error}"
        );
    }

    /// Goal: a bus that never answers is given up on rather than waited on
    /// forever. Method: nothing but read timeouts, past the bound.
    #[test]
    fn a_bus_that_never_answers_a_call_is_given_up_on() {
        let mut duplex = after_handshake(vec![Turn::Idle; MAX_REPLY_READS * 2]);
        let mut conn = greeted(&mut duplex);
        assert!(conn.hello().is_err());
    }

    /// Goal: an answer that is not the shape the specification names is
    /// refused, not read as something else. Method: answer `Hello` with a
    /// number and `RequestName` with a string.
    #[test]
    fn an_answer_of_the_wrong_shape_is_refused() {
        let mut duplex = after_handshake(vec![Turn::Bytes(bus_reply(
            1,
            1,
            vec![Arg::Value(Value::Uint32(7))],
        ))]);
        assert!(greeted(&mut duplex).hello().is_err());

        let mut duplex = after_handshake(vec![Turn::Bytes(bus_reply(
            1,
            1,
            vec![Arg::Value(Value::Str("yes".into()))],
        ))]);
        assert!(greeted(&mut duplex).request_name("org.a.b").is_err());
    }

    /// Goal: the handshake is the three things spec 0.43 requires, in order and
    /// with nothing in between. Method: replay the `OK` a real bus sends and
    /// compare the bytes priel wrote, byte for byte - the leading nul carries
    /// no keyword, so an implementation that dropped it would still look right
    /// in a log.
    #[test]
    fn the_handshake_is_a_nul_an_identity_and_begin() {
        let mut duplex = Duplex::text(OK_LINE);
        {
            let mut conn = Connection::new(&mut duplex, None);
            assert!(conn.authenticate(Some(1000)).is_ok());
        }
        assert_eq!(duplex.written, b"\0AUTH EXTERNAL 31303030\r\nBEGIN\r\n");
    }

    /// Goal: the identity is the *decimal* user id as text, hex-encoded, and
    /// not the number in hex. Method: uid 1000 is "31303030" and not "3e8".
    #[test]
    fn the_identity_is_the_decimal_user_id_encoded() {
        assert_eq!(hex(b"1000"), "31303030");
        assert_eq!(hex(b"0"), "30");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    /// Goal: a bus that will not have EXTERNAL is declined rather than waited
    /// on, because priel speaks no other mechanism and has nothing to retry
    /// with. Method: the `REJECTED` a real bus answers an unknown mechanism
    /// with - and check `BEGIN` was never sent.
    #[test]
    fn a_bus_that_rejects_external_is_declined_not_retried() {
        let mut duplex = Duplex::text(b"REJECTED DBUS_COOKIE_SHA1 ANONYMOUS\r\n");
        {
            let mut conn = Connection::new(&mut duplex, None);
            let error = conn.authenticate(Some(1000)).expect_err("EXTERNAL refused");
            assert!(
                error.to_string().contains("DBUS_COOKIE_SHA1"),
                "the log has to say which mechanisms were on offer: {error}"
            );
        }
        assert!(!duplex.written.ends_with(b"BEGIN\r\n"));
    }

    /// Goal: a bus that asks for the identity a second time is answered rather
    /// than abandoned. Method: `dbus-broker` answers the identity-less form
    /// with `DATA`, and `OK` once an empty `DATA` comes back.
    #[test]
    fn a_second_request_for_the_identity_is_answered_empty() {
        let mut duplex = Duplex::new(vec![
            Turn::Bytes(b"DATA\r\n".to_vec()),
            Turn::Bytes(OK_LINE.to_vec()),
        ]);
        {
            let mut conn = Connection::new(&mut duplex, None);
            assert!(conn.authenticate(None).is_ok());
        }
        assert_eq!(duplex.written, b"\0AUTH EXTERNAL\r\nDATA\r\nBEGIN\r\n");
    }

    /// Goal: an `ERROR` from the bus ends the handshake instead of being read
    /// as one more line to answer. Method: send it where `OK` belongs.
    #[test]
    fn an_error_from_the_bus_ends_the_handshake() {
        let mut duplex = Duplex::text(b"ERROR Not enough\r\n");
        let mut conn = Connection::new(&mut duplex, None);
        assert!(conn.authenticate(Some(1000)).is_err());
    }

    /// Goal: a peer that hangs up mid-handshake is a failure to be reported,
    /// not a line to wait forever for. Method: half of the `OK` line, then the
    /// socket closing.
    #[test]
    fn a_peer_that_hangs_up_mid_handshake_is_declined() {
        let mut duplex = Duplex::text(b"OK cc77a0b3");
        let mut conn = Connection::new(&mut duplex, None);
        let error = conn
            .authenticate(Some(1000))
            .expect_err("a truncated handshake");
        assert!(
            error.to_string().contains("closed"),
            "the log has to say the bus hung up: {error}"
        );
    }

    /// Goal: a peer that never terminates a line cannot make priel allocate
    /// without limit. Method: more than the cap, with no `\r\n` anywhere in it.
    #[test]
    fn an_auth_line_that_never_ends_is_declined() {
        let mut duplex = Duplex::text(&b"O".repeat(MAX_AUTH_LINE * 4));
        let mut conn = Connection::new(&mut duplex, None);
        assert!(conn.authenticate(Some(1000)).is_err());
    }

    /// Goal: a read timeout during the handshake is waited through, since the
    /// socket carries one and the bus is entitled to take a moment. Method: two
    /// timeouts before the `OK`.
    #[test]
    fn a_timeout_during_the_handshake_is_waited_through() {
        let mut duplex = Duplex::new(vec![
            Turn::Idle,
            Turn::Idle,
            Turn::Bytes(OK_LINE.to_vec()),
        ]);
        let mut conn = Connection::new(&mut duplex, None);
        assert!(conn.authenticate(Some(1000)).is_ok());
    }

    /// Goal: a bus that says nothing at all is given up on rather than waited
    /// on forever. Method: nothing but timeouts, past the read bound.
    #[test]
    fn a_bus_that_never_answers_is_given_up_on() {
        let mut duplex = Duplex::new(vec![Turn::Idle; MAX_AUTH_READS * 2]);
        let mut conn = Connection::new(&mut duplex, None);
        assert!(conn.authenticate(Some(1000)).is_err());
    }

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
