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

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libmpv2::{Mpv, SetData, events::Event, protocol::Protocol};
use priel_core::PlayableSource;
use ureq::{Agent, Body, http::Response};

use crate::hw::{self, HwParams};
use crate::{Cmd, PlaybackStatus, PlayerConfig};

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

/// Growing per-track buffer shared between a downloader and mpv's callbacks.
struct Shared {
    inner: Mutex<Buf>,
    cv: Condvar,
}
struct Buf {
    data: Vec<u8>,
    complete: bool,
    total: Option<u64>,
    aborted: bool,
    /// Furthest byte offset any reader has consumed. Only ever moves forward, so
    /// a backward seek does not let the downloader run away again.
    read_pos: u64,
}

/// How far the downloader may run ahead of the reader before it parks.
///
/// Without this, appending a preloaded next track downloads that whole track
/// immediately - and mpv does not open a queued playlist entry until it reaches
/// it, so nothing consumes those bytes in the meantime. 32 MiB is around 28s of
/// 24/192 FLAC, enough of a head start to cover the transition.
const DOWNLOAD_AHEAD_MAX: u64 = 32 * 1024 * 1024;

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
            data: Vec::new(),
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
        let len = g.data.len() as u64;
        if cookie.pos < len {
            let start = cookie.pos as usize;
            let n = ((len - cookie.pos) as usize).min(want);
            for (d, s) in buf[..n].iter_mut().zip(&g.data[start..start + n]) {
                // mpv hands us a C `char*`; reinterpret, never numerically convert.
                *d = i8::from_ne_bytes([*s]);
            }
            cookie.pos += n as u64;
            // Let the downloader know it may resume (see DOWNLOAD_AHEAD_MAX).
            g.read_pos = g.read_pos.max(cookie.pos);
            cookie.shared.cv.notify_all();
            return n as i64;
        }
        if g.complete || g.aborted {
            return 0; // EOF
        }
        g = wait(&cookie.shared.cv, g);
    }
}

fn size(cookie: &mut Cookie) -> i64 {
    lock(&cookie.shared.inner)
        .total
        .and_then(|t| i64::try_from(t).ok())
        .unwrap_or(-1)
}

// ---- player thread ----

pub fn spawn(
    config: PlayerConfig,
    rx: Receiver<Cmd>,
    status: Arc<Mutex<PlaybackStatus>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
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
        //  - `protocol` is declared after `mpv` and so is dropped *before* it,
        //    unregistering while the handle is still alive; both outlive the loop.
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
                None,
                Some(size),
            )
        };
        if let Err(e) = protocol.register() {
            log::error!("the prielseg protocol would not register, so there is no playback: {e}");
            return;
        }

        let mut entries: Vec<Entry> = Vec::new();
        let mut seq: u64 = 0;
        // The ALSA readout costs a handful of /proc reads, so it is refreshed on
        // its own slower cadence rather than on every status tick.
        let mut hw: Option<HwParams> = None;
        // `None` means "never probed", which forces the first tick to look.
        let mut hw_checked: Option<Instant> = None;

        loop {
            let mut quit = false;
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        if handle_cmd(&mpv, &registry, &mut entries, &mut seq, cmd) {
                            quit = true;
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        quit = true;
                        break;
                    }
                }
            }
            if quit {
                break;
            }
            drain_events(&mpv);
            cleanup_playlist(&mpv, &registry, &mut entries);
            if hw_checked.is_none_or(|t| t.elapsed() >= HW_PROBE_INTERVAL) {
                hw = hw::probe(config.audio_device.as_deref());
                hw_checked = Some(Instant::now());
            }
            let st = read_status(&mpv, &entries, hw.clone());
            // Poll fast enough for a smooth progress bar while audio is moving,
            // and back off when it is not. `recv_timeout` rather than `sleep` so
            // a command still wakes the thread immediately either way - the
            // backoff must not cost keypress latency.
            let idle_backoff = if st.playing {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(500)
            };
            *lock(&status) = st;
            match rx.recv_timeout(idle_backoff) {
                Ok(cmd) => {
                    if handle_cmd(&mpv, &registry, &mut entries, &mut seq, cmd) {
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
fn drain_events(mpv: &Mpv) {
    for _ in 0..EVENT_DRAIN_MAX {
        // Zero timeout: this runs on the player tick and must not block it.
        match mpv.wait_event(0.0) {
            None => return,
            Some(Err(e)) => log::warn!("mpv reported a failure: {e}"),
            Some(Ok(event)) => match event {
                Event::EndFile(reason) => {
                    log::debug!("mpv ended a file: {}", end_file_reason(reason));
                }
                Event::FileLoaded => log::debug!("mpv loaded a file"),
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

    // mpv's own log, when the caller asked for one. It writes this itself and
    // truncates on open, which is why it cannot share priel's file.
    if let Some(path) = config.mpv_log_file.as_deref() {
        set_prop(mpv, "log-file", path);
    }
}

/// Returns true if the thread should quit.
fn handle_cmd(
    mpv: &Mpv,
    registry: &Registry,
    entries: &mut Vec<Entry>,
    seq: &mut u64,
    cmd: Cmd,
) -> bool {
    match cmd {
        Cmd::Quit => return true,
        Cmd::Load(id, src) => {
            clear_all(registry, entries);
            let (arg, s) = register_source(registry, seq, src);
            command(mpv, "loadfile", &[&arg, "replace"]);
            entries.push(Entry { id, seq: s });
        }
        Cmd::Append(id, src) => {
            let (arg, s) = register_source(registry, seq, src);
            command(mpv, "loadfile", &[&arg, "append"]);
            entries.push(Entry { id, seq: s });
        }
        Cmd::Next => {
            command(mpv, "playlist-next", &["force"]);
        }
        Cmd::TogglePause => {
            command(mpv, "cycle", &["pause"]);
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
    }
    false
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
                    data: Vec::new(),
                    complete: false,
                    total: None,
                    aborted: false,
                    read_pos: 0,
                }),
                cv: Condvar::new(),
            });
            lock(registry).insert(s, shared.clone());
            spawn_downloader(urls, shared);
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
    // for a skipped hi-res track that is hundreds of megabytes freed at once.
    g.data = Vec::new();
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
    // Saturating: a reader may seek past the end of what has been downloaded, so
    // read_pos can legitimately exceed the buffer length.
    (b.data.len() as u64).saturating_sub(b.read_pos) > DOWNLOAD_AHEAD_MAX
}

/// Why a segment stopped feeding the buffer.
enum Stop {
    /// The stream was discarded; do not touch the buffer again.
    Aborted,
    /// The fetch failed. Playback ends short, but the buffer must still be
    /// completed so a waiting reader is released rather than hanging.
    Failed,
}

fn spawn_downloader(urls: Vec<String>, shared: Arc<Shared>) {
    thread::spawn(move || {
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
        let mut g = lock(&shared.inner);
        g.total = Some(g.data.len() as u64);
        g.complete = true;
        shared.cv.notify_all();
    });
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
                    g.data.extend_from_slice(&chunk[..n]);
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
fn read_status(mpv: &Mpv, entries: &[Entry], hw: Option<HwParams>) -> PlaybackStatus {
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
        hw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    fn shared(data: Vec<u8>, complete: bool) -> Arc<Shared> {
        Arc::new(Shared {
            inner: Mutex::new(Buf {
                total: complete.then_some(data.len() as u64),
                data,
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
    /// The buffer is global, so the tests that read it take turns.
    static READING: Mutex<()> = Mutex::new(());

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            lock(&LINES).push(format!("{} {}", record.target(), record.args()));
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

    fn cookie(sh: &Arc<Shared>) -> Cookie {
        Cookie {
            shared: sh.clone(),
            pos: 0,
        }
    }

    /// Serves one canned body then closes. Used to exercise the download path
    /// without reaching the network.
    fn one_shot(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/seg", listener.local_addr().expect("addr"));
        thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 S\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(body);
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
            g.data.extend_from_slice(b"late");
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
            data: vec![0; 8],
            complete: false,
            total: None,
            aborted: false,
            read_pos: 0,
        };
        assert!(!too_far_ahead(&b), "a small buffer keeps downloading");

        b.data = vec![0; usize::try_from(DOWNLOAD_AHEAD_MAX).unwrap() + 1];
        assert!(too_far_ahead(&b), "unread data past the cap parks it");

        b.read_pos = b.data.len() as u64;
        assert!(!too_far_ahead(&b), "once consumed it resumes");

        b.read_pos = b.data.len() as u64 + 999;
        assert!(!too_far_ahead(&b), "a seek past the end must not underflow");
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

    // ---- downloading ----

    #[test]
    fn a_downloaded_segment_lands_in_the_buffer_and_completes() {
        // Goal: the end-to-end fetch path, including the completion flag that
        // tells a waiting reader it has reached the real end of the track.
        let url = one_shot(b"segment-bytes");
        let sh = shared(Vec::new(), false);
        spawn_downloader(vec![url], sh.clone());
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
        spawn_downloader(vec!["http://127.0.0.1:1/x".into()], sh.clone());
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
            spawn_downloader(vec![dead], sh.clone());
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
                drain_events(&mpv);
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
    fn draining_an_idle_queue_costs_nothing_and_says_nothing() {
        // Goal: this runs on every tick of the player loop, so the common case
        // must be a cheap no-op that does not fill the log with noise.
        let lines = captured(|| {
            let mpv = silent_mpv();
            drain_events(&mpv);
            drain_events(&mpv);
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
    fn mpvs_own_log_is_a_setting_mpv_accepts() {
        // Goal: the one property priel does not always apply, so it is the one
        // that could sit misspelled and unnoticed. mpv opens the file itself.
        let path = std::env::temp_dir().join(format!("priel-mpvlog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let lines = captured(|| {
            let mpv = Mpv::new().expect("mpv should initialise headlessly");
            init_mpv(
                &mpv,
                &PlayerConfig {
                    audio_device: Some("null".into()),
                    mpv_log_file: Some(path.to_string_lossy().into_owned()),
                },
            );
        });
        assert!(
            !lines.iter().any(|l| l.contains("rejected the property")),
            "{lines:?}"
        );
        assert!(path.exists(), "mpv should have opened its own log");
        let _ = std::fs::remove_file(&path);
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
    fn status_reflects_an_idle_player() {
        // Goal: the UI derives "buffering" and the end-of-track fallback from
        // this snapshot, so an idle player must report honestly rather than
        // looking like a loaded track at position zero.
        let mpv = silent_mpv();
        let st = read_status(&mpv, &[], None);
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
        let entries = vec![Entry { id: 11, seq: None }, Entry { id: 22, seq: None }];
        let st = read_status(&mpv, &entries, None);
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

        assert!(!handle_cmd(
            &mpv,
            &reg,
            &mut entries,
            &mut seq,
            Cmd::SetVolume(42.0)
        ));
        assert!((mpv.get_property::<f64>("volume").unwrap() - 42.0).abs() < f64::EPSILON);

        for cmd in [
            Cmd::TogglePause,
            Cmd::Seek(1.0),
            Cmd::SeekRelative(-1.0),
            Cmd::Next,
            Cmd::Stop,
        ] {
            assert!(!handle_cmd(&mpv, &reg, &mut entries, &mut seq, cmd));
        }

        assert!(
            handle_cmd(&mpv, &reg, &mut entries, &mut seq, Cmd::Quit),
            "only Quit ends the loop"
        );
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

        handle_cmd(&mpv, &reg, &mut entries, &mut seq, Cmd::Load(1, src()));
        assert_eq!(entries.len(), 1);
        handle_cmd(&mpv, &reg, &mut entries, &mut seq, Cmd::Append(2, src()));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[1].id, 2);

        handle_cmd(&mpv, &reg, &mut entries, &mut seq, Cmd::Load(3, src()));
        assert_eq!(entries.len(), 1, "Load replaces rather than appends");
        assert_eq!(entries[0].id, 3);
    }

    #[test]
    fn cleanup_is_a_no_op_while_the_first_entry_is_still_playing() {
        // Goal: it may only drop entries mpv has moved past. Dropping the head
        // would free the buffer of the track being played.
        let mpv = silent_mpv();
        let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
        let mut entries = vec![Entry { id: 1, seq: None }];
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
        spawn_downloader(vec![url], sh.clone());

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
        spawn_downloader(vec![url], sh.clone());
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
        spawn_downloader(vec![url], sh.clone());
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
            if target <= g.data.len() as u64 || g.complete || g.aborted {
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

        // Feed at a fixed rate, as a download would.
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
                    g.data.extend_from_slice(&media[at..end]);
                    feeder.cv.notify_all();
                }
                *lock(&fed_w) = end;
                at = end;
                thread::sleep(Duration::from_millis(50));
            }
            let mut g = lock(&feeder.inner);
            g.total = Some(g.data.len() as u64);
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
}
