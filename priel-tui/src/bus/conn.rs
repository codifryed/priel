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
// before anything depends on them. Nothing calls `start` until they do, and
// with no caller at the top every item below it reads as dead.
#![allow(
    dead_code,
    reason = "the interfaces above it are the callers, and land next"
)]

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use super::wire::{
    Arg, Endian, Message, MessageType, NO_REPLY_EXPECTED, Serials, Value, WireError, frame_len,
    marshal, parse,
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

/// The environment variable that says where the session bus is. Its absence is
/// the ordinary state of a machine with no desktop on it.
const ADDRESS_VARIABLE: &str = "DBUS_SESSION_BUS_ADDRESS";

/// The interface every peer on the bus answers, whatever else it implements.
const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";

/// The errors priel sends. Both are spelled as `dbus-broker` spells them, which
/// is what `busctl` and `gdbus` print when this is being debugged.
const ERROR_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const ERROR_FAILED: &str = "org.freedesktop.DBus.Error.Failed";

/// The read timeout that lets one thread serve both directions.
///
/// A read finding nothing costs this long and then the outbound queue gets its
/// turn, which is the whole reason there is no self-pipe and no sixth thread.
/// It is the rate the player already ticks at.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// The most messages one turn of the loop will write before reading again. A
/// burst of signals may not starve the side that answers calls.
const MAX_OUTBOUND_PER_TURN: usize = 64;

/// The most of a machine-id file priel will read. The file is 33 bytes; the
/// bound is what stops a path that is not one from being read whole.
const MACHINE_ID_MAX: u64 = 64;

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
                return Err(refused(
                    "the bus closed the connection during the handshake",
                ));
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

    /// Serve the bus until the connection or the caller ends.
    ///
    /// One thread does both directions, which the socket's read timeout is what
    /// makes possible: a read that finds nothing costs [`READ_TIMEOUT`] and
    /// then the outbound queue gets its turn. The inbox accumulates across
    /// those timeouts and only whole frames are parsed, so a timeout landing
    /// mid-message is not a special case.
    ///
    /// Ends when the peer hangs up or when `outbox`'s sender is dropped, which
    /// is how the app takes the bus down with it on the way out.
    pub(crate) fn serve(&mut self, outbox: &Receiver<Message>) -> Result<()> {
        loop {
            self.answer_inbox()?;
            if !self.pump_outbox(outbox)? {
                return Ok(());
            }
            if self.fill()? == Fill::Closed {
                return Ok(());
            }
        }
    }

    /// Answer every call the inbox already holds.
    ///
    /// Bounded by the inbox, which is bounded in [`Connection::fill`]: every
    /// turn takes a whole frame out of it and no frame is shorter than a fixed
    /// header, so the loop cannot fail to make progress.
    fn answer_inbox(&mut self) -> Result<()> {
        while let Some(message) = self.take_message()? {
            if message.kind != MessageType::MethodCall {
                // The bus sends `NameAcquired` unprompted the moment a name is
                // taken, and replies to calls priel gave up waiting on can
                // still land. Answering either is a protocol error.
                continue;
            }
            if let Some(reply) = answer(&message, self.machine_id.as_deref()) {
                self.send(reply)?;
            }
        }
        Ok(())
    }

    /// Write the messages the app queued.
    ///
    /// Bounded per turn, so a burst of signals cannot starve the side that
    /// answers calls. `false` means the app has dropped its end and the
    /// connection goes with it.
    fn pump_outbox(&mut self, outbox: &Receiver<Message>) -> Result<bool> {
        for _ in 0..MAX_OUTBOUND_PER_TURN {
            match outbox.try_recv() {
                Ok(message) => self.send(message)?,
                Err(TryRecvError::Empty) => return Ok(true),
                Err(TryRecvError::Disconnected) => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Write one message, with the serial minted here.
    ///
    /// Fire-and-forget: nothing this sends has a reply to wait for. The serial
    /// is minted here rather than by the caller because the counter belongs to
    /// the connection, and two messages sharing a serial are two messages a
    /// reply cannot be matched to.
    pub(crate) fn send(&mut self, mut message: Message) -> Result<()> {
        message.serial = self.serials.mint();
        match marshal(&message, Endian::NATIVE) {
            Ok(bytes) => self.write(&bytes),
            // priel built this one, so this is priel's own bug rather than a
            // peer's, and it costs the message it is in rather than the
            // connection - the socket is still on a message boundary.
            Err(error) => {
                log::error!("priel dropped a message it could not marshal: {error}");
                Ok(())
            }
        }
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

/// priel's end of the session bus.
///
/// Fire-and-forget, the way `Player`'s command channel is: the app posts a
/// signal and never waits for it. Dropping this ends the dispatch loop within
/// one [`READ_TIMEOUT`], which is how the bus thread joins on the way out.
pub(crate) struct Bus {
    signals: mpsc::Sender<Message>,
    name: String,
    unique_name: String,
}

impl Bus {
    /// The well-known name priel is published under. Either the one asked for
    /// or its per-instance spelling.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The name the bus gave this connection, which is what a peer sees as the
    /// sender of everything priel emits.
    pub(crate) fn unique_name(&self) -> &str {
        &self.unique_name
    }

    /// Queue a signal for the bus thread to write.
    ///
    /// Never blocks and never fails upwards: a closed channel means the bus
    /// thread has gone, which costs the desktop a notification and the player
    /// nothing at all.
    pub(crate) fn emit(&self, signal: Message) {
        if self.signals.send(signal).is_err() {
            log::debug!("the session bus connection is gone, so a signal was dropped");
        }
    }
}

/// Put priel on the session bus under `well_known`, if there is a session bus.
///
/// `None` is the ordinary answer on a machine with no desktop, and the reason
/// is in the diagnostic log either way. Nothing here can fail the program.
#[cfg(unix)]
pub(crate) fn start(well_known: &str) -> Option<Bus> {
    let Ok(spec) = std::env::var(ADDRESS_VARIABLE) else {
        log::info!("no {ADDRESS_VARIABLE} in the environment, so priel is not on the session bus");
        return None;
    };
    let Some(address) = parse_address(&spec) else {
        log::warn!("no address priel can speak in {ADDRESS_VARIABLE}: {spec}");
        return None;
    };
    match connect(&address, well_known) {
        Ok(bus) => {
            log::info!(
                "priel is on the session bus as {} ({})",
                bus.name(),
                bus.unique_name()
            );
            Some(bus)
        }
        Err(error) => {
            log::warn!("priel is not on the session bus: {error}");
            None
        }
    }
}

/// Open, authenticate, name, and hand the connection to its own thread.
#[cfg(unix)]
fn connect(address: &Address, well_known: &str) -> Result<Bus> {
    let stream = open(address)?;
    // The one thing that makes one thread enough for both directions.
    stream.set_read_timeout(Some(READ_TIMEOUT))?;

    let mut connection = Connection::new(stream, machine_id());
    connection.authenticate(current_uid())?;
    connection.hello()?;
    let unique_name = connection.unique_name().to_owned();
    let Some(name) = connection.acquire_name(well_known)? else {
        return Err(refused(format!(
            "{well_known} and its per-instance spelling are both owned already"
        )));
    };

    let (signals, outbox) = mpsc::channel();
    std::thread::Builder::new()
        .name("bus".into())
        .spawn(move || {
            if let Err(error) = connection.serve(&outbox) {
                log::warn!("the session bus connection ended: {error}");
            }
        })
        .map_err(BusError::Io)?;
    Ok(Bus {
        signals,
        name,
        unique_name,
    })
}

/// The one line that names a real socket.
///
/// Everything above it is written against `Read + Write`, which is what makes a
/// whole session replayable in memory with no bus anywhere near the tests.
#[cfg(unix)]
fn open(address: &Address) -> Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    match address {
        Address::Path(path) => Ok(UnixStream::connect(path)?),
        #[cfg(target_os = "linux")]
        Address::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt as _;
            let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            Ok(UnixStream::connect_addr(&address)?)
        }
        #[cfg(not(target_os = "linux"))]
        Address::Abstract(_) => Err(refused(
            "the bus is on an abstract socket, which only Linux has",
        )),
    }
}

/// This process's user id, which EXTERNAL proves possession of.
///
/// Read off `/proc/self` rather than through a C binding: one number is not a
/// dependency's worth of reason, and `None` falls back to the identity-less
/// form, which asks the bus to take the credentials it already has off the
/// socket.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/proc/self")
        .ok()
        .map(|metadata| metadata.uid())
}

/// This machine's id, which `org.freedesktop.DBus.Peer.GetMachineId` answers
/// with.
///
/// The read is bounded because the path is not priel's to trust: the file is 33
/// bytes and anything longer is not one.
fn machine_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(MACHINE_ID_MAX).read_to_string(&mut text).is_err() {
            continue;
        }
        if let Some(id) = valid_machine_id(&text) {
            return Some(id);
        }
    }
    log::debug!("this machine has no readable id, so GetMachineId will answer with an error");
    None
}

/// Spec 0.43: a machine id is 32 lowercase hex digits and nothing else.
///
/// Checked rather than trusted, because whatever is at that path is answered
/// with verbatim and a consumer has no way to tell a bad one from a good one.
fn valid_machine_id(text: &str) -> Option<String> {
    let id = text.trim();
    let shaped = id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    shaped.then(|| id.to_owned())
}

/// The answer to one method call, or `None` where the caller asked for none.
///
/// Pure, in the shape `App::decide` already established: the loop mints the
/// serial and writes it. Every reply leaves here with serial 0, which
/// [`marshal`] refuses, so a path that forgot to mint one fails loudly rather
/// than sending a message no reply can be matched to.
///
/// priel answers only what the bus requires of every peer. Everything else gets
/// the error a real service gives an unknown member - and that fallback is what
/// the interfaces above will slot in ahead of.
fn answer(call: &Message, machine_id: Option<&str>) -> Option<Message> {
    if call.flags & NO_REPLY_EXPECTED != 0 {
        return None;
    }
    let member = call.member().unwrap_or_default();
    // The object path is deliberately not matched on: spec 0.43 puts `Peer` on
    // every path a connection exposes, and a `Ping` answered only at one is a
    // peer that looks hung to whatever probed it elsewhere.
    let reply = match (call.interface(), member) {
        (Some(PEER_INTERFACE), "Ping") => Message::method_return(0, call),
        (Some(PEER_INTERFACE), "GetMachineId") => match machine_id {
            Some(id) => Message::method_return(0, call)
                .with_body(vec![Arg::Value(Value::Str(id.to_owned()))]),
            // An invented id is worse than an error: a consumer would believe
            // it, and two machines answering the same one is what the id exists
            // to rule out.
            None => Message::error(
                0,
                call,
                ERROR_FAILED,
                "priel could not read this machine's id",
            ),
        },
        (interface, _) => Message::error(
            0,
            call,
            ERROR_UNKNOWN_METHOD,
            &format!(
                "priel implements no member \"{member}\" in interface \"{}\"",
                interface.unwrap_or("")
            ),
        ),
    };
    Some(reply)
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
            expected
                .strip_prefix("org.mpris.MediaPlayer2.")
                .map(|tail| tail.split('.').next()),
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
        assert_eq!(
            conn.acquire_name("org.mpris.MediaPlayer2.priel").ok(),
            Some(None)
        );
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
            error
                .to_string()
                .contains("org.freedesktop.DBus.Error.Failed"),
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
            Turn::Bytes(bus_reply(
                9,
                77,
                vec![Arg::Value(Value::Str(":1.9".into()))],
            )),
            Turn::Bytes(bus_reply(
                1,
                1,
                vec![Arg::Value(Value::Str(":1.4242".into()))],
            )),
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

    /// The bytes of a method call arriving from another peer.
    fn incoming(serial: u32, interface: Option<&str>, member: &str, flags: u8) -> Vec<u8> {
        let mut fields = vec![
            Field::Path("/org/mpris/MediaPlayer2".to_owned()),
            Field::Member(member.to_owned()),
            Field::Sender(":1.99".to_owned()),
            Field::Destination(":1.4242".to_owned()),
        ];
        if let Some(interface) = interface {
            fields.push(Field::Interface(interface.to_owned()));
        }
        let call = Message {
            kind: MessageType::MethodCall,
            flags,
            serial,
            fields,
            body: Vec::new(),
        };
        marshal(&call, Endian::NATIVE).expect("a call the tests built")
    }

    /// A `Ping` from another peer.
    fn ping(serial: u32) -> Vec<u8> {
        incoming(serial, Some(PEER_INTERFACE), "Ping", 0)
    }

    /// Run the dispatch loop over a scripted session with nothing to send.
    fn served(duplex: &mut Duplex, machine_id: Option<String>) -> Result<()> {
        // The sender is held for the whole loop, so the script is what ends it.
        let (signals, outbox) = mpsc::channel();
        let mut conn = Connection::new(duplex, machine_id);
        conn.authenticate(Some(1000))
            .expect("the scripted handshake");
        let result = conn.serve(&outbox);
        drop(signals);
        result
    }

    /// Goal: a whole session works end to end - handshake, name, and a call
    /// answered - because each piece passing on its own does not mean they
    /// compose. Method: replay the lot and read back every message priel wrote.
    #[test]
    fn a_whole_session_runs_from_handshake_to_answered_call() {
        let mut duplex = after_handshake(vec![
            Turn::Bytes(bus_reply(
                1,
                1,
                vec![Arg::Value(Value::Str(":1.4242".into()))],
            )),
            Turn::Bytes(bus_reply(2, 2, vec![Arg::Value(Value::Uint32(1))])),
            Turn::Bytes(ping(7)),
        ]);
        let (name, unique) = {
            let (signals, outbox) = mpsc::channel();
            let mut conn = Connection::new(&mut duplex, None);
            conn.authenticate(Some(1000)).expect("the handshake");
            conn.hello().expect("Hello");
            let name = conn
                .acquire_name("org.mpris.MediaPlayer2.priel")
                .expect("RequestName");
            conn.serve(&outbox).expect("a session that ends cleanly");
            drop(signals);
            (name, conn.unique_name().to_owned())
        };
        assert_eq!(name.as_deref(), Some("org.mpris.MediaPlayer2.priel"));
        assert_eq!(unique, ":1.4242");

        let written = sent(&duplex);
        assert_eq!(written.len(), 3);
        assert_eq!(written[0].member(), Some("Hello"));
        assert_eq!(written[1].member(), Some("RequestName"));
        assert_eq!(written[2].kind, MessageType::MethodReturn);
        assert_eq!(written[2].reply_serial(), Some(7));
        assert_eq!(written[2].destination(), Some(":1.99"));
        // Serial 3 follows the two calls, and no reply may reuse one.
        assert_eq!(written[2].serial, 3);
    }

    /// Goal: a frame arriving in pieces is answered once it is whole, which is
    /// what the inbox accumulating across reads buys. Method: one `Ping` cut
    /// into three, with the cuts inside the fixed header and inside the fields.
    #[test]
    fn a_frame_split_across_three_reads_is_answered_once_it_is_whole() {
        let frame = ping(7);
        let mut duplex = after_handshake(vec![
            Turn::Bytes(frame[..8].to_vec()),
            Turn::Bytes(frame[8..20].to_vec()),
            Turn::Bytes(frame[20..].to_vec()),
        ]);
        served(&mut duplex, None).expect("a session that ends cleanly");
        let written = sent(&duplex);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].reply_serial(), Some(7));
    }

    /// Goal: two frames sharing one read are both answered, since a read is not
    /// a message boundary. Method: concatenate two calls into one turn.
    #[test]
    fn two_frames_in_one_read_are_both_answered() {
        let mut both = ping(7);
        both.extend(ping(8));
        let mut duplex = after_handshake(vec![Turn::Bytes(both)]);
        served(&mut duplex, None).expect("a session that ends cleanly");
        let written = sent(&duplex);
        assert_eq!(written.len(), 2);
        assert_eq!(written[0].reply_serial(), Some(7));
        assert_eq!(written[1].reply_serial(), Some(8));
    }

    /// Goal: a frame that cannot be measured costs the connection, because
    /// nothing after it can be trusted to start on a message boundary. Method:
    /// a fixed header whose byte-order marker is neither `l` nor `B`, with a
    /// good call behind it that must never be answered.
    #[test]
    fn a_frame_that_cannot_be_measured_costs_the_connection() {
        let mut stream = b"xxxxxxxxxxxxxxxx".to_vec();
        stream.extend(ping(7));
        let mut duplex = after_handshake(vec![Turn::Bytes(stream)]);
        let error = served(&mut duplex, None).expect_err("a stream that cannot resynchronise");
        assert!(
            matches!(error, BusError::Wire(WireError::Framing(_))),
            "framing is what costs the connection: {error}"
        );
        assert!(sent(&duplex).is_empty());
    }

    /// Goal: a frame that is malformed but measurable costs one message and not
    /// the connection - that split is the whole reason `WireError` has two
    /// variants. Method: patch a good call's serial to zero, which spec 0.43
    /// forbids, and put a good call behind it that must still be answered.
    #[test]
    fn a_malformed_but_measurable_frame_costs_only_itself() {
        let mut broken = ping(7);
        // Bytes 8..12 are the serial, which leaves both declared lengths - and
        // so the frame's own length - untouched.
        broken[8..12].copy_from_slice(&0u32.to_ne_bytes());
        broken.extend(ping(8));
        let mut duplex = after_handshake(vec![Turn::Bytes(broken)]);
        served(&mut duplex, None).expect("a connection that survives one bad message");
        let written = sent(&duplex);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].reply_serial(), Some(8));
    }

    /// Goal: a read timeout is not an event - the loop simply goes round, which
    /// is what lets one thread serve both directions. Method: nothing but
    /// timeouts before the call, and check it is still answered.
    #[test]
    fn a_read_that_finds_nothing_just_goes_round_again() {
        let mut duplex = after_handshake(vec![
            Turn::Idle,
            Turn::Idle,
            Turn::Idle,
            Turn::Bytes(ping(7)),
        ]);
        served(&mut duplex, None).expect("a session that ends cleanly");
        assert_eq!(sent(&duplex).len(), 1);
    }

    /// Goal: `Ping` is answered on whatever object path it arrives at, because
    /// spec 0.43 puts `Peer` on every one. Method: a path priel publishes
    /// nothing at.
    #[test]
    fn ping_is_answered_at_any_object_path() {
        let reply = answer(&parsed(&ping(7)), None).expect("a reply to Ping");
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(reply.reply_serial(), Some(7));
        assert!(reply.body.is_empty());
    }

    /// Goal: `GetMachineId` answers with the id when there is one and errors
    /// when there is not, rather than inventing one - a made-up id is worse
    /// than an error, because a consumer would believe it. Method: both.
    #[test]
    fn get_machine_id_answers_with_the_id_or_says_it_has_none() {
        let call = parsed(&incoming(7, Some(PEER_INTERFACE), "GetMachineId", 0));
        let reply = answer(&call, Some("5aa16f5d981140ac87052e109dd35eaf")).expect("a reply");
        assert_eq!(reply.kind, MessageType::MethodReturn);
        assert_eq!(reply.signature(), "s");
        assert_eq!(
            reply.body,
            vec![Arg::Value(Value::Str(
                "5aa16f5d981140ac87052e109dd35eaf".into()
            ))]
        );

        let reply = answer(&call, None).expect("a reply");
        assert_eq!(reply.kind, MessageType::Error);
        assert_eq!(reply.error_name(), Some(ERROR_FAILED));
    }

    /// Goal: a member priel does not implement gets the error a real service
    /// gives, so a consumer sees a service that answered rather than one that
    /// hung. Method: the interfaces that land next, plus a call carrying no
    /// interface at all, which spec 0.43 permits.
    #[test]
    fn a_member_priel_does_not_implement_gets_a_well_formed_error() {
        for interface in [
            Some("org.mpris.MediaPlayer2.Player"),
            Some(PEER_INTERFACE),
            Some("org.freedesktop.DBus.Properties"),
            None,
        ] {
            let call = parsed(&incoming(7, interface, "Wobble", 0));
            let reply = answer(&call, None).expect("a reply to every call");
            assert_eq!(reply.kind, MessageType::Error, "{interface:?}");
            assert_eq!(reply.error_name(), Some(ERROR_UNKNOWN_METHOD));
            assert_eq!(reply.reply_serial(), Some(7));
            assert_eq!(reply.destination(), Some(":1.99"));
            assert_eq!(reply.signature(), "s", "an error carries its own text");
        }
    }

    /// Goal: a caller that said it wants no reply gets none, which spec 0.43
    /// requires and which a `Ping` from a monitor relies on. Method: set the
    /// flag on a call that would otherwise be answered.
    #[test]
    fn a_call_that_wants_no_reply_is_not_answered() {
        let call = parsed(&incoming(
            7,
            Some(PEER_INTERFACE),
            "Ping",
            NO_REPLY_EXPECTED,
        ));
        assert!(answer(&call, None).is_none());

        let mut duplex = after_handshake(vec![Turn::Bytes(incoming(
            7,
            Some(PEER_INTERFACE),
            "Ping",
            NO_REPLY_EXPECTED,
        ))]);
        served(&mut duplex, None).expect("a session that ends cleanly");
        assert!(sent(&duplex).is_empty());
    }

    /// Goal: what is not a method call is not answered. The bus sends
    /// `NameAcquired` unprompted the moment a name is taken, and a reply to a
    /// signal is a protocol error. Method: a signal and a stray reply.
    #[test]
    fn a_signal_and_a_stray_reply_are_not_answered() {
        let signal = Message {
            kind: MessageType::Signal,
            flags: NO_REPLY_EXPECTED,
            serial: 3,
            fields: vec![
                Field::Path(BUS_PATH.to_owned()),
                Field::Interface(BUS_NAME.to_owned()),
                Field::Member("NameAcquired".to_owned()),
                Field::Sender(BUS_NAME.to_owned()),
            ],
            body: Vec::new(),
        };
        let mut stream = marshal(&signal, Endian::NATIVE).expect("a signal the tests built");
        stream.extend(bus_reply(4, 999, Vec::new()));
        let mut duplex = after_handshake(vec![Turn::Bytes(stream)]);
        served(&mut duplex, None).expect("a session that ends cleanly");
        assert!(sent(&duplex).is_empty());
    }

    /// Goal: a signal the app queued goes out with a serial minted by the
    /// connection, since the app has no counter and two messages sharing a
    /// serial are two messages a reply cannot be matched to. Method: queue one
    /// before the loop starts and read it back off the wire.
    #[test]
    fn a_queued_signal_goes_out_with_a_minted_serial() {
        let mut duplex = after_handshake(vec![Turn::Idle]);
        {
            let (signals, outbox) = mpsc::channel();
            let mut conn = Connection::new(&mut duplex, None);
            conn.authenticate(Some(1000)).expect("the handshake");
            let queued = Message::signal(
                0,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
            );
            signals.send(queued).expect("a live channel");
            conn.serve(&outbox).expect("a session that ends cleanly");
        }
        let written = sent(&duplex);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].member(), Some("PropertiesChanged"));
        assert_eq!(written[0].serial, 1);
    }

    /// Goal: the app dropping its end takes the connection down with it, which
    /// is how the bus thread joins on the way out. Method: drop the sender
    /// before the loop runs, and check it stopped rather than reading on - the
    /// scripted call is never answered.
    #[test]
    fn the_app_dropping_its_end_ends_the_loop() {
        let mut duplex = after_handshake(vec![Turn::Bytes(ping(7))]);
        {
            let (signals, outbox) = mpsc::channel::<Message>();
            drop(signals);
            let mut conn = Connection::new(&mut duplex, None);
            conn.authenticate(Some(1000)).expect("the handshake");
            conn.serve(&outbox).expect("a loop that stops cleanly");
        }
        assert!(sent(&duplex).is_empty());
    }

    /// Goal: a peer cannot make priel hold an unbounded buffer by declaring a
    /// message larger than priel accepts. Method: more bytes than the cap, none
    /// of which complete a frame.
    #[test]
    fn a_peer_cannot_grow_the_inbox_without_limit() {
        let mut frame = ping(7);
        // A declared body length far past what will ever arrive, so the inbox
        // keeps accumulating and never holds a whole frame.
        frame[4..8].copy_from_slice(&1_000_000u32.to_ne_bytes());
        frame.extend(vec![0u8; MAX_INBOX * 2]);
        let mut duplex = after_handshake(vec![Turn::Bytes(frame)]);
        let error = served(&mut duplex, None).expect_err("a message priel will not hold");
        assert!(matches!(error, BusError::Wire(WireError::Framing(_))));
    }

    /// Goal: whatever is at the machine-id path is checked rather than trusted,
    /// because `GetMachineId` answers with it verbatim and a consumer cannot
    /// tell a bad id from a good one. Method: the real id off this machine,
    /// then the shapes something that is not a machine-id file has.
    #[test]
    fn a_machine_id_is_checked_before_it_is_answered_with() {
        assert_eq!(
            valid_machine_id("5aa16f5d981140ac87052e109dd35eaf\n").as_deref(),
            Some("5aa16f5d981140ac87052e109dd35eaf")
        );
        assert_eq!(valid_machine_id(""), None);
        assert_eq!(valid_machine_id("5aa16f5d981140ac87052e109dd35ea"), None);
        assert_eq!(valid_machine_id("5aa16f5d981140ac87052e109dd35eaff"), None);
        assert_eq!(valid_machine_id("5AA16F5D981140AC87052E109DD35EAF"), None);
        assert_eq!(valid_machine_id("not a machine id at all, but 32"), None);
    }

    fn parsed(frame: &[u8]) -> Message {
        parse(frame).expect("a frame the tests built")
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
        let mut duplex = Duplex::new(vec![Turn::Idle, Turn::Idle, Turn::Bytes(OK_LINE.to_vec())]);
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
