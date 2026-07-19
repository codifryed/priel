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

/// Is there more unread data buffered than [`DOWNLOAD_AHEAD_MAX`] allows?
fn too_far_ahead(b: &Buf) -> bool {
    // Saturating: a reader may seek past the end of what has been downloaded, so
    // read_pos can legitimately exceed the buffer length.
    (b.data.len() as u64).saturating_sub(b.read_pos) > DOWNLOAD_AHEAD_MAX
}

fn spawn_downloader(urls: Vec<String>, shared: Arc<Shared>) {
    thread::spawn(move || {
        for u in &urls {
            {
                // Backpressure: never hold more than DOWNLOAD_AHEAD_MAX bytes
                // that nothing has read yet.
                let mut g = lock(&shared.inner);
                while !g.aborted && too_far_ahead(&g) {
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

    // ---- seeking ----

    #[test]
    fn seeks_move_the_cursor_and_reject_negatives() {
        // Goal: the stream_cb contract is a byte offset; negative is invalid and
        // must be refused rather than wrapped into a huge unsigned value.
        let sh = shared(b"0123456789".to_vec(), true);
        let mut c = cookie(&sh);
        assert_eq!(seek(&mut c, -1), -1);
        assert_eq!(seek(&mut c, 4), 4);
        let mut buf = [0i8; 2];
        assert_eq!(read(&mut c, &mut buf), 2);
        assert_eq!(&buf, b"45".map(|b| i8::from_ne_bytes([b])).as_slice());
    }

    #[test]
    fn a_seek_past_a_complete_stream_is_allowed() {
        // Goal: mpv probes past the end while parsing; on a complete buffer that
        // must resolve instead of blocking forever.
        let sh = shared(b"abc".to_vec(), true);
        let mut c = cookie(&sh);
        assert_eq!(seek(&mut c, 99), 99);
        let mut buf = [0i8; 4];
        assert_eq!(read(&mut c, &mut buf), 0);
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

    fn silent_mpv() -> Mpv {
        let mpv = Mpv::new().expect("mpv should initialise headlessly");
        init_mpv(&mpv, Some("null"));
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
        assert_eq!(mpv.get_property::<String>("ao").unwrap(), "null");
    }

    #[test]
    fn a_device_string_selects_that_output() {
        // Goal: `--device` is how a user reaches their DAC; it must become
        // audio-device rather than being swallowed.
        let mpv = Mpv::new().expect("mpv");
        init_mpv(&mpv, Some("pipewire/some.dac"));
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
        let st = read_status(&mpv, &[]);
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
        let st = read_status(&mpv, &entries);
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

    #[test]
    fn a_seek_beyond_the_download_waits_for_the_bytes() {
        // Goal: mpv may seek ahead of what has been fetched. On an incomplete
        // stream that must block until the data lands rather than report a
        // position the buffer cannot serve.
        let sh = shared(b"12".to_vec(), false);
        let writer = sh.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut g = lock(&writer.inner);
            g.data.extend_from_slice(b"345678");
            writer.cv.notify_all();
        });
        let mut c = cookie(&sh);
        assert_eq!(seek(&mut c, 6), 6, "should have waited rather than failed");
    }
}
