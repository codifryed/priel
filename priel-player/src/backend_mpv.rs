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
use std::time::Duration;

use libmpv2::{Mpv, SetData, protocol::Protocol};
use priel_core::PlayableSource;
use ureq::{Agent, Body, http::Response};

use crate::{Cmd, PlaybackStatus};

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

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "the stream_cb ABI is i64; negative offsets are rejected above"
)]
fn seek(cookie: &mut Cookie, offset: i64) -> i64 {
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

fn size(cookie: &mut Cookie) -> i64 {
    lock(&cookie.shared.inner)
        .total
        .and_then(|t| i64::try_from(t).ok())
        .unwrap_or(-1)
}

// ---- player thread ----

pub fn spawn(
    audio_device: Option<String>,
    rx: Receiver<Cmd>,
    status: Arc<Mutex<PlaybackStatus>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mpv = match Mpv::new() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("priel-player: Mpv::new failed: {e:?}");
                return;
            }
        };
        init_mpv(&mpv, audio_device.as_deref());

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
                Some(seek),
                Some(size),
            )
        };
        if let Err(e) = protocol.register() {
            eprintln!("priel-player: protocol register failed: {e:?}");
            return;
        }

        let mut entries: Vec<Entry> = Vec::new();
        let mut seq: u64 = 0;

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
            cleanup_playlist(&mpv, &registry, &mut entries);
            let st = read_status(&mpv, &entries);
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

        let _ = mpv.command("stop", &[]);
        clear_all(&registry, &mut entries);
    })
}

/// Set an mpv property, asserting in debug builds that mpv accepted it.
///
/// There is no channel to report a rejected property on, so a wrong name or type
/// would silently do nothing. That is not hypothetical: `demuxer-readahead-secs`
/// was set here for a while and had no effect, and nothing said so.
fn set_prop<V: SetData>(mpv: &Mpv, name: &str, value: V) {
    let accepted = mpv.set_property(name, value).is_ok();
    debug_assert!(accepted, "mpv rejected property {name}");
}

/// Apply the fixed player settings and select the output device.
fn init_mpv(mpv: &Mpv, audio_device: Option<&str>) {
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
    set_prop(mpv, "demuxer-max-bytes", 64i64 * 1024 * 1024);
    // Only consulted on the non-network path, where cache-secs does not apply.
    set_prop(mpv, "demuxer-readahead-secs", 30.0);

    match audio_device {
        Some("null") => set_prop(mpv, "ao", "null"),
        Some(dev) => set_prop(mpv, "audio-device", dev),
        None => set_prop(mpv, "ao", "pipewire"),
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
            let _ = mpv.command("loadfile", &[&arg, "replace"]);
            entries.push(Entry { id, seq: s });
        }
        Cmd::Append(id, src) => {
            let (arg, s) = register_source(registry, seq, src);
            let _ = mpv.command("loadfile", &[&arg, "append"]);
            entries.push(Entry { id, seq: s });
        }
        Cmd::Next => {
            let _ = mpv.command("playlist-next", &["force"]);
        }
        Cmd::TogglePause => {
            let _ = mpv.command("cycle", &["pause"]);
        }
        Cmd::Seek(t) => {
            let v = format!("{t}");
            let _ = mpv.command("seek", &[&v, "absolute"]);
        }
        Cmd::SeekRelative(t) => {
            let v = format!("{t}");
            let _ = mpv.command("seek", &[&v, "relative"]);
        }
        Cmd::SetVolume(v) => {
            let _ = mpv.set_property("volume", v);
        }
        Cmd::Stop => {
            let _ = mpv.command("stop", &[]);
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
        let _ = mpv.command("playlist-remove", &["0"]);
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

fn spawn_downloader(urls: Vec<String>, shared: Arc<Shared>) {
    thread::spawn(move || {
        for u in &urls {
            {
                // Backpressure: never hold more than DOWNLOAD_AHEAD_MAX bytes
                // that nothing has read yet.
                let mut g = lock(&shared.inner);
                while !g.aborted
                    && (g.data.len() as u64).saturating_sub(g.read_pos) > DOWNLOAD_AHEAD_MAX
                {
                    g = wait(&shared.cv, g);
                }
                if g.aborted {
                    return;
                }
            }
            // A non-2xx is an `Err` here (the agent keeps ureq's default
            // `http_status_as_error`), matching the old `error_for_status`.
            let Some(bytes) = HTTP.get(u).call().ok().and_then(|mut r| read_body(&mut r)) else {
                break;
            };
            let mut g = lock(&shared.inner);
            if g.aborted {
                return;
            }
            g.data.extend_from_slice(&bytes);
            shared.cv.notify_all();
        }
        let mut g = lock(&shared.inner);
        g.total = Some(g.data.len() as u64);
        g.complete = true;
        shared.cv.notify_all();
    });
}

/// Read a whole segment body. `as_reader` is used rather than `read_to_vec`
/// because the latter applies ureq's body-size limit, and hi-res segments run
/// to several megabytes.
fn read_body(resp: &mut Response<Body>) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "bitrate is a display value; fractional bits/s are meaningless"
)]
fn read_status(mpv: &Mpv, entries: &[Entry]) -> PlaybackStatus {
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
    let codec = mpv
        .get_property::<String>("audio-codec-name")
        .unwrap_or_default();
    let bitrate = mpv.get_property::<f64>("audio-bitrate").unwrap_or(0.0) as i64;
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
        codec,
        bitrate,
        ended,
        volume,
        current_id: entries.first().map_or(0, |e| e.id),
        has_next: entries.len() > 1,
        cache_secs,
    }
}
