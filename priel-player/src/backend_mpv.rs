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

//! libmpv backend with a gapless play queue.
//!
//! The `prielseg://<seq>` protocol serves *multiple* concurrent streams from a
//! registry (keyed by a monotonic seq), so the next track can be preloaded and
//! appended to mpv's playlist while the current one plays — mpv then transitions
//! gaplessly (within a sample rate; a rate change still reinits the output,
//! which is unavoidable for bit-perfect switching).
//!
//! The player thread keeps `entries` in lockstep with mpv's playlist and, each
//! tick, removes anything mpv has already moved past (freeing its buffer).

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libmpv2::{Mpv, SetData, events::Event, protocol::Protocol};
use priel_core::PlayableSource;
use ureq::{Agent, Body, http::Response};

use crate::graph::{self, ServerSink};
use crate::hw::{self, HwParams};
use crate::{AudioDevice, Cmd, OutputAccess, PlaybackStatus, PlayerConfig, Published};

/// Lock a mutex, tolerating poisoning.
///
/// mpv invokes the protocol callbacks below across an FFI boundary, where
/// unwinding is undefined behaviour, so nothing on that path may panic. The data
/// behind these locks is a byte buffer plus its completion flags: a thread that
/// panicked mid-update leaves it consistent enough to keep serving or to abort,
/// so recovering the guard is strictly better than propagating the panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wait on a condvar, tolerating poisoning. See [`lock`].
fn wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

/// Sliding per-track window shared between a downloader and mpv's callbacks.
///
/// It is a *window*, not the whole track: the downloader appends at the end and
/// [`trim`] releases at the front, so a long track costs a bounded amount of
/// memory rather than all of itself.
struct Shared {
    inner: Mutex<Buf>,
    cv: Condvar,
}
struct Buf {
    /// The bytes still held, which begin at `base` in the stream - *not* at
    /// offset zero. Every offset in a `Cookie`, in `read_pos` and in `total` is
    /// absolute, so indexing this needs `base` subtracting first.
    ///
    /// A deque rather than a vector because releasing from the front has to be
    /// free: `read` does it, and `read` may not stall the audio.
    data: VecDeque<u8>,
    /// Stream offset of `data[0]`. Everything below it has been played and
    /// released, and cannot be served again.
    base: u64,
    complete: bool,
    total: Option<u64>,
    aborted: bool,
    /// Furthest byte offset any reader has consumed. Only ever moves forward, so
    /// a backward seek does not let the downloader run away again.
    read_pos: u64,
}

/// The offset one past the last byte held: `base` plus what is resident.
fn end_of(b: &Buf) -> u64 {
    b.base + b.data.len() as u64
}

/// How far the downloader may run ahead of the reader before it parks.
///
/// Without this, appending a preloaded next track downloads that whole track
/// immediately - and mpv does not open a queued playlist entry until it reaches
/// it, so nothing consumes those bytes in the meantime. 32 MiB is around 28s of
/// 24/192 FLAC, enough of a head start to cover the transition.
const DOWNLOAD_AHEAD_MAX: u64 = 32 * 1024 * 1024;

/// How much of what has already been read stays in memory behind the reader.
///
/// The bound that mattered was on *unread* bytes, which left a played track
/// holding all of itself by the end - twice over with a preload. Nothing needs
/// those bytes: the stream is served without a seek callback, so mpv cannot ask
/// for one it has gone past, and backward seeking is served from mpv's own
/// demuxer cache, which holds its own copy of what it has demuxed (50 MiB of
/// back buffer by default, which priel does not change). Measured, not assumed;
/// see `mpv_never_reads_a_byte_it_has_already_gone_past`.
///
/// So this is headroom rather than a requirement, and 8 MiB is chosen to be
/// obviously more than enough while still small: about 7s of 24/192 FLAC behind
/// a reader that mpv has already read past. With `DOWNLOAD_AHEAD_MAX` it puts
/// the resident ceiling at 40 MiB per track, against a whole hi-res track before.
const KEEP_BEHIND_MAX: u64 = 8 * 1024 * 1024;

/// How often the ALSA device parameters are re-read. The hardware rate only
/// changes on a track boundary, so once a second is generous.
const HW_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// The first slice of a segment published to the reader.
///
/// Small on purpose: this is what decides how soon a track makes a sound. The
/// per-chunk overhead is a lock and a condvar wake, measured at ~0.2 microseconds
/// even with a reader parked, so a small first chunk costs nothing worth counting.
const SEGMENT_CHUNK_MIN: usize = 16 * 1024;

/// The ceiling the chunk size ramps up to once playback is under way, where
/// fewer, larger reads are the cheaper shape and latency no longer matters.
const SEGMENT_CHUNK_MAX: usize = 256 * 1024;

/// Grow the publish size once audio is flowing.
///
/// Doubling rather than jumping keeps the first few chunks small, so a track
/// starts promptly, and reaches the cheap steady state within a few reads.
fn next_chunk(current: usize) -> usize {
    current.saturating_mul(2).min(SEGMENT_CHUNK_MAX)
}

type Registry = Arc<Mutex<HashMap<u64, Arc<Shared>>>>;

/// One mpv playlist entry we know about.
struct Entry {
    id: u64,          // TIDAL track id
    seq: Option<u64>, // registry key (None for Direct/http sources)
    /// The argument `loadfile` was given for this entry.
    ///
    /// Kept so the playlist can be rebuilt from `entries` alone. mpv abandons a
    /// file when the output will not open, and the only thing that resumes it
    /// is loading it again - which needs the string it was loaded with, since a
    /// direct source has no registry key to rebuild one from.
    source: String,
}

/// The output-device state the player thread carries between ticks.
struct Output {
    /// What mpv last reported, shared with the `Player` that owns this thread.
    devices: Arc<Mutex<Vec<AudioDevice>>>,
    /// A change made and not yet judged. See [`settle_switch`].
    pending: Option<Switch>,
    /// Why the last change did not take. Published in the status; display only.
    error: Option<String>,
    /// Exclusive access is asked for and has not been refused.
    exclusive: bool,
    /// The last exclusive open was refused, so priel is sharing instead. Held
    /// until the next request, because the badge has to keep saying so.
    refused: bool,
    /// The exclusivity now in force has already been put to the test by a
    /// track. Without this every subsequent load would re-arm the judgement and
    /// a slow-starting track could condemn a working exclusive output.
    judged: bool,
    /// The sound server's graph clock, as the frontend last read it. Published
    /// unchanged in the status; see [`crate::Cmd::SetClock`].
    clock: Option<crate::graph::ClockRates>,
}

/// A device change waiting to be judged.
struct Switch {
    /// What to go back to if the new device will not open.
    previous: String,
    /// The exclusivity to go back to with it. Restored together, because a
    /// device only counts as known-good on the terms it was opened under.
    previous_exclusive: bool,
    requested: String,
    /// Exclusive access was asked for as part of this change. What makes a
    /// refusal tellable from an ordinary device failure.
    exclusive: bool,
    /// When mpv has had long enough to have reopened the output.
    deadline: Instant,
}

/// How long mpv is given to reopen the output before the change is judged.
///
/// The reinit happens on mpv's own playloop, so the answer is not there the
/// instant the property is set. Short, because until this passes the app can
/// still advance the queue onto a device that is not working - and shorter
/// still in practice, since mpv giving up on the file settles the change the
/// moment it does so.
const SWITCH_GRACE: Duration = Duration::from_millis(400);

/// The backstop deadline for an exclusive request made before anything played.
///
/// Far longer than [`SWITCH_GRACE`], because there is nothing open yet: mpv
/// opens the output once it has audio to decode, which waits on the first bytes
/// of the track arriving. A verdict reached before then would be about the
/// network rather than the device.
///
/// It rarely decides anything now. mpv giving up on the file settles the
/// request the moment it happens (see [`drain_events`]), and this only covers a
/// failure that produces no event at all - in which case the output genuinely
/// is not open and the verdict is right. Waiting this out *was* what the
/// listener heard: eight seconds of silence before the fallback fired.
const EXCLUSIVE_OPEN_GRACE: Duration = Duration::from_secs(8);

/// How many devices are taken off mpv's list.
///
/// Bounded like everything else fed from outside. A machine with an audio
/// interface per channel still lands far inside this.
const DEVICE_LIST_MAX: i64 = 512;

/// Ask mpv which audio devices exist, and add the ones it cannot see.
///
/// mpv publishes this as a node array, which libmpv2 has no type for - but the
/// same property answers to an indexed path (`audio-device-list/3/name`), so the
/// safe wrapper is enough and nothing here has to reach past it.
///
/// Reading it makes mpv ask each audio driver what it can see. That advertises
/// devices; it opens none.
///
/// What it advertises is not everything that exists. ALSA's discovery interface
/// reports the plugin spellings of a card and never the raw `hw:` PCM
/// underneath - so the devices where exclusivity means anything were exactly
/// the ones the picker could not offer. [`hw::with_card_devices`] constructs
/// those from the kernel's own listing. This is the single place the published
/// list is built, so the picker and `--list-devices` cannot disagree.
fn read_devices(mpv: &Mpv) -> Vec<AudioDevice> {
    let count = mpv
        .get_property::<i64>("audio-device-list/count")
        .unwrap_or(0)
        .clamp(0, DEVICE_LIST_MAX);
    let mut devices = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for i in 0..count {
        // A name is what makes an entry selectable, so an entry without one is
        // skipped rather than shown as a row that cannot be chosen.
        let Ok(name) = mpv.get_property::<String>(&format!("audio-device-list/{i}/name")) else {
            continue;
        };
        let description = mpv
            .get_property::<String>(&format!("audio-device-list/{i}/description"))
            .unwrap_or_default();
        devices.push(AudioDevice { name, description });
    }
    hw::with_card_devices(devices)
}

/// The mpv audio driver a device identifier names.
///
/// Every identifier mpv lists carries its driver before the slash, and `--ao`
/// pins the driver: setting `audio-device` alone is silently ignored when it
/// names a different one, which is exactly what the picker offers. So a switch
/// sets both. `auto` is mpv's "no preference", which the driver list spells as
/// empty rather than as the word.
fn device_driver(device: &str) -> &str {
    match device.split_once('/') {
        Some((driver, _)) => driver,
        None if device == "auto" => "",
        None => device,
    }
}

/// Point mpv at a device, driver and all. See [`device_driver`].
fn apply_device(mpv: &Mpv, device: &str) {
    set_prop(mpv, "ao", device_driver(device));
    set_prop(mpv, "audio-device", device);
}

/// Can this device actually be had exclusively?
///
/// mpv's `audio-exclusive` is honoured by pipewire, wasapi, coreaudio and
/// audiounit, and **silently ignored** by every other driver - it says so in its
/// own manual. Reporting exclusivity after asking a driver that ignores the
/// request would be exactly the overstatement the indicator exists to avoid.
///
/// ALSA is the case that needs the device as well as the driver, because it has
/// no exclusive flag at all: a card device *is* exclusive, admitting one opener
/// and reaching the hardware with nothing in between, while `alsa/pipewire` and
/// `alsa/default` are routed back into the sound server and are shared however
/// they are opened. A card device is one that names a card, either as
/// `CARD=<id>` or as the index in `hw:<n>`; that is the same distinction the
/// hardware readout draws, and for the same reason.
fn honours_exclusive(device: &str) -> bool {
    match device_driver(device) {
        "pipewire" | "wasapi" | "coreaudio" | "audiounit" => true,
        "alsa" => device.contains("CARD=") || device.contains("hw:"),
        _ => false,
    }
}

/// mpv's "no preference", which is also the identifier it lists for it.
const AUTO_DEVICE: &str = "auto";

/// Where to keep playing when `refused` would not open exclusively.
///
/// Pure, and a table of four cases rather than reasoning buried in the switch
/// handler, because getting it wrong is silent: the audio simply goes somewhere
/// nobody asked for, or nowhere at all.
///
/// - A device the sound server owns was refused *by* the sound server, so the
///   same device shared is what was wanted. Clearing the exclusive flag is the
///   whole of the fallback and the device does not move.
/// - A hardware device has no shared spelling: `hw:` **is** the card, it admits
///   one opener, and reapplying it after clearing a flag mpv never sent to ALSA
///   lands on the same held card again. That was the bug. The server's own
///   entry for the same card is the nearest thing to what the listener asked
///   for - the same physical DAC, just shared.
/// - No entry for that card, or no sound server at all, leaves nothing to map
///   onto, and the honest answer is whatever the system would have played
///   through anyway.
///
/// The server's node name is prefixed rather than looked up in the player's own
/// device list, because that list is only ever populated when the picker has
/// been opened - which, for a request made on the command line, it has not. A
/// dump that answered at all is a sound server that is running, so the
/// identifier is good.
fn shared_fallback(refused: &str, sinks: &[ServerSink]) -> String {
    if !hw::is_direct_card_device(refused) {
        return refused.to_string();
    }
    sinks
        .iter()
        .find(|s| hw::refers_to_card(refused, s.card_id.as_deref(), s.card_index))
        .map_or_else(
            || AUTO_DEVICE.to_string(),
            |s| format!("pipewire/{}", s.node_name),
        )
}

/// Make a buffer servable from its beginning again.
///
/// Reloading a track plays it from the start, and `trim` releases what the
/// reader has already gone past - so this answers whether there is still a
/// beginning to serve. `read_pos` is wound back with it, or the first read of
/// the replay would trim away the bytes it is in the middle of serving.
fn rewind(shared: &Arc<Shared>) -> bool {
    let mut buf = lock(&shared.inner);
    if buf.base > 0 || buf.aborted {
        return false;
    }
    buf.read_pos = 0;
    true
}

/// Load the queue into mpv's playlist again after the output was rebuilt.
///
/// mpv abandons the file when the output will not open, so restoring a working
/// device is only half of a recovery: without this the interface waits forever
/// for a track that is no longer loaded, which is exactly what hardware testing
/// found. The playlist is rebuilt from `entries` wholesale rather than patched,
/// which keeps the two in lockstep by construction - afterwards mpv's playlist
/// *is* the entries vector, in order, and `entries` itself is untouched.
///
/// The position is lost and the track starts again. The bytes are already
/// buffered, so this costs no refetch, only the seconds already heard.
fn reload_entries(mpv: &Mpv, registry: &Registry, entries: &[Entry]) {
    for (position, entry) in entries.iter().enumerate() {
        if let Some(seq) = entry.seq {
            let buffered = lock(registry).get(&seq).cloned();
            // A buffer that has released its own beginning cannot be replayed
            // from one, and loading it would fail again for a new reason.
            if !buffered.is_some_and(|sh| rewind(&sh)) {
                log::error!(
                    "track {} cannot be played again from what is buffered",
                    entry.id
                );
                continue;
            }
        }
        let mode = if position == 0 { "replace" } else { "append" };
        command(mpv, "loadfile", &[&entry.source, mode]);
    }
}

/// What to report about how the output device is being held.
///
/// Three inputs and no state: a request, a refusal, and whether there is an
/// output open at all to be held. Pure so that "priel never claims exclusivity
/// it did not get" is a table of cases rather than a promise.
fn output_access(exclusive: bool, refused: bool, device: &str, output_open: bool) -> OutputAccess {
    if refused {
        return OutputAccess::Refused;
    }
    if exclusive && output_open && honours_exclusive(device) {
        return OutputAccess::Exclusive;
    }
    OutputAccess::Shared
}

/// Which device the hardware readout should be matched against.
///
/// mpv's own `audio-device` rather than the one priel started with, because the
/// picker can move the output mid-session: a hint that stays on the startup
/// device sends the readout to whichever card happened to be open first, and
/// the badge would then report another application's card as priel's. `auto`
/// names no device at all, so it is no hint - it would only ever match a card
/// by accident.
fn hw_hint(mpv: &Mpv, config: &PlayerConfig) -> Option<String> {
    match mpv.get_property::<String>("audio-device") {
        Ok(device) if device != "auto" && !device.is_empty() => Some(device),
        _ => config.audio_device.clone(),
    }
}

/// Did a device change leave the player with no output at all?
///
/// mpv does not fall back when the device it was given will not open: it
/// abandons the file and says so in its log, leaving a player that is loaded
/// but idle with no output open. That pair is the only symptom there is, and
/// it is what the previous device gets restored on.
fn switch_failed(has_entries: bool, output_open: bool) -> bool {
    has_entries && !output_open
}

/// Is there an output open at all?
///
/// `current-ao` is unavailable rather than empty when there is not, which is
/// the whole of how a device that would not open is told from an idle player.
fn output_is_open(mpv: &Mpv) -> bool {
    mpv.get_property::<String>("current-ao").is_ok()
}

/// The backstop: judge a change mpv has had long enough to have made.
///
/// A change is normally settled the moment mpv says it gave up on the file (see
/// [`drain_events`]), which is deterministic and immediate. This covers the case
/// where no such event arrives at all - a device that quietly never opens with
/// nothing loaded to abandon - and is deliberately the slower of the two, since
/// every second it waits is a second of silence.
fn settle_switch(mpv: &Mpv, registry: &Registry, entries: &[Entry], output: &mut Output) {
    if output
        .pending
        .as_ref()
        .is_none_or(|s| Instant::now() < s.deadline)
    {
        return;
    }
    settle_now(mpv, registry, entries, output, output_is_open(mpv));
}

/// Judge the pending change, and act on it.
///
/// Restoring a working device is what keeps audio going, and reloading is what
/// makes that mean anything: mpv abandons the file when an output will not
/// open, so the device alone recovers nothing and the interface waits forever
/// for a track that is no longer loaded. An exclusive request is judged the
/// same way and by the same symptom, because a device already held by
/// something else fails in exactly that manner.
///
/// **Taking the pending change is the whole of the runaway guard.** This is
/// reached from a failure event, and the reload it performs may fail in turn
/// and produce another one - which arrives here with nothing left to settle and
/// therefore moves no device and reloads nothing. A failure with nothing
/// pending is a track's own problem, which is also why `output_open` decides
/// rather than the event itself: a bad file and a held device arrive
/// identically, and only one of them leaves the player with no output.
fn settle_now(
    mpv: &Mpv,
    registry: &Registry,
    entries: &[Entry],
    output: &mut Output,
    output_open: bool,
) {
    let Some(switch) = output.pending.take() else {
        return;
    };
    if entries.is_empty() {
        // An idle player has no output open, which is indistinguishable from a
        // device that would not open. There is nothing to judge, so an
        // exclusive request goes back to waiting for a track to test it.
        output.judged = false;
        return;
    }
    if !switch_failed(true, output_open) {
        return;
    }
    // An exclusive request is the likelier culprit when one was made, and it is
    // the one priel can do something about: give the device back and play on.
    if switch.exclusive && !switch.previous_exclusive {
        refuse_exclusive(mpv, registry, entries, &switch, output);
        return;
    }
    log::error!(
        "the output {} would not open; going back to {}",
        switch.requested,
        switch.previous
    );
    output.error = Some(format!(
        "{} would not open, so the previous output was restored",
        switch.requested
    ));
    output.exclusive = switch.previous_exclusive;
    set_prop(mpv, "audio-exclusive", switch.previous_exclusive);
    apply_device(mpv, &switch.previous);
    // The track mpv abandoned when the output failed. Without this the device
    // is working again and nothing is playing through it.
    reload_entries(mpv, registry, entries);
}

/// Give the device back, move somewhere it can still be heard, and resume.
///
/// Three things, and leaving any of them out is a session that stops making a
/// sound. Reading the sound server's sinks runs a subprocess on this thread,
/// which is affordable exactly here: it happens once, on a failure, and the
/// output it would otherwise be holding up is not open anyway.
fn refuse_exclusive(
    mpv: &Mpv,
    registry: &Registry,
    entries: &[Entry],
    switch: &Switch,
    output: &mut Output,
) {
    let fallback = shared_fallback(&switch.requested, &graph::sinks());
    log::error!(
        "{} would not open exclusively; playing through {fallback} instead",
        switch.requested
    );
    output.error = Some(format!(
        "{} would not open exclusively; priel is sharing {fallback} instead",
        switch.requested
    ));
    output.refused = true;
    output.exclusive = false;
    set_prop(mpv, "audio-exclusive", false);
    apply_device(mpv, &fallback);
    // mpv abandoned the track when the output failed, so the device alone
    // restores nothing. The position is lost; the bytes are not.
    reload_entries(mpv, registry, entries);
}

/// The devices mpv can see, without a player thread. See
/// [`crate::audio_devices`].
pub fn audio_devices() -> Vec<AudioDevice> {
    match Mpv::new() {
        Ok(mpv) => read_devices(&mpv),
        Err(e) => {
            log::error!("mpv would not initialise, so its devices cannot be listed: {e}");
            Vec::new()
        }
    }
}

struct Cookie {
    shared: Arc<Shared>,
    pos: u64,
}

// ---- protocol callbacks (plain fns) ----
//
// These run on mpv's threads, not ours. They must never panic (unwinding across
// FFI is undefined behaviour), never perform I/O, and hold exactly one lock —
// the buffer's own — so a slow callback can only ever stall its own stream.

fn empty_complete() -> Arc<Shared> {
    Arc::new(Shared {
        inner: Mutex::new(Buf {
            data: VecDeque::new(),
            base: 0,
            complete: true,
            total: Some(0),
            aborted: false,
            read_pos: 0,
        }),
        cv: Condvar::new(),
    })
}

fn parse_seq(uri: &str) -> Option<u64> {
    uri.rsplit('/').next().and_then(|s| s.parse().ok())
}

fn open(reg: &mut Registry, uri: &str) -> Cookie {
    let shared = parse_seq(uri)
        .and_then(|s| lock(reg).get(&s).cloned())
        .unwrap_or_else(empty_complete);
    Cookie { shared, pos: 0 }
}

#[allow(
    clippy::boxed_local,
    reason = "signature is dictated by libmpv2's StreamClose"
)]
fn close(_cookie: Box<Cookie>) {}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "the stream_cb ABI is i64/u64; buffer offsets are bounded by a track in RAM"
)]
fn read(cookie: &mut Cookie, buf: &mut [i8]) -> i64 {
    let want = buf.len();
    let mut g = lock(&cookie.shared.inner);
    loop {
        if g.aborted {
            return 0; // EOF: the stream was discarded and its bytes are gone.
        }
        if cookie.pos < g.base {
            // Released by `trim`, and no longer obtainable. mpv is not supposed
            // to be able to get here - it never reads behind what it has taken -
            // so say so as an error rather than as a short read, which would
            // look like a truncated track with nothing anywhere to explain it.
            // `drain_events` records the end-of-file that mpv reports for it.
            return -1;
        }
        let end = end_of(&g);
        if cookie.pos < end {
            let start = (cookie.pos - g.base) as usize;
            let n = ((end - cookie.pos) as usize).min(want);
            for (d, s) in buf[..n].iter_mut().zip(g.data.range(start..start + n)) {
                // mpv hands us a C `char*`; reinterpret, never numerically convert.
                *d = i8::from_ne_bytes([*s]);
            }
            cookie.pos += n as u64;
            // Let the downloader know it may resume (see DOWNLOAD_AHEAD_MAX).
            g.read_pos = g.read_pos.max(cookie.pos);
            trim(&mut g);
            cookie.shared.cv.notify_all();
            return n as i64;
        }
        if g.complete {
            return 0; // EOF
        }
        g = wait(&cookie.shared.cv, g);
    }
}

/// Release what the reader has left behind, keeping [`KEEP_BEHIND_MAX`].
///
/// Called from `read`, so it must cost nothing: draining the front of a deque
/// moves the head index and copies no bytes, which is the reason `data` is a
/// deque at all. It allocates nothing and cannot panic - the count is clamped to
/// what is actually resident, because `read_pos` may sit past the end of it.
fn trim(b: &mut Buf) {
    let keep_from = b.read_pos.saturating_sub(KEEP_BEHIND_MAX);
    let behind = keep_from.saturating_sub(b.base);
    let drop_count = usize::try_from(behind)
        .unwrap_or(usize::MAX)
        .min(b.data.len());
    b.data.drain(..drop_count);
    b.base += drop_count as u64;
}

fn size(cookie: &mut Cookie) -> i64 {
    lock(&cookie.shared.inner)
        .total
        .and_then(|t| i64::try_from(t).ok())
        .unwrap_or(-1)
}

// ---- player thread ----

/// Start the player thread.
///
/// # Errors
///
/// If the OS will not give us a thread.
pub fn spawn(
    config: PlayerConfig,
    rx: Receiver<Cmd>,
    published: Published,
) -> std::io::Result<JoinHandle<()>> {
    let Published { status, devices } = published;
    // Named, because the log records which thread wrote each line and "-" says
    // nothing. This is also why the spawn is fallible now: `thread::spawn`
    // panics on the same condition, and a panic is not how priel reports that
    // it could not start.
    thread::Builder::new().name("player".into()).spawn(move || {
        let mpv = match Mpv::new() {
            Ok(m) => m,
            Err(e) => {
                log::error!("mpv would not initialise, so there is no playback: {e}");
                return;
            }
        };
        init_mpv(&mpv, &config);

        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        // SAFETY: `Protocol::new` is unsafe because libmpv stores these callbacks
        // and the cookie pointer and invokes them from its own threads for as
        // long as the protocol stays registered. That contract holds here:
        //  - the callbacks are plain `fn`s that capture nothing, and the data
        //    they receive is a cloned `Arc<Mutex<..>>` registry that outlives
        //    every callback invocation;
        //  - all shared state sits behind mutexes, so concurrent calls from
        //    mpv's threads are sound, and `lock`/`wait` above are poison-tolerant
        //    so no callback can unwind across the FFI boundary;
        //  - the registration is **never dropped**, and that is the whole of the
        //    contract. libmpv has `mpv_stream_cb_add_ro` and no remove: a
        //    protocol stays registered for the life of the handle. libmpv2's
        //    `Protocol` frees the callback data in `Drop` anyway, and borrows
        //    the `Mpv`, so the borrow checker *forces* it to drop first - which
        //    frees that data while mpv's demuxer threads can still call `open`.
        //    That is a use-after-free, and it was one: six core dumps in a day,
        //    every one faulting in `Mutex::lock` inside `open`, called from
        //    mpv's `open_demux_thread`. `forget` is the fix, and the leak it
        //    costs is one small box per handle, freed by the process exiting.
        let protocol = unsafe {
            Protocol::new(
                &mpv,
                "prielseg".into(),
                registry.clone(),
                open,
                close,
                read,
                // No seek callback, deliberately. Advertising the stream as
                // seekable makes the MP4 demuxer index the whole fragmented
                // file before it will play anything: measured at 100% of the
                // file downloaded before the first note, scaling with the
                // connection. Without it, audio starts on the first fragment.
                // In-track seeking still works through mpv's demuxer cache,
                // which it enables precisely because the stream is not seekable.
                //
                // This is also what makes bytes below the reader unreachable,
                // which is what lets `trim` release them. Measured rather than
                // assumed, by `mpv_never_reads_a_byte_it_has_already_gone_past`:
                // across a whole playback plus a backward seek, mpv opened the
                // stream once, never restarted a read behind what it had already
                // taken, and asked to seek exactly once - to offset 0, before it
                // had read anything, which is refused here and always has been.
                None,
                Some(size),
            )
        };
        if let Err(e) = protocol.register() {
            log::error!("the prielseg protocol would not register, so there is no playback: {e}");
            return;
        }
        // Deliberately never dropped; see the SAFETY note above.
        std::mem::forget(protocol);

        let mut entries: Vec<Entry> = Vec::new();
        let mut seq: u64 = 0;
        // Devices are enumerated only when asked for: it questions every audio
        // driver on the machine, which is work no session that never opens the
        // picker should pay for.
        let mut output = Output {
            devices,
            pending: None,
            error: None,
            // Applied by `init_mpv`; carried here so the first track to play can
            // put it to the test. priel never sets this itself - it is only ever
            // here because the listener asked.
            exclusive: config.exclusive,
            refused: false,
            judged: false,
            clock: None,
        };
        // The ALSA readout costs a handful of /proc reads, so it is refreshed on
        // its own slower cadence rather than on every status tick.
        let mut hw: Option<HwParams> = None;
        // `None` means "never probed", which forces the first tick to look.
        let mut hw_checked: Option<Instant> = None;

        loop {
            if drain_commands(&mpv, &registry, &mut entries, &mut seq, &mut output, &rx) {
                break;
            }
            drain_events(&mpv, &registry, &entries, &mut output);
            cleanup_playlist(&mpv, &registry, &mut entries);
            if hw_checked.is_none_or(|t| t.elapsed() >= HW_PROBE_INTERVAL) {
                hw = hw::probe(hw_hint(&mpv, &config).as_deref());
                hw_checked = Some(Instant::now());
            }
            settle_switch(&mpv, &registry, &entries, &mut output);
            let st = read_status(&mpv, &entries, hw.clone(), &output);
            let idle_backoff = poll_interval(st.playing);
            *lock(&status) = st;
            match rx.recv_timeout(idle_backoff) {
                Ok(cmd) => {
                    if handle_cmd(&mpv, &registry, &mut entries, &mut seq, &mut output, cmd) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        command(&mpv, "stop", &[]);
        clear_all(&registry, &mut entries);
    })
}

/// Set an mpv property, recording a rejection rather than discarding it.
///
/// A wrong name or type silently does nothing: libmpv returns an error and the
/// setting simply never applies. That is not hypothetical -
/// `demuxer-readahead-secs` was set here for a while and had no effect, and
/// nothing said so. The log line is the only symptom there will ever be.
fn set_prop<V: SetData>(mpv: &Mpv, name: &str, value: V) {
    if let Err(e) = mpv.set_property(name, value) {
        log::warn!("mpv rejected the property {name}: {e}");
    }
}

/// Run an mpv command, recording a rejection rather than discarding it.
///
/// Commands here are fire-and-forget by design - the UI must never wait on the
/// player - so a rejected one otherwise leaves the player quietly doing nothing.
fn command(mpv: &Mpv, name: &str, args: &[&str]) {
    if let Err(e) = mpv.command(name, args) {
        log::warn!("mpv rejected the command {name} {args:?}: {e}");
    }
}

/// How many mpv events one tick will take off the queue.
///
/// Bounded like everything else fed from outside: mpv fills this queue on its
/// own schedule, and the player loop has a status tick to get back to.
const EVENT_DRAIN_MAX: usize = 64;

/// Take what mpv has to say off its event queue and record it.
///
/// Nothing read this queue at all until now, which had two costs. mpv's queue
/// filled up and it dropped events; and, worse, **a failed load was
/// indistinguishable from an idle player** - playback state is inferred by
/// polling properties, and "not playing" looks the same whether the file ended,
/// never loaded, or was never asked for. The end-of-file reason is the only
/// place that difference exists.
///
/// libmpv2 turns an end-of-file *carrying an error* into `Err` rather than an
/// `EndFile` event, so that arm is the one that matters here.
///
/// It is also where a device change is settled. mpv giving up on a file is the
/// deterministic signal that an output would not open, and waiting out a
/// timeout instead cost eight seconds of silence with a buffering indicator
/// over it - the indicator lying, because nothing was buffering and the load
/// had already failed. The timeout in [`settle_switch`] stays as the backstop
/// for a failure that produces no event at all.
fn drain_events(mpv: &Mpv, registry: &Registry, entries: &[Entry], output: &mut Output) {
    for _ in 0..EVENT_DRAIN_MAX {
        // Zero timeout: this runs on the player tick and must not block it.
        match mpv.wait_event(0.0) {
            None => return,
            Some(Err(e)) => {
                log::warn!("mpv reported a failure: {e}");
                // Not necessarily the device: a bad file arrives identically.
                // `settle_now` decides on whether an output is open, and
                // consumes the pending change so a reload that fails again
                // cannot settle a second time.
                settle_now(mpv, registry, entries, output, output_is_open(mpv));
            }
            Some(Ok(event)) => match event {
                Event::EndFile(reason) => {
                    log::debug!("mpv ended a file: {}", end_file_reason(reason));
                }
                Event::FileLoaded => log::debug!("mpv loaded a file"),
                // mpv's own account of what it is doing, asked for by
                // `request_log_messages` and recorded at the matching level so a
                // reader can scan for severity across both halves of the log.
                Event::LogMessage {
                    prefix,
                    level,
                    text,
                    ..
                } => {
                    log::log!(target: "priel::mpv", mpv_level(level), "[{prefix}] {}", text.trim_end());
                }
                // The audio output being rebuilt is the audible cost of a
                // sample-rate change, so it is worth being able to see one.
                Event::AudioReconfig => log::debug!("mpv reconfigured the audio output"),
                Event::QueueOverflow => log::warn!("mpv dropped events: its queue overflowed"),
                Event::Shutdown => log::warn!("mpv is shutting down"),
                other => log::trace!("mpv event: {other:?}"),
            },
        }
    }
    log::debug!(
        "mpv had more than {EVENT_DRAIN_MAX} events waiting; the rest keep until next tick"
    );
}

/// mpv's log levels, mapped onto priel's.
///
/// mpv has two priel does not: `fatal`, which is an error either way, and `v`
/// (verbose), which sits between info and debug. An unrecognised level is kept
/// at info rather than dropped - a level priel has not heard of is not a reason
/// to hide the message.
fn mpv_level(level: &str) -> log::Level {
    match level {
        "fatal" | "error" => log::Level::Error,
        "warn" => log::Level::Warn,
        "v" | "debug" => log::Level::Debug,
        "trace" => log::Level::Trace,
        _ => log::Level::Info,
    }
}

/// Name an end-of-file reason.
///
/// The values are `mpv_end_file_reason` from mpv's `client.h`, which libmpv2
/// re-exports as a bare integer type without the constants. They are part of
/// mpv's public ABI and so cannot change; an unknown one still prints.
fn end_file_reason(reason: libmpv2::EndFileReason) -> String {
    match reason {
        0 => "reached the end".to_string(),
        2 => "stopped".to_string(),
        3 => "mpv quit".to_string(),
        4 => "an error".to_string(),
        5 => "redirected".to_string(),
        other => format!("reason {other}"),
    }
}

/// Run an mpv command whose rejection is routine rather than a fault.
///
/// mpv rejects a seek when nothing is loaded, and the seek keys work at any
/// time, so this would otherwise warn about the user pressing a key on an idle
/// player.
fn command_optional(mpv: &Mpv, name: &str, args: &[&str]) {
    if let Err(e) = mpv.command(name, args) {
        log::debug!("mpv declined the command {name} {args:?}: {e}");
    }
}

/// Apply the fixed player settings and select the output device.
fn init_mpv(mpv: &Mpv, config: &PlayerConfig) {
    // Nothing read mpv's event queue until `drain_events` existed, and mpv still
    // queues the events it has deprecated. Turning them off keeps the drain to
    // things that mean something.
    if let Err(e) = mpv.disable_deprecated_events() {
        log::warn!("mpv would not disable its deprecated events: {e}");
    }
    set_prop(mpv, "vid", "no");
    set_prop(mpv, "volume", 100i64);
    // "weak" = gapless only when the format matches; a sample-rate change
    // reinits the output (keeps playback bit-perfect). Do NOT force "yes".
    set_prop(mpv, "gapless-audio", "weak");

    // Buffering bounds. This is a memory decision as much as a resilience one:
    // mpv holds demuxed packets per *playlist entry*, so with a preloaded next
    // track the cost is paid twice.
    //
    // The duration limit that applies here is `cache-secs`, NOT
    // `demuxer-readahead-secs`: mpv overrides the latter once the cache is on and
    // the stream counts as a network stream, and `cache-secs` defaults to
    // 3600000 (unbounded). Both are set explicitly so whichever binds first does
    // so deliberately:
    //   - 64 MiB caps memory: about 58s of 24/192, or ~190s of 24/96;
    //   - 120s caps duration for material compressed far enough to get there.
    set_prop(mpv, "cache", "yes");
    set_prop(mpv, "cache-secs", 120.0);

    // Start playing as soon as there is something to decode, and let mpv handle
    // a slow link by pausing on underrun rather than by pre-filling a buffer up
    // front. That is the adaptive version of a download-rate check: it costs
    // nothing on a fast connection and only intervenes when the bytes genuinely
    // are not arriving. The two-second resume window is deliberately longer than
    // mpv's default of one, because a hi-res stream that resumes with a single
    // second in hand tends to stall again immediately.
    // The stream is served without a seek callback (see `spawn`), so mpv keeps a
    // seekable cache instead. Stated outright rather than left to the `auto`
    // heuristic, because in-track seeking depends on it.
    set_prop(mpv, "demuxer-seekable-cache", "yes");
    set_prop(mpv, "cache-pause-initial", "no");
    set_prop(mpv, "cache-pause", "yes");
    set_prop(mpv, "cache-pause-wait", 2.0);
    set_prop(mpv, "demuxer-max-bytes", 64i64 * 1024 * 1024);
    // Only consulted on the non-network path, where cache-secs does not apply.
    set_prop(mpv, "demuxer-readahead-secs", 30.0);

    match config.audio_device.as_deref() {
        Some("null") => set_prop(mpv, "ao", "null"),
        Some(dev) => set_prop(mpv, "audio-device", dev),
        None => set_prop(mpv, "ao", "pipewire"),
    }
    // Only ever true because the listener asked for it: taking a device
    // exclusively silences every other application on the machine, so priel
    // never selects this path on its own. Set unconditionally rather than only
    // when true, so the property always says what priel means.
    set_prop(mpv, "audio-exclusive", config.exclusive);

    if let Some(level) = config.mpv_log_level.as_deref() {
        request_log_messages(mpv, level);
    }
}

/// Ask mpv to deliver its own log as events.
///
/// libmpv2 does not wrap `mpv_request_log_messages`, so this is the one call
/// that reaches past it. The messages then arrive through [`drain_events`] and
/// are recorded like anything else, which is what puts mpv's account of a track
/// and priel's into one file, in order.
fn request_log_messages(mpv: &Mpv, level: &str) {
    let Ok(level) = std::ffi::CString::new(level) else {
        log::warn!("mpv log level {level:?} is not a C string, so mpv was not asked for its log");
        return;
    };
    // SAFETY: `mpv.ctx` is a live handle - `Mpv` owns it and outlives this call,
    // which happens on the thread that owns it and before any other thread can
    // reach it. `level` is NUL-terminated by `CString` and outlives the call;
    // mpv copies what it needs rather than retaining the pointer. The call only
    // sets the minimum level on the handle and returns an error code.
    let rc = unsafe { libmpv2_sys::mpv_request_log_messages(mpv.ctx.as_ptr(), level.as_ptr()) };
    if rc < 0 {
        log::warn!("mpv would not give up its log: error {rc}");
    }
}

/// Take every command that is already waiting, without blocking.
///
/// Separate from the tick below because they answer different questions: this
/// one is "has anything been asked of us", the tick is "what should we report".
/// Returns true if the thread should quit.
fn drain_commands(
    mpv: &Mpv,
    registry: &Registry,
    entries: &mut Vec<Entry>,
    seq: &mut u64,
    output: &mut Output,
    rx: &Receiver<Cmd>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(cmd) => {
                if handle_cmd(mpv, registry, entries, seq, output, cmd) {
                    return true;
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

/// How long to wait before the next status tick.
///
/// Fast enough for a smooth progress bar while audio is moving, and backed off
/// when it is not. The caller waits with `recv_timeout` rather than sleeping, so
/// a command still wakes the thread immediately either way - the backoff must
/// not cost keypress latency.
fn poll_interval(playing: bool) -> Duration {
    if playing {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(500)
    }
}

/// Returns true if the thread should quit.
fn handle_cmd(
    mpv: &Mpv,
    registry: &Registry,
    entries: &mut Vec<Entry>,
    seq: &mut u64,
    output: &mut Output,
    cmd: Cmd,
) -> bool {
    match cmd {
        Cmd::Quit => return true,
        Cmd::Load(id, src) => {
            clear_all(registry, entries);
            let (arg, s) = register_source(registry, seq, src);
            command(mpv, "loadfile", &[&arg, "replace"]);
            entries.push(Entry {
                id,
                seq: s,
                source: arg,
            });
            arm_exclusive_check(mpv, output);
        }
        Cmd::Append(id, src) => {
            let (arg, s) = register_source(registry, seq, src);
            command(mpv, "loadfile", &[&arg, "append"]);
            entries.push(Entry {
                id,
                seq: s,
                source: arg,
            });
        }
        Cmd::Next => {
            command(mpv, "playlist-next", &["force"]);
        }
        Cmd::SetPaused(paused) => {
            set_prop(mpv, "pause", paused);
        }
        Cmd::Seek(t) => {
            let v = format!("{t}");
            command_optional(mpv, "seek", &[&v, "absolute"]);
        }
        Cmd::SeekRelative(t) => {
            let v = format!("{t}");
            command_optional(mpv, "seek", &[&v, "relative"]);
        }
        Cmd::SetVolume(v) => {
            set_prop(mpv, "volume", v);
        }
        Cmd::Stop => {
            command(mpv, "stop", &[]);
            clear_all(registry, entries);
        }
        Cmd::RefreshDevices => {
            *lock(&output.devices) = read_devices(mpv);
        }
        Cmd::SetDevice(device) => set_device(mpv, output, device),
        Cmd::SetExclusive(exclusive) => set_exclusive(mpv, output, exclusive),
        Cmd::SetClock(clock) => output.clock = clock,
    }
    false
}

/// Move the output, refusing a device that is no longer there.
///
/// Checking the list first is what covers the device that was unplugged between
/// the picker opening and a row being chosen: mpv accepts any string for
/// `audio-device` and only fails later, by which time the track has stopped.
/// Refusing here costs nothing and leaves the output exactly as it was.
fn set_device(mpv: &Mpv, output: &mut Output, device: String) {
    let known = lock(&output.devices);
    // An empty list means nothing has been enumerated yet, not that no device
    // exists, so there is nothing to check the name against.
    if !known.is_empty() && !known.iter().any(|d| d.name == device) {
        drop(known);
        log::error!("no output device is called {device}, so the output was left alone");
        output.error = Some(format!(
            "{device} is not available; the output was left alone"
        ));
        return;
    }
    drop(known);
    let previous = mpv
        .get_property::<String>("audio-device")
        .unwrap_or_else(|_| "auto".to_string());
    log::info!("moving the output from {previous} to {device}");
    output.error = None;
    apply_device(mpv, &device);
    output.pending = Some(Switch {
        previous,
        previous_exclusive: output.exclusive,
        requested: device,
        exclusive: output.exclusive,
        deadline: Instant::now() + SWITCH_GRACE,
    });
}

/// Ask for, or give up, exclusive use of the device now in use.
///
/// The device is reapplied rather than only the property set, for two reasons:
/// it is what makes mpv rebuild the output now instead of at the next track,
/// and it puts the request through the very same judgement a device change goes
/// through. There is deliberately no second mechanism for deciding whether an
/// exclusive open worked - the symptom is identical, so the machinery is too.
fn set_exclusive(mpv: &Mpv, output: &mut Output, exclusive: bool) {
    let previous_exclusive = output.exclusive;
    let device = mpv
        .get_property::<String>("audio-device")
        .unwrap_or_else(|_| "auto".to_string());
    log::info!("asking for {device} to be {}", held_as(exclusive));
    output.exclusive = exclusive;
    // A fresh request; whatever was refused before was refused of something
    // else, or long enough ago that it is worth asking again.
    output.refused = false;
    output.error = None;
    set_prop(mpv, "audio-exclusive", exclusive);
    apply_device(mpv, &device);
    output.pending = Some(Switch {
        previous: device.clone(),
        previous_exclusive,
        requested: device,
        exclusive,
        deadline: Instant::now() + SWITCH_GRACE,
    });
    // Nothing may be playing, in which case this proves nothing and the first
    // track to load has to re-arm it.
    output.judged = true;
}

/// How a device is being held, for a log line.
fn held_as(exclusive: bool) -> &'static str {
    if exclusive { "exclusive" } else { "shared" }
}

/// Put a standing exclusive request to the test against a track that is
/// starting.
///
/// `--exclusive` is given before there is anything to play, and a request made
/// in the picker while the queue is empty is no different: an idle player has
/// no output open, so neither can be judged where it was made. The first track
/// to load is the first thing that actually opens the device, so that is where
/// the judgement is armed - once, because a later track re-arming it would let
/// a slow start condemn an exclusivity that is working.
fn arm_exclusive_check(mpv: &Mpv, output: &mut Output) {
    if !output.exclusive || output.judged || output.pending.is_some() {
        return;
    }
    let device = mpv
        .get_property::<String>("audio-device")
        .unwrap_or_else(|_| "auto".to_string());
    output.judged = true;
    output.pending = Some(Switch {
        previous: device.clone(),
        previous_exclusive: false,
        requested: device,
        exclusive: true,
        deadline: Instant::now() + EXCLUSIVE_OPEN_GRACE,
    });
}

/// Register a source and return (loadfile arg, registry seq if buffered).
fn register_source(
    registry: &Registry,
    seq: &mut u64,
    src: PlayableSource,
) -> (String, Option<u64>) {
    match src {
        PlayableSource::Direct(url) => (url, None),
        PlayableSource::Segments(urls) => {
            *seq += 1;
            let s = *seq;
            let shared = Arc::new(Shared {
                inner: Mutex::new(Buf {
                    data: VecDeque::new(),
                    base: 0,
                    complete: false,
                    total: None,
                    aborted: false,
                    read_pos: 0,
                }),
                cv: Condvar::new(),
            });
            lock(registry).insert(s, shared.clone());
            spawn_downloader(urls, &shared);
            (format!("prielseg://{s}"), Some(s))
        }
    }
}

/// Drop everything mpv has already moved past, freeing buffers.
fn cleanup_playlist(mpv: &Mpv, registry: &Registry, entries: &mut Vec<Entry>) {
    loop {
        let pos = mpv.get_property::<i64>("playlist-pos").unwrap_or(-1);
        if pos <= 0 {
            break;
        }
        command(mpv, "playlist-remove", &["0"]);
        if !entries.is_empty() {
            let e = entries.remove(0);
            if let Some(s) = e.seq
                && let Some(sh) = lock(registry).remove(&s)
            {
                abort(&sh);
            }
        }
    }
}

fn clear_all(registry: &Registry, entries: &mut Vec<Entry>) {
    let mut reg = lock(registry);
    for sh in reg.values() {
        abort(sh);
    }
    reg.clear();
    entries.clear();
}

fn abort(sh: &Arc<Shared>) {
    let mut g = lock(&sh.inner);
    g.aborted = true;
    // Drop the bytes now. mpv may still hold a cookie pointing at this buffer,
    // but an aborted stream reports EOF before it ever indexes `data`, so
    // reclaiming here rather than waiting for the cookie to close is safe - and
    // for a skipped hi-res track that is tens of megabytes freed at once.
    g.data = VecDeque::new();
    sh.cv.notify_all();
}

/// One agent for every segment download, for the whole process.
///
/// An `Agent` owns the connection pool and the rustls config, so building one
/// per track (as this did before) threw away keep-alive between tracks and
/// redid the TLS setup each time. Cloning or sharing it costs nothing.
static HTTP: LazyLock<Agent> = LazyLock::new(|| {
    Agent::new_with_config(
        Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .build(),
    )
});

/// Is there more unread data buffered than [`DOWNLOAD_AHEAD_MAX`] allows?
fn too_far_ahead(b: &Buf) -> bool {
    // Absolute offsets on both sides, so releasing bytes at the front does not
    // look like the reader catching up. Saturating: a reader may seek past the
    // end of what has been downloaded, so read_pos can legitimately exceed it.
    end_of(b).saturating_sub(b.read_pos) > DOWNLOAD_AHEAD_MAX
}

/// Why a segment stopped feeding the buffer.
enum Stop {
    /// The stream was discarded; do not touch the buffer again.
    Aborted,
    /// The fetch failed. Playback ends short, but the buffer must still be
    /// completed so a waiting reader is released rather than hanging.
    Failed,
}

fn spawn_downloader(urls: Vec<String>, shared: &Arc<Shared>) {
    let owned = shared.clone();
    let started = thread::Builder::new().name("segments".into()).spawn(move || {
        let shared = owned;
        let total = urls.len();
        for (done, u) in urls.iter().enumerate() {
            // A non-2xx is an `Err` here (the agent keeps ureq's default
            // `http_status_as_error`), matching the old `error_for_status`.
            let outcome = match HTTP.get(u).call() {
                Ok(mut resp) => stream_body(&mut resp, &shared),
                Err(e) => {
                    log::warn!(target: "priel::download", "segment {} of {total} could not be fetched: {e}", done + 1);
                    Err(Stop::Failed)
                }
            };
            match outcome {
                Ok(()) => {}
                Err(Stop::Aborted) => return,
                Err(Stop::Failed) => {
                    // The buffer is still completed below, so a waiting reader
                    // is released - but the track stops here, and this line is
                    // the only thing that will ever say why.
                    log::error!(
                        target: "priel::download",
                        "the track will end short: {done} of {total} segments fetched"
                    );
                    break;
                }
            }
        }
        finish(&shared);
    });
    if let Err(e) = started {
        // Nothing will ever fill this buffer, and a reader parked on the
        // condvar would wait for it forever. Release it here instead.
        log::error!(target: "priel::download", "no thread for the download: {e}");
        finish(shared);
    }
}

/// Mark a buffer finished and wake whoever is waiting on it.
///
/// Every path out of a download ends here, including the one where the download
/// never started: a reader blocked in the `read` callback is only ever released
/// by `complete`.
fn finish(shared: &Arc<Shared>) {
    let mut g = lock(&shared.inner);
    // The stream's length, not what is still resident: `trim` has been releasing
    // the front of it all along.
    g.total = Some(end_of(&g));
    g.complete = true;
    shared.cv.notify_all();
}

/// Feed one segment into the shared buffer *as it arrives*.
///
/// Appending in chunks rather than a segment at a time is what lets playback
/// start promptly. A hi-res DASH segment is several megabytes; reading it to the
/// end before publishing any of it meant mpv had nothing to decode until the
/// whole thing landed, which is seconds of silence on every track change.
///
/// Backpressure is checked per chunk for the same reason: a segment can be
/// larger than the whole allowance.
fn stream_body(resp: &mut Response<Body>, shared: &Arc<Shared>) -> Result<(), Stop> {
    let mut reader = resp.body_mut().as_reader();
    let mut chunk = vec![0u8; SEGMENT_CHUNK_MAX];
    let mut want = SEGMENT_CHUNK_MIN;
    loop {
        {
            let mut g = lock(&shared.inner);
            while !g.aborted && too_far_ahead(&g) {
                g = wait(&shared.cv, g);
            }
            if g.aborted {
                return Err(Stop::Aborted);
            }
        }
        match reader.read(&mut chunk[..want]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                {
                    let mut g = lock(&shared.inner);
                    if g.aborted {
                        return Err(Stop::Aborted);
                    }
                    g.data.extend(&chunk[..n]);
                    // Wake whoever is blocked in `read`: this is the
                    // notification that turns a downloaded chunk into sound.
                    shared.cv.notify_all();
                }
                want = next_chunk(want);
            }
            Err(e) => {
                log::warn!(target: "priel::download", "a segment stopped mid-stream: {e}");
                return Err(Stop::Failed);
            }
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "bitrate is a display value; fractional bits/s are meaningless"
)]
fn read_status(
    mpv: &Mpv,
    entries: &[Entry],
    hw: Option<HwParams>,
    output: &Output,
) -> PlaybackStatus {
    let position = mpv.get_property::<f64>("time-pos").unwrap_or(0.0);
    let duration = mpv.get_property::<f64>("duration").unwrap_or(0.0);
    let paused = mpv.get_property::<bool>("pause").unwrap_or(false);
    let idle = mpv.get_property::<bool>("core-idle").unwrap_or(true);
    let ended = mpv.get_property::<bool>("eof-reached").unwrap_or(false);
    let volume = mpv.get_property::<f64>("volume").unwrap_or(100.0);
    let sample_rate = mpv
        .get_property::<i64>("audio-out-params/samplerate")
        .ok()
        .and_then(|r| u32::try_from(r).ok())
        .unwrap_or(0);
    let out_format = mpv
        .get_property::<String>("audio-out-params/format")
        .unwrap_or_default();
    // The decoder side, for comparison: a rate that differs from the output rate
    // means something resampled between the two.
    let in_sample_rate = mpv
        .get_property::<i64>("audio-params/samplerate")
        .ok()
        .and_then(|r| u32::try_from(r).ok())
        .unwrap_or(0);
    let in_format = mpv
        .get_property::<String>("audio-params/format")
        .unwrap_or_default();
    let codec = mpv
        .get_property::<String>("audio-codec-name")
        .unwrap_or_default();
    let bitrate = mpv.get_property::<f64>("audio-bitrate").unwrap_or(0.0) as i64;
    // The audio server's volume for our stream. Absent on outputs with no such
    // concept (the null AO used by the tests, for one), which is not a fault.
    let ao_volume = mpv.get_property::<f64>("ao-volume").ok();
    let cache_secs = mpv
        .get_property::<f64>("demuxer-cache-duration")
        .unwrap_or(0.0);
    // What the picker marks as in use. mpv's own default for this is the word
    // `auto`, which is also the identifier of the entry it lists for it, so the
    // two compare directly.
    let audio_device = mpv
        .get_property::<String>("audio-device")
        .unwrap_or_else(|_| "auto".to_string());
    // `current-ao` is unavailable rather than empty when no output is open, so
    // this is also how "priel is holding nothing at all" is told apart from
    // "priel is holding this device exclusively".
    let output_open = mpv.get_property::<String>("current-ao").is_ok();
    let access = output_access(output.exclusive, output.refused, &audio_device, output_open);
    PlaybackStatus {
        loaded: duration > 0.0,
        playing: !idle && !paused,
        paused,
        position,
        duration,
        sample_rate,
        out_format,
        in_sample_rate,
        in_format,
        codec,
        bitrate,
        ended,
        volume,
        current_id: entries.first().map_or(0, |e| e.id),
        has_next: entries.len() > 1,
        cache_secs,
        ao_volume,
        audio_device,
        device_error: output.error.clone(),
        hw,
        access,
        clock: output.clock.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    /// A buffer holding `data` from offset zero, as a fresh download does.
    fn shared(data: Vec<u8>, complete: bool) -> Arc<Shared> {
        Arc::new(Shared {
            inner: Mutex::new(Buf {
                total: complete.then_some(data.len() as u64),
                data: data.into(),
                base: 0,
                complete,
                aborted: false,
                read_pos: 0,
            }),
            cv: Condvar::new(),
        })
    }

    /// A logger the tests read back, so "this was recorded" is asserted rather
    /// than assumed. A rejected property or command does nothing observable
    /// otherwise, which is the whole reason it is logged.
    struct Capture;

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// The buffer is global, so the tests that read it take turns - and so do
    /// the tests that fill it without reading it. A test driving a live mpv logs
    /// every event it drains, into whichever capture happens to be open.
    static READING: Mutex<()> = Mutex::new(());

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            // The thread name is part of what is being asserted: a line is only
            // placeable if the thread that wrote it has a name.
            let current = thread::current();
            let who = current.name().unwrap_or("-").to_string();
            lock(&LINES).push(format!("{who}|{} {}", record.target(), record.args()));
        }
        fn flush(&self) {}
    }

    /// Run `f` and hand back everything it logged.
    fn captured<F: FnOnce()>(f: F) -> Vec<String> {
        static CAPTURE: Capture = Capture;
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        let _turn = lock(&READING);
        INSTALLED.call_once(|| {
            let _ = log::set_logger(&CAPTURE);
            log::set_max_level(log::LevelFilter::Trace);
        });
        lock(&LINES).clear();
        f();
        lock(&LINES).clone()
    }

    /// A playlist entry with nothing buffered behind it.
    fn entry(id: u64) -> Entry {
        Entry {
            id,
            seq: None,
            source: format!("http://127.0.0.1:1/{id}"),
        }
    }

    /// An empty stream registry, for the tests that need one to pass along.
    fn registry() -> Registry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// Device state nobody is watching, for the command tests.
    fn no_output() -> Output {
        Output {
            devices: Arc::new(Mutex::new(Vec::new())),
            pending: None,
            error: None,
            exclusive: false,
            refused: false,
            judged: false,
            clock: None,
        }
    }

    fn cookie(sh: &Arc<Shared>) -> Cookie {
        Cookie {
            shared: sh.clone(),
            pos: 0,
        }
    }

    /// Serves one canned body then closes. Used to exercise the download path
    /// without reaching the network. Takes an owned body because one of them is
    /// a whole track, built rather than written out as a literal.
    fn one_shot(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/seg", listener.local_addr().expect("addr"));
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 S\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
            }
        });
        url
    }

    // ---- uri routing ----

    #[test]
    fn a_stream_uri_carries_its_registry_key() {
        // Goal: the protocol multiplexes several concurrent buffers by seq, so
        // the key has to survive the round trip through the URI.
        assert_eq!(parse_seq("prielseg://7"), Some(7));
        assert_eq!(parse_seq("prielseg://0"), Some(0));
        assert_eq!(parse_seq("prielseg://x"), None);
        assert_eq!(parse_seq(""), None);
    }

    #[test]
    fn opening_an_unknown_stream_yields_an_empty_one_rather_than_failing() {
        // Goal: `open` cannot report an error to mpv, and it must never panic
        // across the FFI boundary. An immediate clean EOF is the safe answer.
        let mut reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut c = open(&mut reg, "prielseg://404");
        let mut buf = [0i8; 8];
        assert_eq!(read(&mut c, &mut buf), 0, "unknown stream reads as EOF");
        assert_eq!(size(&mut c), 0);
    }

    #[test]
    fn opening_a_registered_stream_finds_its_buffer() {
        // Goal: the happy path of the same lookup.
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        lock(&reg).insert(3, shared(b"abcd".to_vec(), true));
        let mut reg2 = reg.clone();
        let mut c = open(&mut reg2, "prielseg://3");
        let mut buf = [0i8; 8];
        assert_eq!(read(&mut c, &mut buf), 4);
        close(Box::new(c));
    }

    // ---- reading ----

    #[test]
    fn reads_copy_bytes_verbatim_and_advance() {
        // Goal: mpv gets a C `char*`; the bytes must be reinterpreted, not
        // numerically converted, or anything above 0x7F is corrupted. 0xFF as a
        // signed byte is -1, and that is what must land in the buffer.
        let sh = shared(vec![0x00, 0x7F, 0x80, 0xFF], true);
        let mut c = cookie(&sh);
        let mut buf = [0i8; 4];
        assert_eq!(read(&mut c, &mut buf), 4);
        assert_eq!(buf, [0, 127, -128, -1]);
        assert_eq!(read(&mut c, &mut buf), 0, "a second read is at EOF");
    }

    #[test]
    fn a_short_buffer_takes_what_fits_and_leaves_the_rest() {
        // Goal: mpv chooses the chunk size; a read must never overrun it, and
        // the next read must resume exactly where this one stopped.
        let sh = shared(b"abcdef".to_vec(), true);
        let mut c = cookie(&sh);
        let mut buf = [0i8; 4];
        assert_eq!(read(&mut c, &mut buf), 4);
        assert_eq!(&buf, b"abcd".map(|b| i8::from_ne_bytes([b])).as_slice());
        assert_eq!(
            read(&mut c, &mut buf),
            2,
            "the tail is shorter than the buffer"
        );
    }

    #[test]
    fn a_read_waits_for_a_slow_download_instead_of_reporting_eof() {
        // Goal: this is the whole point of the protocol - an incomplete buffer
        // must block, not look like the end of the track. Returning 0 here would
        // truncate every stream that outruns its download.
        let sh = shared(Vec::new(), false);
        let writer = sh.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut g = lock(&writer.inner);
            g.data.extend(b"late");
            writer.cv.notify_all();
        });
        let mut c = cookie(&sh);
        let mut buf = [0i8; 8];
        assert_eq!(
            read(&mut c, &mut buf),
            4,
            "should have waited for the bytes"
        );
    }

    #[test]
    fn an_aborted_stream_stops_reading_immediately() {
        // Goal: skipping a track must release its reader at once rather than
        // leaving mpv blocked on a buffer nobody is filling any more.
        let sh = shared(Vec::new(), false);
        let other = sh.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            abort(&other);
        });
        let mut c = cookie(&sh);
        let mut buf = [0i8; 8];
        assert_eq!(read(&mut c, &mut buf), 0);
    }

    #[test]
    fn abort_releases_the_buffer_memory() {
        // Goal: a skipped hi-res track is hundreds of megabytes. Waiting for mpv
        // to drop its cookie before reclaiming that is what made the process sit
        // at a gigabyte.
        let sh = shared(vec![7u8; 4096], false);
        abort(&sh);
        let g = lock(&sh.inner);
        assert!(g.aborted);
        assert!(
            g.data.is_empty(),
            "the bytes should be gone, not just flagged"
        );
    }

    #[test]
    fn size_is_unknown_until_the_download_finishes() {
        // Goal: mpv treats -1 as "not seekable yet"; claiming a size before the
        // total is known would make it seek into bytes that do not exist.
        let sh = shared(vec![1, 2, 3], false);
        let mut c = cookie(&sh);
        assert_eq!(size(&mut c), -1);
        {
            let mut g = lock(&sh.inner);
            g.total = Some(3);
        }
        assert_eq!(size(&mut c), 3);
    }

    // ---- backpressure ----

    #[test]
    fn the_downloader_parks_only_when_far_ahead_of_the_reader() {
        // Goal: without this the preloaded next track downloads in full while
        // nothing reads it, which is where the memory went. It must key off
        // unread bytes, not total bytes, or a long track parks forever.
        let mut b = Buf {
            data: vec![0; 8].into(),
            base: 0,
            complete: false,
            total: None,
            aborted: false,
            read_pos: 0,
        };
        assert!(!too_far_ahead(&b), "a small buffer keeps downloading");

        b.data = vec![0; usize::try_from(DOWNLOAD_AHEAD_MAX).unwrap() + 1].into();
        assert!(too_far_ahead(&b), "unread data past the cap parks it");

        b.read_pos = end_of(&b);
        assert!(!too_far_ahead(&b), "once consumed it resumes");

        b.read_pos = end_of(&b) + 999;
        assert!(!too_far_ahead(&b), "a seek past the end must not underflow");

        // The same again with the front released: the allowance is on unread
        // bytes, and a base offset must not make consumed ones count twice or
        // underflow the subtraction.
        b.base = 1_000_000;
        b.read_pos = 0;
        assert!(too_far_ahead(&b), "unread data past the cap still parks it");
        b.read_pos = end_of(&b);
        assert!(!too_far_ahead(&b), "and consuming it still resumes");
    }

    #[test]
    fn reading_wakes_a_parked_downloader() {
        // Goal: the park is a condvar wait, so the reader has to notify or the
        // download stalls until the next unrelated notification.
        let sh = shared(b"xy".to_vec(), false);
        let mut c = cookie(&sh);
        let mut buf = [0i8; 2];
        assert_eq!(read(&mut c, &mut buf), 2);
        assert_eq!(lock(&sh.inner).read_pos, 2, "consumption must be recorded");
    }

    // ---- releasing what has been played ----

    /// A buffer of `len` bytes with a repeating pattern, so a read can be
    /// checked against the offset it came from rather than just its length.
    fn patterned(len: usize) -> Arc<Shared> {
        let mut v = Vec::with_capacity(len);
        for i in 0..len {
            v.push(u8::try_from(i % 251).unwrap_or(0));
        }
        shared(v, false)
    }

    fn at(offset: usize) -> i8 {
        i8::from_ne_bytes([u8::try_from(offset % 251).unwrap_or(0)])
    }

    #[test]
    fn trimming_keeps_a_margin_behind_the_reader_and_never_rewinds() {
        // Goal: the policy on its own. It has to be exact about which bytes go,
        // because `base` is what every absolute offset is measured against, and
        // it has to survive a read position past the end of what is held - which
        // is legitimate, see the backpressure test above.
        let mut b = Buf {
            data: vec![0u8; 4096].into(),
            base: 0,
            complete: false,
            total: None,
            aborted: false,
            read_pos: 0,
        };

        trim(&mut b);
        assert_eq!(b.base, 0, "nothing goes until the margin is used up");
        assert_eq!(b.data.len(), 4096);

        b.read_pos = KEEP_BEHIND_MAX + 1000;
        trim(&mut b);
        assert_eq!(b.base, 1000, "the margin behind the reader stays");
        assert_eq!(b.data.len(), 3096);
        assert_eq!(end_of(&b), 4096, "the stream offsets do not move");

        b.read_pos = u64::MAX;
        trim(&mut b);
        assert_eq!(b.data.len(), 0, "a reader past the end releases the rest");
        assert_eq!(b.base, 4096, "and never past what was actually held");

        trim(&mut b);
        assert_eq!(b.base, 4096, "trimming an empty buffer changes nothing");
    }

    #[test]
    fn reading_releases_the_bytes_left_behind() {
        // Goal: the bound that existed was on *unread* bytes, so a played track
        // kept all of itself. Reading is what must release them, and it must
        // still hand back the right bytes once the front has gone: every offset
        // is absolute and `data` no longer starts at zero.
        const LEN: usize = 12 * 1024 * 1024;
        const STEP: usize = 64 * 1024;
        let sh = patterned(LEN);
        let mut c = cookie(&sh);
        // On the heap: mpv's own read buffer is not on this thread's stack
        // either, and 64 KiB of it would be over clippy's limit.
        let mut buf = vec![0i8; STEP];
        let mut got = 0usize;
        while got < LEN {
            let n = read(&mut c, &mut buf);
            assert!(n > 0, "the buffer is fully populated, so nothing may block");
            let n = usize::try_from(n).unwrap_or(0);
            assert_eq!(
                buf[0],
                at(got),
                "a read after a trim came from the wrong offset"
            );
            assert_eq!(buf[n - 1], at(got + n - 1));
            got += n;
        }
        assert_eq!(got, LEN);

        let g = lock(&sh.inner);
        assert_eq!(
            g.base,
            LEN as u64 - KEEP_BEHIND_MAX,
            "everything but the margin should have been released"
        );
        assert_eq!(g.data.len(), usize::try_from(KEEP_BEHIND_MAX).unwrap_or(0));
        assert_eq!(end_of(&g), LEN as u64, "the stream itself did not shrink");
    }

    #[test]
    fn a_long_track_holds_the_ceiling_rather_than_all_of_itself() {
        // Goal: the point of the whole change. 300 MiB fed through the buffer at
        // the rate it is consumed - a long hi-res track - must never hold more
        // than the downloader's allowance plus the margin behind the reader.
        const MIB: usize = 1024 * 1024;
        let ceiling = usize::try_from(DOWNLOAD_AHEAD_MAX + KEEP_BEHIND_MAX).unwrap_or(usize::MAX);
        let sh = shared(Vec::new(), false);
        let block = vec![0u8; MIB];
        let mut worst = 0usize;
        for played in 0..300u64 {
            let mut g = lock(&sh.inner);
            g.data.extend(&block[..]);
            // The reader stays a little behind, as it does in life.
            g.read_pos = played * MIB as u64;
            trim(&mut g);
            worst = worst.max(g.data.len());
            assert!(!too_far_ahead(&g), "the reader is keeping up");
        }
        assert!(
            worst <= ceiling,
            "held {worst} bytes at worst, over the {ceiling} byte ceiling"
        );
        let g = lock(&sh.inner);
        assert_eq!(end_of(&g), 300 * MIB as u64, "all 300 MiB went through it");
        assert!(
            g.data.len() < 300 * MIB,
            "the whole track was still resident"
        );
    }

    #[test]
    fn a_whole_track_is_never_resident_at_once() {
        // Goal: the same ceiling as the test above, but end to end - the real
        // downloader on one side with its own backpressure, a real mpv demuxer
        // reading on the other, and nothing simulated in between. A track of
        // 57 MiB is comfortably over the 40 MiB ceiling, so if the released
        // bytes were still being held this would hold all of it.
        //
        // 192 kHz stereo, because the demuxer reads ahead until *either* budget
        // runs out and this is the shape of the material priel plays: at CD rate
        // 64 MiB is 12 minutes, so `cache-secs` would bind first and mpv would
        // read the stream in real time rather than as fast as it arrives.
        // Draining mpv's events logs them, and the capture buffer those land in
        // is global; see `READING`.
        let _turn = lock(&READING);
        let media = wav_bytes(192_000, 2, 78);
        let track_bytes = media.len();
        let ceiling = usize::try_from(DOWNLOAD_AHEAD_MAX + KEEP_BEHIND_MAX).unwrap_or(usize::MAX);
        assert!(
            track_bytes > ceiling,
            "the track has to overrun the ceiling"
        );

        let mpv = silent_mpv();
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let sh = shared(Vec::new(), false);
        lock(&registry).insert(1, sh.clone());
        // SAFETY: same contract as the player thread; see `spawn`.
        let protocol = unsafe {
            Protocol::new(
                &mpv,
                "prielseg".into(),
                registry.clone(),
                open,
                close,
                read,
                None,
                Some(size),
            )
        };
        protocol.register().expect("the protocol should register");
        // Never dropped, for the reason in `spawn`: dropping it frees the
        // callback data while mpv can still call back into it.
        std::mem::forget(protocol);

        spawn_downloader(vec![one_shot(media)], &sh);
        command(&mpv, "loadfile", &["prielseg://1", "replace"]);

        // Enough of the track past the ceiling to prove the point. Not all of
        // it: mpv stops reading ahead when its own cache is full, which is what
        // `demuxer-max-bytes` is for, so waiting for the last byte would wait
        // for playback to consume the rest in real time.
        let target = ceiling as u64 + 8 * 1024 * 1024;
        let mut peak = 0usize;
        let mut reached = 0u64;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && reached < target {
            {
                let g = lock(&sh.inner);
                peak = peak.max(g.data.len());
                reached = g.read_pos;
            }
            drain_events(&mpv, &registry, &[], &mut no_output());
            thread::sleep(Duration::from_millis(5));
        }
        command(&mpv, "stop", &[]);
        abort(&sh);

        assert!(
            reached >= target,
            "mpv only read {reached} of the {track_bytes} byte track, which proves nothing"
        );
        assert!(
            peak <= ceiling + SEGMENT_CHUNK_MAX,
            "held {peak} bytes at once, over the {ceiling} byte ceiling"
        );
    }

    #[test]
    fn a_read_below_what_was_released_is_an_error_not_a_short_track() {
        // Goal: mpv never asks for one of these - it cannot seek the stream and
        // it does not reopen it, which is what the trimming rests on and what
        // `mpv_never_reads_a_byte_it_has_already_gone_past` establishes. If that
        // ever stops being true, the symptom must be a reported failure and not
        // a track that quietly plays back the wrong bytes or stops early.
        let sh = patterned(64);
        {
            let mut g = lock(&sh.inner);
            g.data.drain(..32);
            g.base = 32;
            g.read_pos = 32;
        }
        let mut c = cookie(&sh);
        let mut buf = [0i8; 8];
        assert_eq!(read(&mut c, &mut buf), -1, "offset 0 is gone");

        c.pos = 32;
        assert_eq!(read(&mut c, &mut buf), 8, "what is still held still reads");
        assert_eq!(buf[0], at(32));
    }

    #[test]
    fn the_size_reported_is_the_whole_stream_not_what_is_still_held() {
        // Goal: mpv is told the length of the track. Reporting only the resident
        // window would claim a track far shorter than it is.
        let sh = patterned(64);
        {
            let mut g = lock(&sh.inner);
            g.data.drain(..48);
            g.base = 48;
            g.read_pos = 48;
        }
        finish(&sh);
        let mut c = cookie(&sh);
        assert_eq!(size(&mut c), 64);
    }

    // ---- downloading ----

    #[test]
    fn a_downloaded_segment_lands_in_the_buffer_and_completes() {
        // Goal: the end-to-end fetch path, including the completion flag that
        // tells a waiting reader it has reached the real end of the track.
        let url = one_shot(b"segment-bytes".to_vec());
        let sh = shared(Vec::new(), false);
        spawn_downloader(vec![url], &sh);
        for _ in 0..200 {
            if lock(&sh.inner).complete {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let g = lock(&sh.inner);
        assert_eq!(g.data, b"segment-bytes");
        assert_eq!(g.total, Some(13));
        assert!(g.complete);
    }

    #[test]
    fn an_unreachable_segment_still_completes_the_stream() {
        // Goal: a failed fetch must not leave readers blocked forever. The
        // stream ends short, which is audible, rather than hanging, which is not
        // recoverable without a restart.
        let sh = shared(Vec::new(), false);
        // Port 1 on loopback refuses immediately, so this does not touch DNS.
        spawn_downloader(vec!["http://127.0.0.1:1/x".into()], &sh);
        for _ in 0..200 {
            if lock(&sh.inner).complete {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lock(&sh.inner).complete,
            "a dead download must still finish"
        );
    }

    // ---- registry bookkeeping ----

    #[test]
    fn a_direct_source_needs_no_buffer_but_segments_do() {
        // Goal: BTS URLs are handed to mpv untouched; only segmented sources get
        // a registry entry, and each gets a distinct key so two can be live.
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut seq = 0;

        let (arg, key) = register_source(&reg, &mut seq, PlayableSource::Direct("http://d".into()));
        assert_eq!(arg, "http://d");
        assert!(key.is_none());
        assert!(lock(&reg).is_empty());

        let (a1, k1) = register_source(
            &reg,
            &mut seq,
            PlayableSource::Segments(vec!["http://127.0.0.1:1/a".into()]),
        );
        let (a2, k2) = register_source(
            &reg,
            &mut seq,
            PlayableSource::Segments(vec!["http://127.0.0.1:1/b".into()]),
        );
        assert_eq!(a1, "prielseg://1");
        assert_eq!(a2, "prielseg://2");
        assert_ne!(k1, k2, "concurrent streams must not share a key");
        assert_eq!(lock(&reg).len(), 2);
    }

    #[test]
    fn clearing_drops_every_entry_and_aborts_its_buffer() {
        // Goal: stopping playback must free everything at once; a leftover entry
        // is both a leak and a stream that could still be served.
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let sh = shared(vec![1; 32], false);
        lock(&reg).insert(1, sh.clone());
        let mut entries = vec![Entry {
            id: 10,
            seq: Some(1),
            source: "prielseg://1".into(),
        }];

        clear_all(&reg, &mut entries);

        assert!(lock(&reg).is_empty());
        assert!(entries.is_empty());
        assert!(
            lock(&sh.inner).aborted,
            "the orphaned buffer must be released"
        );
    }

    // ---- against a real mpv handle ----

    /// The null output, nothing else configured.
    fn silent_config() -> PlayerConfig {
        PlayerConfig {
            audio_device: Some("null".into()),
            ..PlayerConfig::default()
        }
    }

    fn silent_mpv() -> Mpv {
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        init_mpv(&mpv, &silent_config());
        mpv
    }

    #[test]
    fn init_applies_the_settings_playback_correctness_depends_on() {
        // Goal: these are silently ignored if mispelled, which is exactly how
        // demuxer-readahead-secs sat dead for a while. Read them back.
        let mpv = silent_mpv();
        assert_eq!(
            mpv.get_property::<String>("gapless-audio").unwrap(),
            "weak",
            "forcing `yes` would break bit-perfect rate switching"
        );
        assert_eq!(
            mpv.get_property::<i64>("demuxer-max-bytes").unwrap(),
            64 * 1024 * 1024
        );
        assert!((mpv.get_property::<f64>("cache-secs").unwrap() - 120.0).abs() < f64::EPSILON);
        assert!(
            !mpv.get_property::<bool>("cache-pause-initial").unwrap(),
            "playback must start as soon as it can decode, not once a buffer is full"
        );
        assert!(
            mpv.get_property::<bool>("cache-pause").unwrap(),
            "a slow link should pause cleanly rather than stutter"
        );
        assert!((mpv.get_property::<f64>("cache-pause-wait").unwrap() - 2.0).abs() < f64::EPSILON);
        assert_eq!(mpv.get_property::<String>("ao").unwrap(), "null");
        assert!(
            mpv.get_property::<String>("demuxer-seekable-cache")
                .unwrap()
                != "no",
            "the stream is served without a seek callback, so in-track seeking \
             depends entirely on mpv's cache"
        );
    }

    #[test]
    fn every_property_the_player_sets_is_accepted_by_mpv() {
        // Goal: a misspelled property or a wrong type is silently ignored by
        // libmpv, which is exactly how demuxer-readahead-secs sat here dead for
        // a while. This catches the whole set at once, in release builds too,
        // where the debug assertion this replaced did nothing.
        let lines = captured(|| {
            let mpv = Mpv::new().expect("mpv should initialise headlessly");
            init_mpv(&mpv, &silent_config());
        });
        assert!(
            !lines.iter().any(|l| l.contains("rejected the property")),
            "mpv rejected something: {lines:?}"
        );
    }

    #[test]
    fn the_startup_flag_reaches_mpv_and_is_off_without_it() {
        // Goal: --exclusive is the half of this that has no interactive
        // feedback until a track plays, so a request that never reached mpv
        // would look identical to one that was refused. And the default has to
        // be off: priel never takes a device on its own.
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        init_mpv(&mpv, &silent_config());
        assert!(
            !mpv.get_property::<bool>("audio-exclusive").unwrap_or(true),
            "no flag, no exclusivity"
        );

        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        init_mpv(
            &mpv,
            &PlayerConfig {
                audio_device: Some("null".into()),
                exclusive: true,
                ..PlayerConfig::default()
            },
        );
        assert!(
            mpv.get_property::<bool>("audio-exclusive").unwrap_or(false),
            "the flag has to reach mpv, not just priel's own state"
        );
    }

    #[test]
    fn a_property_mpv_rejects_is_recorded_by_name() {
        // Goal: the failure has no other symptom - the setting simply does not
        // apply - so the log line is the only way anyone finds out.
        let mpv = silent_mpv();
        let lines = captured(|| set_prop(&mpv, "not-a-real-property", "x"));
        assert!(
            lines.iter().any(|l| l.contains("not-a-real-property")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_command_mpv_rejects_is_recorded_by_name() {
        // Goal: every command here is fire-and-forget, so a rejected one leaves
        // the player quietly doing nothing at all.
        let mpv = silent_mpv();
        let lines = captured(|| command(&mpv, "not-a-real-command", &[]));
        assert!(
            lines.iter().any(|l| l.contains("not-a-real-command")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_segment_that_cannot_be_fetched_says_which_track_ends_short() {
        // Goal: a failed fetch ends the track early with no other symptom. The
        // user hears a song stop; without this nothing anywhere says why.
        let sh = shared(Vec::new(), false);
        let dead = "http://127.0.0.1:1/never".to_string();
        let lines = captured(|| {
            spawn_downloader(vec![dead], &sh);
            for _ in 0..200 {
                if lock(&sh.inner).complete {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            lock(&sh.inner).complete,
            "the buffer must still be released"
        );
        assert!(
            lines.iter().any(|l| l.contains("will end short")),
            "the failure should say the track was cut off: {lines:?}"
        );
    }

    #[test]
    fn a_file_mpv_cannot_load_is_reported_rather_than_looking_idle() {
        // Goal: nothing here ever read mpv's event queue, so a failed load was
        // indistinguishable from a player with nothing to do - the UI showed a
        // track that simply never started and no explanation existed anywhere.
        let lines = captured(|| {
            let mpv = silent_mpv();
            command(
                &mpv,
                "loadfile",
                &["/nonexistent/priel-not-a-file.flac", "replace"],
            );
            for _ in 0..200 {
                drain_events(&mpv, &registry(), &[], &mut no_output());
                if lock(&LINES).iter().any(|l: &String| l.contains("mpv")) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            lines.iter().any(|l| l.contains("mpv")),
            "the failed load should be recorded: {lines:?}"
        );
    }

    #[test]
    fn a_downloader_says_which_thread_it_is() {
        // Goal: the log's thread column is worth its width only if the threads
        // have names. A stalled download and a player that will not start are
        // read together, and they come from different threads.
        let sh = shared(Vec::new(), false);
        let lines = captured(|| {
            spawn_downloader(vec!["http://127.0.0.1:1/never".into()], &sh);
            for _ in 0..200 {
                if lock(&sh.inner).complete {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            lines.iter().any(|l| l.starts_with("segments|")),
            "the downloader should be named: {lines:?}"
        );
    }

    #[test]
    fn the_player_thread_says_which_thread_it_is() {
        // Goal: as above, for the thread that owns mpv. Seeking an idle player
        // is declined by mpv, which is a line the player thread writes itself.
        let lines = captured(|| {
            let player = crate::Player::new(Some("null".into())).expect("player");
            player.seek(10.0);
            for _ in 0..200 {
                if lock(&LINES)
                    .iter()
                    .any(|l: &String| l.starts_with("player|"))
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            lines.iter().any(|l| l.starts_with("player|")),
            "the player thread should be named: {lines:?}"
        );
    }

    #[test]
    fn draining_an_idle_queue_costs_nothing_and_says_nothing() {
        // Goal: this runs on every tick of the player loop, so the common case
        // must be a cheap no-op that does not fill the log with noise.
        let lines = captured(|| {
            let mpv = silent_mpv();
            drain_events(&mpv, &registry(), &[], &mut no_output());
            drain_events(&mpv, &registry(), &[], &mut no_output());
        });
        // Named exactly: the buffer is shared with whatever else is running,
        // and "mpv event" is the catch-all arm that the deprecated events used
        // to trip. Nothing else in the tree emits it.
        assert!(
            !lines.iter().any(|l| l.contains("mpv event")),
            "an idle queue should be silent: {lines:?}"
        );
    }

    #[test]
    fn mpvs_own_log_arrives_in_priels() {
        // Goal: what mpv makes of a track is half the story of a playback bug,
        // and it used to be in a second file with its own timestamps. Asking
        // mpv for its messages puts both halves in one place, in order.
        let lines = captured(|| {
            let mpv = Mpv::new().expect("mpv should initialise headlessly");
            init_mpv(
                &mpv,
                &PlayerConfig {
                    audio_device: Some("null".into()),
                    mpv_log_level: Some("info".into()),
                    ..PlayerConfig::default()
                },
            );
            command(
                &mpv,
                "loadfile",
                &["/nonexistent/priel-not-a-file.flac", "replace"],
            );
            for _ in 0..200 {
                drain_events(&mpv, &registry(), &[], &mut no_output());
                if lock(&LINES)
                    .iter()
                    .any(|l: &String| l.contains("priel::mpv"))
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        assert!(
            lines.iter().any(|l| l.contains("priel::mpv")),
            "mpv's own messages should be recorded: {lines:?}"
        );
    }

    #[test]
    fn mpvs_levels_are_mapped_onto_priels() {
        // Goal: mpv has two levels priel does not ("v", "fatal"), and an
        // unmapped one would silently land at the wrong severity - the thing a
        // reader scans the log by.
        assert_eq!(mpv_level("fatal"), log::Level::Error);
        assert_eq!(mpv_level("error"), log::Level::Error);
        assert_eq!(mpv_level("warn"), log::Level::Warn);
        assert_eq!(mpv_level("info"), log::Level::Info);
        assert_eq!(mpv_level("v"), log::Level::Debug);
        assert_eq!(mpv_level("debug"), log::Level::Debug);
        assert_eq!(mpv_level("trace"), log::Level::Trace);
        assert_eq!(
            mpv_level("something new"),
            log::Level::Info,
            "an unknown level still shows"
        );
    }

    #[test]
    fn a_device_string_selects_that_output() {
        // Goal: `--device` is how a user reaches their DAC; it must become
        // audio-device rather than being swallowed.
        let mpv = Mpv::new().expect("mpv");
        init_mpv(
            &mpv,
            &PlayerConfig {
                audio_device: Some("pipewire/some.dac".into()),
                ..PlayerConfig::default()
            },
        );
        assert_eq!(
            mpv.get_property::<String>("audio-device").unwrap(),
            "pipewire/some.dac"
        );
    }

    #[test]
    fn the_devices_mpv_knows_carry_a_name_and_a_description() {
        // Goal: both the picker and --list-devices need an identifier to hand
        // back to mpv and something a person can recognise it by. Enumeration
        // reads a property and opens no output, so it is safe headlessly.
        let mpv = silent_mpv();
        let devices = read_devices(&mpv);
        assert!(!devices.is_empty(), "mpv reports at least its own default");
        assert!(
            devices.iter().any(|d| d.name == "auto"),
            "the automatic choice is always offered: {devices:?}"
        );
        assert!(
            devices.iter().all(|d| !d.name.is_empty()),
            "a nameless device could never be selected: {devices:?}"
        );
    }

    #[test]
    fn the_published_list_carries_the_hardware_devices_the_kernel_names() {
        // Goal: this is the one place the published list is built, so it is the
        // one place the constructed entries have to arrive - miss it and the
        // picker is back to offering everything except the direct path.
        //
        // Both sides of the check depend on the host, and deliberately so: a
        // machine with no /proc/asound names no cards and nothing extra is the
        // right answer, and a player with no ALSA output would reject the
        // identifiers, so they are withheld there too. On a Linux machine with
        // a sound card this asserts the whole wiring.
        let mpv = silent_mpv();
        let devices = read_devices(&mpv);
        if !devices.iter().any(|d| d.name.starts_with("alsa/")) {
            return;
        }
        for card in crate::hw::card_devices() {
            assert!(
                devices.iter().any(|d| d.name == card.name),
                "{} should be offered: {devices:?}",
                card.name
            );
        }
    }

    #[test]
    fn enumerating_devices_needs_no_player() {
        // Goal: --list-devices runs before anything else exists, so the list has
        // to be reachable without starting a player thread.
        let devices = crate::audio_devices();
        assert!(devices.iter().any(|d| d.name == "auto"), "{devices:?}");
    }

    #[test]
    fn a_device_identifier_names_the_driver_that_has_to_be_used_with_it() {
        // Goal: --ao pins the driver, and priel pins it to pipewire when no
        // device was given. Setting audio-device alone is then silently ignored
        // for every alsa and pulse device the picker offers - measured: the
        // property took the new value and current-ao never changed.
        assert_eq!(device_driver("alsa/pipewire"), "alsa");
        assert_eq!(device_driver("pipewire/some.dac"), "pipewire");
        assert_eq!(device_driver("pipewire"), "pipewire");
        assert_eq!(
            device_driver("auto"),
            "",
            "mpv's driver list spells no preference as empty, not as the word"
        );
        assert_eq!(
            device_driver("alsa/hw:CARD=X,DEV=0"),
            "alsa",
            "only the first slash separates the driver"
        );
    }

    #[test]
    fn both_halves_of_a_switch_are_accepted_by_mpv() {
        // Goal: a property mpv rejects is silently ignored, so a misspelling
        // here would make every choice in the picker a no-op with no symptom
        // but silence. Nothing is loaded, so no output is opened by this.
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        let lines = captured(|| {
            apply_device(&mpv, "auto");
            apply_device(&mpv, "pipewire/some.dac");
        });
        assert!(
            !lines.iter().any(|l| l.contains("rejected the property")),
            "mpv rejected something: {lines:?}"
        );
        assert_eq!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            "pipewire/some.dac"
        );
    }

    #[test]
    fn a_device_that_is_not_there_is_refused_before_it_can_silence_anything() {
        // Goal: a device can be unplugged between the picker opening and a row
        // being chosen. mpv accepts any string for audio-device and only fails
        // later, by which time the track has stopped - so the name is checked
        // against the list first and the output is left exactly as it was.
        let mpv = silent_mpv();
        let mut output = no_output();
        *lock(&output.devices) = vec![AudioDevice {
            name: "auto".into(),
            description: "Autoselect device".into(),
        }];

        let lines = captured(|| {
            set_device(&mpv, &mut output, "pipewire/one-that-left".into());
        });

        assert_eq!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            "auto",
            "the output must not have moved"
        );
        assert!(output.pending.is_none(), "there is nothing to judge later");
        let reported = output.error.clone().unwrap_or_default();
        assert!(
            reported.contains("pipewire/one-that-left"),
            "the user is told which device: {reported}"
        );
        assert!(
            lines.iter().any(|l| l.contains("one-that-left")),
            "and it is in the log too: {lines:?}"
        );
    }

    #[test]
    fn the_hardware_readout_follows_the_device_in_use_not_the_one_priel_started_with() {
        // Goal: the readout finds the card by matching the device identifier
        // against it, and the picker can move the output mid-session. Matching
        // against the startup device sends the readout to whichever card
        // happened to be open first - and the badge would then report another
        // application's card as priel's own.
        let mpv = silent_mpv();
        assert_eq!(
            hw_hint(&mpv, &silent_config()).as_deref(),
            Some("null"),
            "with no choice made, the device priel started with is the hint"
        );
        apply_device(&mpv, "alsa/hw:CARD=X,DEV=0");
        assert_eq!(
            hw_hint(&mpv, &silent_config()).as_deref(),
            Some("alsa/hw:CARD=X,DEV=0"),
            "once the output has moved, the hint moves with it"
        );
    }

    #[test]
    fn an_automatic_device_is_no_hint_at_all() {
        // Goal: `auto` names no device, so it must not be matched against a card
        // id - it would only ever match by accident.
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        apply_device(&mpv, "auto");
        assert_eq!(hw_hint(&mpv, &PlayerConfig::default()), None);
    }

    #[test]
    fn a_device_that_will_not_open_is_told_apart_from_an_idle_player() {
        // Goal: mpv does not fall back when the device it was given refuses to
        // open - it abandons the file and leaves no output open. A player with
        // nothing loaded also has no output open, and that is not a failure.
        assert!(
            switch_failed(true, false),
            "loaded with no output open is the failure"
        );
        assert!(
            !switch_failed(false, false),
            "an idle player has no output open and nothing is wrong"
        );
        assert!(!switch_failed(true, true), "playing on the new device");
        assert!(!switch_failed(false, true));
    }

    #[test]
    fn exclusivity_is_only_claimed_of_a_driver_that_can_give_it() {
        // Goal: mpv's audio-exclusive is honoured by a handful of drivers and
        // *silently ignored* by the rest, and ALSA has no such flag at all -
        // there the exclusivity is the device, because a card device admits one
        // opener while the server's own plugin is shared by construction.
        // Asking a driver that ignores the request and then reporting success
        // is the overstatement this indicator exists to avoid.
        assert!(honours_exclusive("pipewire/some.dac"));
        assert!(honours_exclusive("pipewire"));
        assert!(honours_exclusive("alsa/hw:CARD=AUDIO,DEV=0"));
        assert!(honours_exclusive("alsa/front:CARD=AUDIO,DEV=0"));
        assert!(honours_exclusive("alsa/hw:2,0"));
        assert!(
            !honours_exclusive("alsa/pipewire"),
            "the server's own ALSA device is shared however it is opened"
        );
        assert!(
            !honours_exclusive("pulse/alsa_output.usb-x"),
            "pulse ignores the request without saying so"
        );
        assert!(!honours_exclusive("auto"), "and so does an unmade choice");
    }

    #[test]
    fn nothing_is_claimed_until_the_output_is_actually_open() {
        // Goal: between asking and the device opening there is nothing being
        // held, and a refusal must read as shared however it came about. The
        // three answers are what the badge renders, so each has to be reachable.
        let dac = "alsa/hw:CARD=AUDIO,DEV=0";
        assert_eq!(
            output_access(true, false, dac, true),
            OutputAccess::Exclusive
        );
        assert_eq!(
            output_access(true, false, dac, false),
            OutputAccess::Shared,
            "asked for, not yet open: priel is holding nothing"
        );
        assert_eq!(
            output_access(false, false, dac, true),
            OutputAccess::Shared,
            "a hardware device on its own is not exclusive access"
        );
        assert_eq!(
            output_access(false, true, dac, true),
            OutputAccess::Refused,
            "a refusal outranks everything: the listener asked and did not get"
        );
    }

    #[test]
    fn mpv_accepts_being_asked_for_the_device_exclusively() {
        // Goal: a misspelled property is silently ignored, so the whole feature
        // would be a no-op with no symptom. Nothing is loaded, so this opens no
        // output.
        let mpv = silent_mpv();
        let mut output = no_output();
        let lines = captured(|| {
            set_exclusive(&mpv, &mut output, true);
        });
        assert!(
            !lines.iter().any(|l| l.contains("rejected the property")),
            "mpv rejected something: {lines:?}"
        );
        assert!(
            mpv.get_property::<bool>("audio-exclusive").unwrap_or(false),
            "the request has to reach mpv, not just priel's own state"
        );
        assert!(output.exclusive, "and priel has to remember it asked");
    }

    #[test]
    fn a_refused_exclusive_open_goes_back_to_sharing_and_says_so() {
        // Goal: mpv does not fall back when a device will not open - it
        // abandons the file - so priel gives the exclusivity back itself and
        // keeps the music playing. Nothing is loaded here and the output is
        // therefore not open, which is exactly the symptom of a refusal.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.exclusive = true;
        output.pending = Some(Switch {
            previous: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            previous_exclusive: false,
            requested: "alsa/hw:CARD=AUDIO,DEV=0".into(),
            exclusive: true,
            deadline: Instant::now(),
        });

        let entries = vec![entry(7)];
        let lines = captured(|| settle_switch(&mpv, &registry(), &entries, &mut output));

        assert!(!output.exclusive, "the request is dropped, not retried");
        assert!(output.refused, "and remembered, so the badge can say so");
        assert!(
            !mpv.get_property::<bool>("audio-exclusive").unwrap_or(true),
            "mpv has to be told too, or the next track fails the same way"
        );
        let reported = output.error.clone().unwrap_or_default();
        assert!(
            reported.contains("exclusive"),
            "the notice names what was refused: {reported}"
        );
        assert!(
            lines.iter().any(|l| l.contains("exclusive")),
            "and it is in the diagnostic log too: {lines:?}"
        );
        assert_eq!(
            output_access(output.exclusive, output.refused, "alsa/hw:2,0", true),
            OutputAccess::Refused,
            "the badge must report shared output, never the exclusivity asked for"
        );
    }

    #[test]
    fn a_request_made_before_anything_plays_waits_for_a_track_to_judge_it() {
        // Goal: --exclusive is given before there is anything to play, and an
        // idle player has no output open - which looks exactly like a refusal.
        // Judging it there would drop the request before it was ever tested.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.exclusive = true;
        output.judged = true;
        output.pending = Some(Switch {
            previous: "auto".into(),
            previous_exclusive: false,
            requested: "auto".into(),
            exclusive: true,
            deadline: Instant::now(),
        });

        settle_switch(&mpv, &registry(), &[], &mut output);

        assert!(
            output.exclusive,
            "nothing was tested, so nothing is given up"
        );
        assert!(!output.refused);
        assert!(
            !output.judged,
            "the first track to play has to put it to the test"
        );
    }

    /// The sinks the recorded dump carries, so the mapping is exercised against
    /// a real one rather than a hand-written pair of strings.
    fn fixture_sinks() -> Vec<ServerSink> {
        graph::parse_sinks(include_str!("../tests/fixtures/pw-dump-usb-dac.json"))
    }

    #[test]
    fn a_refused_hardware_device_falls_back_to_the_same_card_through_the_server() {
        // Goal: the listener asked for one physical DAC, so the nearest thing
        // to what they wanted is that same DAC shared rather than some other
        // device. The two identifiers share no substring, so the card the
        // server publishes is the only thing that can pair them.
        let sinks = fixture_sinks();
        assert_eq!(
            shared_fallback("alsa/hw:CARD=AUDIO,DEV=0", &sinks),
            "pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0"
        );
        assert_eq!(
            shared_fallback("alsa/hw:2,0", &sinks),
            "pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0",
            "named by index or by id, it is the same card"
        );
    }

    #[test]
    fn a_hardware_device_the_server_does_not_front_falls_through_to_the_default() {
        // Goal: there is no shared spelling of a `hw:` device - the card is the
        // whole of it - so with nothing to map onto, the only honest answer is
        // whatever the system would have played through anyway.
        assert_eq!(
            shared_fallback("alsa/hw:CARD=NOSUCHCARD,DEV=0", &fixture_sinks()),
            "auto"
        );
        assert_eq!(
            shared_fallback("alsa/hw:CARD=AUDIO,DEV=0", &[]),
            "auto",
            "and a machine with no sound server at all has no sinks to offer"
        );
    }

    #[test]
    fn a_device_the_server_owns_is_its_own_fallback() {
        // Goal: when the sound server refused to give up a device it holds, the
        // same device shared is exactly what is wanted - giving the exclusivity
        // back is the whole of the fallback, and moving the audio elsewhere
        // would be a surprise nobody asked for.
        let sinks = fixture_sinks();
        assert_eq!(
            shared_fallback("pipewire/alsa_output.usb-x", &sinks),
            "pipewire/alsa_output.usb-x"
        );
        assert_eq!(shared_fallback("auto", &sinks), "auto");
    }

    #[test]
    fn a_buffer_can_only_be_replayed_while_it_still_holds_its_own_beginning() {
        // Goal: reloading a track plays it from the start, and `trim` releases
        // what the reader has gone past. Serving a stream whose first bytes are
        // gone would fail the same way the refusal did, with a different cause.
        let sh = shared(vec![7; 64], false);
        lock(&sh.inner).read_pos = 40;
        assert!(rewind(&sh), "nothing has been released yet");
        assert_eq!(
            lock(&sh.inner).read_pos,
            0,
            "or the first read would trim what it is about to serve"
        );

        let gone = shared(vec![7; 64], false);
        lock(&gone.inner).base = 8;
        assert!(!rewind(&gone), "the beginning is no longer obtainable");

        let dead = shared(vec![7; 64], false);
        lock(&dead.inner).aborted = true;
        assert!(!rewind(&dead), "and an abandoned stream serves nothing");
    }

    #[test]
    fn a_refused_hardware_device_is_left_behind_and_the_track_is_reloaded() {
        // Goal: the regression that hardware testing found. Giving the
        // exclusivity back and reapplying the *same* `hw:` device changes
        // nothing - an ALSA card device is exclusive by construction, and the
        // card is still held by whatever refused it - so priel has to move to a
        // different output altogether. And mpv abandons the file when the
        // output will not open, so restoring a working device on its own leaves
        // the interface buffering for a track that is no longer loaded.
        //
        // This reads the sound-server graph, which is a read-only subprocess
        // and opens nothing; the card named here exists nowhere, so nothing is
        // asserted about what it answers.
        let mpv = silent_mpv();
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut output = no_output();
        let refused = "alsa/hw:CARD=NOSUCHCARD,DEV=0";
        output.exclusive = true;
        apply_device(&mpv, refused);
        output.pending = Some(Switch {
            previous: refused.into(),
            previous_exclusive: false,
            requested: refused.into(),
            exclusive: true,
            deadline: Instant::now(),
        });

        let entries = vec![Entry {
            id: 7,
            seq: None,
            source: "http://127.0.0.1:1/a".into(),
        }];
        settle_switch(&mpv, &registry, &entries, &mut output);

        assert_ne!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            refused,
            "falling back onto the device that just refused is not a fallback"
        );
        assert_eq!(
            mpv.get_property::<i64>("playlist-count").unwrap_or(0),
            1,
            "the abandoned track has to be loaded again or nothing ever resumes"
        );
    }

    #[test]
    fn the_first_track_to_play_is_what_puts_a_standing_request_to_the_test() {
        // Goal: --exclusive is given before there is anything to play, so the
        // only thing that ever opens the device is a track. Arming it there is
        // what stops the session going silent when the device is already held -
        // and arming it only once is what stops a slow-starting later track
        // condemning an exclusivity that is working.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.exclusive = true;

        arm_exclusive_check(&mpv, &mut output);
        assert!(
            output.pending.is_some(),
            "the first load arms the judgement"
        );
        assert!(output.judged);
        let deadline = output.pending.as_ref().map(|s| s.deadline);
        assert!(
            deadline.is_some_and(|d| d > Instant::now() + SWITCH_GRACE),
            "and gives the track longer than a reinit, because nothing is open yet"
        );

        output.pending = None;
        arm_exclusive_check(&mpv, &mut output);
        assert!(
            output.pending.is_none(),
            "a later track must not re-arm what has already been tested"
        );

        let mut shared = no_output();
        arm_exclusive_check(&mpv, &mut shared);
        assert!(
            shared.pending.is_none(),
            "and nothing is armed when nothing was asked for"
        );
    }

    /// A pending exclusive request whose deadline is far enough away that only
    /// an event can settle it.
    fn awaiting_a_verdict(device: &str) -> Switch {
        Switch {
            previous: device.into(),
            previous_exclusive: false,
            requested: device.into(),
            exclusive: true,
            deadline: Instant::now() + Duration::from_secs(3600),
        }
    }

    /// Drain mpv's events until `done`, or until patience runs out.
    fn drain_until(
        mpv: &Mpv,
        registry: &Registry,
        entries: &[Entry],
        output: &mut Output,
        done: impl Fn(&Output) -> bool,
    ) {
        for _ in 0..200 {
            drain_events(mpv, registry, entries, output);
            if done(output) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_failed_load_settles_a_refusal_without_waiting_for_the_deadline() {
        // Goal: the deadline is eight seconds, and hardware testing heard every
        // one of them as silence with a buffering indicator over it. mpv says
        // outright that it gave up on the file - that arrives as an error from
        // `wait_event` and is the only place a failed load is ever visible - so
        // the recovery starts there and the deadline is only the backstop.
        let mpv = silent_mpv();
        let registry = registry();
        let mut output = no_output();
        let refused = "alsa/hw:CARD=NOSUCHCARD,DEV=0";
        output.exclusive = true;
        apply_device(&mpv, refused);
        output.pending = Some(awaiting_a_verdict(refused));

        let entries = vec![Entry {
            id: 7,
            seq: None,
            source: "/nonexistent/priel-not-a-file.flac".into(),
        }];
        command(&mpv, "loadfile", &[&entries[0].source, "replace"]);
        drain_until(&mpv, &registry, &entries, &mut output, |o| o.refused);

        assert!(
            output.refused,
            "the event has to settle it; the deadline is an hour away"
        );
        assert_ne!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            refused,
            "and the fallback has to have happened, not just been recorded"
        );
    }

    #[test]
    fn a_reload_that_fails_again_never_settles_a_second_time() {
        // Goal: the reload loads a track that may fail again, which produces
        // another failure event - and settling on that would reload again,
        // forever. The guard is that judging a change consumes it: a failure
        // arriving with nothing pending is a track's own problem and moves no
        // device. Asserted by counting, because reasoning about it is exactly
        // what the queue-advance fallback shows to be unreliable.
        // The card is named uniquely so the count cannot pick up a line another
        // test wrote: the capture buffer is global, and a test driving a live
        // mpv logs into whichever capture happens to be open.
        let refused = "alsa/hw:CARD=PRIELLOOPGUARD,DEV=0";
        let mut left_pending = None;
        let lines = captured(|| {
            let mpv = silent_mpv();
            let registry = registry();
            let mut output = no_output();
            output.exclusive = true;
            output.pending = Some(awaiting_a_verdict(refused));
            // A file that cannot load however often it is tried, so every
            // reload produces another failure event to settle on.
            let entries = vec![Entry {
                id: 7,
                seq: None,
                source: "/nonexistent/priel-not-a-file.flac".into(),
            }];
            command(&mpv, "loadfile", &[&entries[0].source, "replace"]);
            for _ in 0..100 {
                drain_events(&mpv, &registry, &entries, &mut output);
                thread::sleep(Duration::from_millis(5));
            }
            left_pending = Some(output.pending.is_some());
        });

        let settled = lines
            .iter()
            .filter(|l| l.contains("PRIELLOOPGUARD"))
            .count();
        assert_eq!(settled, 1, "one refusal, however many times it fails");
        assert_eq!(
            left_pending,
            Some(false),
            "a change left armed is a reload waiting to happen again"
        );
    }

    #[test]
    fn a_track_that_fails_with_the_output_open_leaves_the_device_alone() {
        // Goal: a failed load is not proof that a device refused. A 404, a
        // corrupt stream and an aborted buffer all arrive the same way, and
        // what tells them apart is whether there is an output open - which is
        // the same question a device change is already judged by. A bad track
        // must cost nothing but itself.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.exclusive = true;
        apply_device(&mpv, "pipewire/some.dac");
        output.pending = Some(awaiting_a_verdict("pipewire/some.dac"));

        settle_now(&mpv, &registry(), &[entry(3)], &mut output, true);

        assert!(!output.refused, "the device did what it was asked");
        assert!(output.exclusive, "so the request stands");
        assert_eq!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            "pipewire/some.dac",
            "and nothing moved"
        );
        assert!(
            output.pending.is_none(),
            "the change is judged and done with either way"
        );
    }

    #[test]
    fn an_ordinary_device_change_that_fails_also_reloads_the_track_it_cost() {
        // Goal: the same bug on the other branch. mpv abandons the file
        // whenever an output will not open, and that is not particular to an
        // exclusive request - so restoring the previous device leaves the
        // interface waiting for a track that is no longer loaded, exactly as a
        // refused exclusive open did.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.pending = Some(Switch {
            previous: "auto".into(),
            previous_exclusive: false,
            requested: "pipewire/one-that-left".into(),
            exclusive: false,
            deadline: Instant::now(),
        });

        settle_switch(&mpv, &registry(), &[entry(3)], &mut output);

        assert_eq!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            "auto",
            "the previous device is restored, as it always was"
        );
        assert_eq!(
            mpv.get_property::<i64>("playlist-count").unwrap_or(0),
            1,
            "and the track it cost is loaded again, or nothing resumes"
        );
    }

    #[test]
    fn a_plain_device_change_that_fails_leaves_exclusivity_where_it_was() {
        // Goal: the two are orthogonal. A device that will not open says
        // nothing about whether exclusivity was available, so restoring the
        // previous device must not also invent a refusal.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.pending = Some(Switch {
            previous: "auto".into(),
            previous_exclusive: false,
            requested: "pipewire/one-that-left".into(),
            exclusive: false,
            deadline: Instant::now(),
        });

        settle_switch(&mpv, &registry(), &[entry(1)], &mut output);

        assert!(
            !output.refused,
            "no exclusivity was asked for, so none was refused"
        );
        assert_eq!(
            mpv.get_property::<String>("audio-device")
                .unwrap_or_default(),
            "auto",
            "the previous device is restored, as it always was"
        );
    }

    #[test]
    fn a_change_is_judged_only_once_mpv_has_had_time_to_make_it() {
        // Goal: the reinit happens on mpv's own playloop, so judging it the
        // instant the property is set would call every switch a failure.
        let mpv = silent_mpv();
        let mut output = no_output();
        output.pending = Some(Switch {
            previous: "auto".into(),
            previous_exclusive: false,
            requested: "pipewire/some.dac".into(),
            exclusive: false,
            deadline: Instant::now() + Duration::from_secs(30),
        });
        settle_switch(&mpv, &registry(), &[], &mut output);
        assert!(
            output.pending.is_some(),
            "it must still be waiting for the deadline"
        );
        assert!(output.error.is_none());
    }

    #[test]
    fn status_reflects_an_idle_player() {
        // Goal: the UI derives "buffering" and the end-of-track fallback from
        // this snapshot, so an idle player must report honestly rather than
        // looking like a loaded track at position zero.
        let mpv = silent_mpv();
        let st = read_status(&mpv, &[], None, &no_output());
        assert!(!st.loaded);
        assert!(!st.playing);
        assert_eq!(st.current_id, 0, "no entry means no track id");
        assert!(!st.has_next);
        assert!((st.volume - 100.0).abs() < 1.0);
    }

    #[test]
    fn status_tracks_the_head_of_the_queue() {
        // Goal: current_id drives the gapless hand-off in the app, and has_next
        // tells it whether a preload already exists.
        let mpv = silent_mpv();
        let entries = vec![entry(11), entry(22)];
        let st = read_status(&mpv, &entries, None, &no_output());
        assert_eq!(st.current_id, 11);
        assert!(st.has_next);
    }

    #[test]
    fn commands_mutate_the_player_and_quit_stops_the_thread() {
        // Goal: the command enum is the whole public surface of the backend.
        // Volume is the one with an observable value without real playback.
        let mpv = silent_mpv();
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut entries = Vec::new();
        let mut seq = 0;

        let mut output = no_output();
        assert!(!handle_cmd(
            &mpv,
            &reg,
            &mut entries,
            &mut seq,
            &mut output,
            Cmd::SetVolume(42.0)
        ));
        assert!((mpv.get_property::<f64>("volume").unwrap() - 42.0).abs() < f64::EPSILON);

        for cmd in [
            Cmd::SetPaused(true),
            Cmd::Seek(1.0),
            Cmd::SeekRelative(-1.0),
            Cmd::Next,
            Cmd::Stop,
        ] {
            assert!(!handle_cmd(
                &mpv,
                &reg,
                &mut entries,
                &mut seq,
                &mut output,
                cmd
            ));
        }

        assert!(
            handle_cmd(&mpv, &reg, &mut entries, &mut seq, &mut output, Cmd::Quit),
            "only Quit ends the loop"
        );
    }

    #[test]
    fn the_pause_state_is_set_to_what_was_asked_for_rather_than_cycled() {
        // Goal: a remote control says "pause", not "the other one". Answering an
        // absolute with a cycle pauses a playing track when a panel applet's
        // play button is pressed twice, which is the standard version of this
        // bug. Method: ask for the same state twice and read the property back -
        // a cycle would put it back where it started.
        let mpv = silent_mpv();
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut entries = Vec::new();
        let mut seq = 0;
        let mut output = no_output();
        let mut set = |paused: bool| {
            handle_cmd(
                &mpv,
                &reg,
                &mut entries,
                &mut seq,
                &mut output,
                Cmd::SetPaused(paused),
            );
            mpv.get_property::<bool>("pause")
        };

        assert_eq!(set(true).ok(), Some(true));
        assert_eq!(set(true).ok(), Some(true), "asking twice is not a cycle");
        assert_eq!(set(false).ok(), Some(false));
        assert_eq!(set(false).ok(), Some(false));
    }

    #[test]
    fn loading_replaces_the_queue_and_appending_extends_it() {
        // Goal: `entries` must stay in lockstep with mpv's playlist; Load starts
        // fresh, Append preloads. Getting this wrong desynchronises current_id.
        let mpv = silent_mpv();
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut entries = Vec::new();
        let mut seq = 0;
        let src = || PlayableSource::Segments(vec!["http://127.0.0.1:1/x".into()]);
        let mut output = no_output();

        handle_cmd(
            &mpv,
            &reg,
            &mut entries,
            &mut seq,
            &mut output,
            Cmd::Load(1, src()),
        );
        assert_eq!(entries.len(), 1);
        handle_cmd(
            &mpv,
            &reg,
            &mut entries,
            &mut seq,
            &mut output,
            Cmd::Append(2, src()),
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[1].id, 2);

        handle_cmd(
            &mpv,
            &reg,
            &mut entries,
            &mut seq,
            &mut output,
            Cmd::Load(3, src()),
        );
        assert_eq!(entries.len(), 1, "Load replaces rather than appends");
        assert_eq!(entries[0].id, 3);
    }

    #[test]
    fn cleanup_is_a_no_op_while_the_first_entry_is_still_playing() {
        // Goal: it may only drop entries mpv has moved past. Dropping the head
        // would free the buffer of the track being played.
        let mpv = silent_mpv();
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut entries = vec![entry(1)];
        cleanup_playlist(&mpv, &reg, &mut entries);
        assert_eq!(entries.len(), 1);
    }

    /// Serves a body in two halves with a pause between them, so a test can tell
    /// "published as it arrived" from "published when it finished".
    fn trickle(first: &'static [u8], second: &'static [u8], gap: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/seg", listener.local_addr().expect("addr"));
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 S\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    first.len() + second.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(first);
                let _ = sock.flush();
                thread::sleep(gap);
                let _ = sock.write_all(second);
            }
        });
        url
    }

    #[test]
    fn audio_becomes_readable_before_the_segment_has_finished() {
        // Goal: the whole reason playback starts promptly. A hi-res segment is
        // megabytes; publishing it only once complete meant mpv had nothing to
        // decode until the entire thing landed, which is seconds of silence on
        // every track change. The first half must be readable while the second
        // is still in flight.
        let url = trickle(b"first-half", b"second-half", Duration::from_millis(400));
        let sh = shared(Vec::new(), false);
        spawn_downloader(vec![url], &sh);

        // A reader blocked on an empty buffer must be released by the first
        // chunk, well before the response completes.
        let mut c = cookie(&sh);
        let mut buf = [0i8; 64];
        let started = std::time::Instant::now();
        let n = read(&mut c, &mut buf);
        let waited = started.elapsed();

        assert!(n > 0, "the first chunk should have woken the reader");
        assert!(
            waited < Duration::from_millis(300),
            "waited {waited:?}, which means it held the bytes back until the end"
        );
        assert!(
            !lock(&sh.inner).complete,
            "the segment should still be arriving"
        );
    }

    #[test]
    fn a_trickled_segment_still_arrives_whole() {
        // Goal: chunking must not drop or reorder anything - the concatenated
        // bytes are a FLAC stream and a gap in the middle is a decode failure.
        let url = trickle(b"AAAA", b"BBBB", Duration::from_millis(50));
        let sh = shared(Vec::new(), false);
        spawn_downloader(vec![url], &sh);
        for _ in 0..200 {
            if lock(&sh.inner).complete {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let g = lock(&sh.inner);
        assert_eq!(g.data, b"AAAABBBB");
        assert_eq!(g.total, Some(8));
    }

    #[test]
    fn aborting_mid_segment_stops_the_download_there() {
        // Goal: skipping a track while a segment is still streaming must release
        // the thread rather than let it keep filling a discarded buffer.
        let url = trickle(b"early", b"late", Duration::from_secs(3));
        let sh = shared(Vec::new(), false);
        spawn_downloader(vec![url], &sh);
        for _ in 0..200 {
            if !lock(&sh.inner).data.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        abort(&sh);
        let g = lock(&sh.inner);
        assert!(g.aborted);
        assert!(
            g.data.is_empty(),
            "the discarded buffer is released at once"
        );
    }

    #[test]
    fn the_publish_size_starts_small_and_settles_large() {
        // Goal: the two ends of the trade. The first chunk decides how soon a
        // track makes a sound; the steady state should not read in 16 KiB bites
        // for the rest of a hi-res track.
        assert_eq!(
            next_chunk(SEGMENT_CHUNK_MIN),
            32 * 1024,
            "it should grow, not stay at the start size"
        );
        let mut size = SEGMENT_CHUNK_MIN;
        for _ in 0..20 {
            size = next_chunk(size);
        }
        assert_eq!(size, SEGMENT_CHUNK_MAX, "and settle at the ceiling");
        assert_eq!(next_chunk(SEGMENT_CHUNK_MAX), SEGMENT_CHUNK_MAX);
        assert_eq!(
            next_chunk(usize::MAX),
            SEGMENT_CHUNK_MAX,
            "the doubling must not overflow"
        );
    }

    // ---- what mpv asks of a stream it cannot seek ----
    //
    // Whether bytes below the reader are reachable at all decides whether they
    // can be released while a track plays. These four statics are what the
    // instrumented callbacks below record; only `probe_backward_seek` uses them,
    // and it takes `READING` so its two scenarios cannot interleave and its own
    // event draining does not land in another test's capture.

    static OPENS: Mutex<usize> = Mutex::new(0);
    /// Each seek mpv asked for, with how far the reader had got at the time.
    static SEEKS: Mutex<Vec<(i64, u64)>> = Mutex::new(Vec::new());
    static HIGH_WATER: Mutex<u64> = Mutex::new(0);
    static BACKWARD_READS: Mutex<usize> = Mutex::new(0);

    /// `open`, counting how many times mpv opens the stream.
    ///
    /// A reopen would start a fresh cookie at offset zero, which is the one way
    /// mpv could still want a byte the reader has passed.
    fn watching_open(reg: &mut Registry, uri: &str) -> Cookie {
        *lock(&OPENS) += 1;
        open(reg, uri)
    }

    /// `read`, recording every offset a read starts at.
    fn watching_read(cookie: &mut Cookie, buf: &mut [i8]) -> i64 {
        {
            let mut high = lock(&HIGH_WATER);
            if cookie.pos < *high {
                *lock(&BACKWARD_READS) += 1;
            }
            *high = (*high).max(cookie.pos);
        }
        read(cookie, buf)
    }

    /// A seek callback that records what was asked and then declines it.
    ///
    /// It returns the *same* error libmpv2's wrapper returns when no seek
    /// callback is registered at all, so mpv sees exactly the production stream
    /// while the test learns which offsets it would have wanted. Registering a
    /// working one is what indexes the whole file before playing a note; see
    /// `spawn`.
    fn recording_seek(_cookie: &mut Cookie, offset: i64) -> i64 {
        let reached = *lock(&HIGH_WATER);
        lock(&SEEKS).push((offset, reached));
        i64::from(libmpv2::mpv_error::Unsupported)
    }

    /// What mpv asked of the stream during one probe.
    struct Asked {
        opens: usize,
        seeks: Vec<(i64, u64)>,
        backward_reads: usize,
        went_back: bool,
    }

    /// Did mpv ever ask the stream for an offset the reader had already passed?
    fn asked_to_go_back(a: &Asked) -> bool {
        a.seeks
            .iter()
            .any(|(offset, reached)| u64::try_from(*offset).is_ok_and(|o| o < *reached))
    }

    /// A 16-bit PCM WAV of exactly `seconds` seconds, so a test needs no media
    /// file and knows what every byte offset in it means.
    ///
    /// WAV because it demuxes from a stream that cannot seek and its size is
    /// exactly `rate * channels * 2` per second, which is what makes the cache
    /// sizes the tests choose mean something.
    fn wav_bytes(rate: u32, channels: u32, seconds: u32) -> Vec<u8> {
        let per_sec = rate * channels * 2;
        // One second of samples, then repeated: a track long enough to overrun
        // the resident ceiling is tens of millions of samples, and building each
        // one costs more than the test is worth.
        let mut second = Vec::with_capacity(per_sec as usize);
        for i in 0..rate * channels {
            // Not silence: a decoder is free to shortcut a stream of zeroes.
            let s = i16::try_from(i % 2000).unwrap_or(0) * 8;
            second.extend_from_slice(&s.to_le_bytes());
        }
        let data_len = per_sec * seconds;
        let mut w = Vec::with_capacity(data_len as usize + 44);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&u16::try_from(channels).unwrap_or(1).to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&per_sec.to_le_bytes()); // bytes per second
        w.extend_from_slice(&u16::try_from(channels * 2).unwrap_or(2).to_le_bytes()); // block align
        w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for _ in 0..seconds {
            w.extend_from_slice(&second);
        }
        w
    }

    /// Play a stream that cannot seek, seek backwards in it, and report
    /// everything mpv asked of the stream while doing so.
    ///
    /// The cache sizes are parameters because the answer depends on them: with a
    /// cache big enough, mpv serves a backward seek from its own copy and never
    /// looks at the stream, which proves nothing about a cache that has run out.
    fn probe_backward_seek(cache_bytes: i64, back_bytes: i64) -> Asked {
        let _turn = lock(&READING);
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        init_mpv(&mpv, &silent_config());
        set_prop(&mpv, "demuxer-max-bytes", cache_bytes);
        set_prop(&mpv, "demuxer-max-back-bytes", back_bytes);

        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        // Deliberately not complete: `size` then reports -1, which is what mpv
        // sees for a real track, because the stream is opened while the download
        // is still running.
        let sh = shared(wav_bytes(44_100, 1, 30), false);
        lock(&registry).insert(1, sh.clone());

        *lock(&OPENS) = 0;
        lock(&SEEKS).clear();
        *lock(&HIGH_WATER) = 0;
        *lock(&BACKWARD_READS) = 0;

        // SAFETY: same contract as the player thread; see `spawn`.
        let protocol = unsafe {
            Protocol::new(
                &mpv,
                "prielseg".into(),
                registry.clone(),
                watching_open,
                close,
                watching_read,
                Some(recording_seek),
                Some(size),
            )
        };
        protocol.register().expect("the protocol should register");
        // Never dropped, for the reason in `spawn`: dropping it frees the
        // callback data while mpv can still call back into it.
        std::mem::forget(protocol);

        command(&mpv, "loadfile", &["prielseg://1", "replace"]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.5 {
                break;
            }
            drain_events(&mpv, &registry, &[], &mut no_output());
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.5,
            "the probe never started playing"
        );

        command(&mpv, "seek", &["0.05", "absolute"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut went_back = false;
        while Instant::now() < deadline {
            if mpv.get_property::<f64>("time-pos").unwrap_or(1.0) < 0.4 {
                went_back = true;
                break;
            }
            drain_events(&mpv, &registry, &[], &mut no_output());
            thread::sleep(Duration::from_millis(10));
        }
        command(&mpv, "stop", &[]);
        abort(&sh);

        Asked {
            opens: *lock(&OPENS),
            seeks: lock(&SEEKS).clone(),
            backward_reads: *lock(&BACKWARD_READS),
            went_back,
        }
    }

    #[test]
    fn mpv_never_reads_a_byte_it_has_already_gone_past() {
        // Goal: settle whether bytes below the reader are reachable, because
        // releasing them while a track plays depends entirely on that answer.
        // Two ways mpv could reach back: seeking the stream, which it cannot do
        // (no seek callback is registered, so the offsets recorded here are only
        // ever refused), or reopening it, which would restart at offset zero.
        //
        // The second scenario is the adversarial one: a demuxer cache of 1 MiB
        // with no back buffer at all cannot serve the seek from its own copy, so
        // if mpv had any way to go back to the stream for those bytes, this is
        // where it would take it.
        let generous = probe_backward_seek(64 * 1024 * 1024, 50 * 1024 * 1024);
        assert_eq!(generous.opens, 1, "the stream was reopened");
        assert_eq!(
            generous.backward_reads, 0,
            "a read started behind the furthest byte already served"
        );
        assert!(!asked_to_go_back(&generous), "{:?}", generous.seeks);
        assert!(
            generous.went_back,
            "seeking backwards must keep working: mpv serves it from its own cache"
        );

        let starved = probe_backward_seek(1024 * 1024, 0);
        assert_eq!(
            starved.opens, 1,
            "the stream was reopened to seek backwards"
        );
        assert_eq!(
            starved.backward_reads, 0,
            "a read started behind the furthest byte already served"
        );
        assert!(!asked_to_go_back(&starved), "{:?}", starved.seeks);
        println!(
            "PROBE generous: seeks={:?} went_back={}\nPROBE starved:  seeks={:?} went_back={}",
            generous.seeks, generous.went_back, starved.seeks, starved.went_back
        );
    }

    // ---- diagnosis harness ----
    //
    // Not part of the normal suite: it needs a media file and measures real
    // playback timing. Run deliberately:
    //   cargo test -p priel-player --  --ignored --nocapture measure_

    /// The seek callback priel deliberately does **not** register, kept here so
    /// the comparison that justified that can be re-run. See
    /// `measure_time_to_first_audio`.
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "the stream_cb ABI is i64; negative offsets are rejected"
    )]
    fn probe_seek(cookie: &mut Cookie, offset: i64) -> i64 {
        if offset < 0 {
            return -1;
        }
        let target = offset as u64;
        let mut g = lock(&cookie.shared.inner);
        loop {
            if target < g.base {
                // Released by `trim`. The production protocol registers no seek
                // callback at all, so this is only reachable from this harness.
                return -1;
            }
            if target <= end_of(&g) || g.complete || g.aborted {
                cookie.pos = target;
                g.read_pos = g.read_pos.max(target);
                cookie.shared.cv.notify_all();
                return target as i64;
            }
            g = wait(&cookie.shared.cv, g);
        }
    }

    /// Feed a file through the protocol at `bytes_per_sec` and report how long
    /// until audio actually starts, and how much of the file had been fed by
    /// then. If playback needs the whole file, the second number is ~100%.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a percentage of a file size; precision beyond a byte is irrelevant"
    )]
    fn measure_start(with_seek: bool, bytes_per_sec: usize) -> Option<(Duration, f64)> {
        let path = std::env::var("PRIEL_TEST_MEDIA").unwrap_or("/tmp/t.mp4".to_string());
        let media = std::fs::read(&path).ok()?;
        let total = media.len();

        let mpv = Mpv::new().ok()?;
        init_mpv(&mpv, &silent_config());
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let sh = shared(Vec::new(), false);
        lock(&registry).insert(1, sh.clone());

        // SAFETY: same contract as the player thread; see `spawn`.
        let protocol = unsafe {
            Protocol::new(
                &mpv,
                "prielseg".into(),
                registry.clone(),
                open,
                close,
                read,
                if with_seek { Some(probe_seek) } else { None },
                Some(size),
            )
        };
        protocol.register().ok()?;
        // Never dropped; see `spawn`.
        std::mem::forget(protocol);

        // Feed at a fixed rate, as a download would. `at` is an offset into the
        // file, which is the stream offset too: the buffer releases bytes at the
        // front as they are read, so `data.len()` is no longer how much has been
        // fed and must not be used as either the progress or the total.
        let feeder = sh.clone();
        let fed = Arc::new(Mutex::new(0usize));
        let fed_w = fed.clone();
        thread::spawn(move || {
            let step = (bytes_per_sec / 20).max(1);
            let mut at = 0usize;
            while at < media.len() {
                let end = (at + step).min(media.len());
                {
                    let mut g = lock(&feeder.inner);
                    if g.aborted {
                        return;
                    }
                    g.data.extend(&media[at..end]);
                    feeder.cv.notify_all();
                }
                *lock(&fed_w) = end;
                at = end;
                thread::sleep(Duration::from_millis(50));
            }
            let mut g = lock(&feeder.inner);
            g.total = Some(end_of(&g));
            g.complete = true;
            feeder.cv.notify_all();
        });

        let started = Instant::now();
        let _ = mpv.command("loadfile", &["prielseg://1", "replace"]);
        let deadline = started + Duration::from_secs(60);
        while Instant::now() < deadline {
            if mpv.get_property::<f64>("time-pos").unwrap_or(0.0) > 0.0 {
                let elapsed = started.elapsed();
                let pct = *lock(&fed) as f64 / total as f64 * 100.0;
                return Some((elapsed, pct));
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    #[ignore = "needs a media file and measures real timing"]
    fn measure_time_to_first_audio() {
        // Goal: settle whether playback starts on the first bytes or waits for
        // the whole stream, and whether advertising the stream as seekable is
        // what makes the difference.
        for rate in [2_000_000usize, 8_000_000] {
            for with_seek in [true, false] {
                match measure_start(with_seek, rate) {
                    Some((took, pct)) => println!(
                        "MEASURE feed={:>5} KB/s  seek={:<5}  audio after {:>6.2}s, {:>5.1}% fed",
                        rate / 1000,
                        with_seek,
                        took.as_secs_f64(),
                        pct
                    ),
                    None => println!(
                        "MEASURE feed={:>5} KB/s  seek={:<5}  NO AUDIO within 60s",
                        rate / 1000,
                        with_seek
                    ),
                }
            }
        }
    }

    // ---- feasibility harness: driving a display from the decoded audio ----
    //
    // Outside the normal suite for the same reasons as the block above: these
    // play in real time, write PCM to a temporary file, and read the process's
    // own CPU counters. Run them deliberately, with the rest:
    //   cargo test -p priel-player -- --ignored --nocapture measure_
    //
    // What they settled is written up in
    // `docs/adr/0002-a-display-of-the-audio-costs-the-gapless-transition.md`.

    /// A deterministic hi-res signal, synthesised by the ffmpeg already inside
    /// libmpv. No media file, no network and no audio device, so the comparison
    /// below reproduces anywhere libmpv does.
    const ANALYSIS_SIGNAL: &str = "av://lavfi:anoisesrc=r=96000:d=25:c=pink:a=0.9:seed=42";

    /// The same signal, short enough to hear the join between two of them.
    const ANALYSIS_SIGNAL_SHORT: &str = "av://lavfi:anoisesrc=r=96000:d=2:c=pink:a=0.9:seed=42";

    /// The only graph shape that feeds a display without standing in the audio
    /// path: the decoded stream is split, one copy goes to the output untouched
    /// and the other into an analysis filter, whose result is a *video* stream.
    const ANALYSIS_GRAPH: &str = "[aid1] asplit [ao] [t]; [t] showcqt=s=120x32 [vo]";

    /// A player configured as priel's is, optionally with the analysis branch
    /// attached. Nothing is loaded yet.
    fn analysing_mpv(with_branch: bool) -> Option<Mpv> {
        let mpv = Mpv::new().ok()?;
        init_mpv(&mpv, &silent_config());
        if with_branch {
            // The analysis filter's output is video, which priel otherwise
            // switches off outright, and it needs somewhere to go.
            set_prop(&mpv, "vid", "auto");
            set_prop(&mpv, "vo", "null");
            set_prop(&mpv, "lavfi-complex", ANALYSIS_GRAPH);
        }
        Some(mpv)
    }

    /// Wait for `count` files to finish, or give up.
    ///
    /// Also counts the times mpv says it is opening the audio output, which is
    /// only visible in mpv's own log: a second one across a playlist transition
    /// means the output was torn down and rebuilt, which is the gap that
    /// `gapless-audio` exists to remove.
    fn play_until_ended(mpv: &Mpv, count: u32, limit: Duration) -> Option<u32> {
        let deadline = Instant::now() + limit;
        let mut ended = 0;
        let mut opens = 0;
        while ended < count && Instant::now() < deadline {
            match mpv.wait_event(0.2) {
                Some(Ok(Event::EndFile(_))) => ended += 1,
                Some(Ok(Event::LogMessage { text, .. })) => {
                    if text.contains("Trying audio driver") {
                        opens += 1;
                    }
                }
                Some(Err(_)) => return None,
                _ => {}
            }
        }
        (ended == count).then_some(opens)
    }

    /// Capture exactly what would reach the audio API, with the analysis branch
    /// attached or not.
    ///
    /// mpv's PCM writer stands where the device would, so this opens nothing and
    /// runs far faster than real time.
    fn output_bytes(with_branch: bool, path: &std::path::Path) -> Option<Vec<u8>> {
        let _ = std::fs::remove_file(path);
        {
            let mpv = analysing_mpv(with_branch)?;
            set_prop(&mpv, "ao", "pcm");
            set_prop(&mpv, "ao-pcm-file", path.to_str()?);
            mpv.command("loadfile", &[ANALYSIS_SIGNAL, "replace"])
                .ok()?;
            play_until_ended(&mpv, 1, Duration::from_secs(120))?;
        }
        std::fs::read(path).ok()
    }

    /// Process CPU time, user plus system, in seconds.
    ///
    /// The 14th and 15th fields of `/proc/self/stat`, in clock ticks. The `comm`
    /// field can hold spaces and brackets, so the split is on the last `") "`
    /// rather than on whitespace from the front.
    fn cpu_secs() -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let mut fields = stat.rsplit_once(") ")?.1.split_whitespace();
        let utime: f64 = fields.nth(11)?.parse().ok()?;
        let stime: f64 = fields.next()?.parse().ok()?;
        // USER_HZ, fixed at 100 on Linux.
        Some((utime + stime) / 100.0)
    }

    /// CPU seconds spent over `window` of real-time playback.
    ///
    /// Whole-process, so the two runs have to be sequential - which they are,
    /// each owning its player for the length of the call.
    fn cpu_while_playing(with_branch: bool, window: Duration) -> Option<f64> {
        let mpv = analysing_mpv(with_branch)?;
        mpv.command("loadfile", &[ANALYSIS_SIGNAL, "replace"])
            .ok()?;
        // The decoder and the filter graph both settle before the clock starts.
        thread::sleep(Duration::from_secs(2));
        let before = cpu_secs()?;
        thread::sleep(window);
        let after = cpu_secs()?;
        Some(after - before)
    }

    #[test]
    #[ignore = "writes PCM to a temporary file and decodes a whole signal"]
    fn measure_what_an_analysis_branch_does_to_the_output() {
        // Goal: the acceptance test for a spectrum display. Splitting the
        // decoded stream and analysing one copy must leave the other copy - the
        // one the device receives - byte for byte what it would have been. The
        // third capture is the control: without it, two identical hashes could
        // just mean the signal is the same both times, and two different ones
        // could just mean the source is not reproducible.
        let dir = std::env::temp_dir();
        let (plain, split, again) = (
            dir.join("priel-analysis-plain.wav"),
            dir.join("priel-analysis-split.wav"),
            dir.join("priel-analysis-again.wav"),
        );
        let (Some(a), Some(b), Some(c)) = (
            output_bytes(false, &plain),
            output_bytes(true, &split),
            output_bytes(false, &again),
        ) else {
            println!("MEASURE no capture: libmpv would not write PCM here");
            return;
        };
        println!(
            "MEASURE plain={} bytes  split={} bytes  repeat={} bytes",
            a.len(),
            b.len(),
            c.len()
        );
        assert_eq!(a, c, "the capture must be reproducible to mean anything");
        assert_eq!(
            a, b,
            "an analysis branch must not alter the samples reaching the output"
        );
        for p in [plain, split, again] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    #[ignore = "plays in real time to measure CPU"]
    fn measure_the_cpu_an_analysis_branch_costs() {
        // Goal: put a number on what a display costs while it runs, against the
        // decode it is added to. priel redraws only when something changed and
        // backs the player thread off when idle, so this is the part of that
        // saving a continuously updating display would hand back.
        let window = Duration::from_secs(10);
        let (Some(plain), Some(split)) = (
            cpu_while_playing(false, window),
            cpu_while_playing(true, window),
        ) else {
            println!("MEASURE no CPU reading available on this platform");
            return;
        };
        let pct = |s: f64| s / window.as_secs_f64() * 100.0;
        println!(
            "MEASURE decode only {:.2}s CPU ({:.1}% of one core); with analysis {:.2}s ({:.1}%); \
             the branch costs {:.2}s ({:.1} points)",
            plain,
            pct(plain),
            split,
            pct(split),
            split - plain,
            pct(split - plain)
        );
    }

    #[test]
    #[ignore = "plays two files in real time to reach the transition"]
    fn measure_whether_an_analysis_branch_keeps_the_transition_gapless() {
        // Goal: gapless works by keeping the audio output open across a playlist
        // transition. A complex filter graph is rebuilt per file, and the
        // question is whether the output survives that. One open across two
        // files is gapless; two is the gap back.
        for with_branch in [false, true] {
            let Some(mpv) = analysing_mpv(with_branch) else {
                println!("MEASURE mpv would not start");
                return;
            };
            // The output being reopened is only visible in mpv's own log.
            request_log_messages(&mpv, "v");
            let _ = mpv.command("loadfile", &[ANALYSIS_SIGNAL_SHORT, "replace"]);
            let _ = mpv.command("loadfile", &[ANALYSIS_SIGNAL_SHORT, "append"]);
            match play_until_ended(&mpv, 2, Duration::from_secs(60)) {
                Some(opens) => println!(
                    "MEASURE analysis branch={with_branch:<5} audio output opened {opens}x across \
                     two files"
                ),
                None => println!("MEASURE analysis branch={with_branch:<5} did not finish"),
            }
        }
    }
}
