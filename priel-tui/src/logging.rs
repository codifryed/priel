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

//! The diagnostics sink: where `log::warn!` and friends end up.
//!
//! priel owns the whole terminal, so a diagnostic printed to stderr lands on the
//! alternate screen and is lost. Everything goes to a file instead.
//!
//! This module owns formatting, the byte budget and the writer thread. It must
//! never do file I/O on the caller's thread: the UI thread has a frame deadline
//! and the player thread has an audio one, so `log` only formats a line and hands
//! it to a bounded queue. A full queue drops records and counts them; it never
//! blocks the producer. The one exception is [`Logger::flush`], a deliberate
//! round-trip used when the process is about to die.
//!
//! Nothing here may be called from mpv's `read`/`seek` callbacks: formatting
//! allocates, and those callbacks may not.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::{Level, LevelFilter, Metadata, Record};

/// How many formatted lines may be in flight before records start being dropped.
/// Deep enough that only a runaway loop reaches it, bounded because everything
/// fed from outside is bounded.
const QUEUE_DEPTH: usize = 1024;

/// The log stops at this size rather than growing without limit in the user's
/// state directory. Rotation would be a dependency; a cap and a truncate at
/// startup are the whole of what a single-user desktop client needs.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How many lines the in-app view keeps. Bounded by what a person will scroll
/// through, not by what the disk will hold - the file is the long record.
const RECENT_MAX: usize = 500;

/// The last few hundred lines, kept in memory as well as written out.
///
/// The file answers "what happened during that session"; this answers "what just
/// went wrong", for the person still sitting in front of the terminal. Sharing
/// one source means the overlay and a bug report cannot disagree.
///
/// Filled by the writer thread, so a producer pays nothing for it.
#[derive(Clone, Default)]
pub struct Recent(Arc<Mutex<VecDeque<String>>>);

impl Recent {
    pub(crate) fn push(&self, line: String) {
        let mut ring = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if ring.len() == RECENT_MAX {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    /// A snapshot, oldest first.
    ///
    /// A copy rather than a borrow: the lock is also taken by the writer thread,
    /// and the render path must not hold it for the length of a frame.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

/// What the writer thread receives.
enum Msg {
    /// A formatted line. `urgent` asks the writer to flush once it is written,
    /// so a warning survives a crash that follows it.
    Line { text: String, urgent: bool },
    /// Flush, then acknowledge. The only path that blocks a producer.
    Sync(SyncSender<()>),
}

/// The `log::Log` front end: formats, then hands off. Owns no file.
pub struct Logger {
    level: LevelFilter,
    tx: SyncSender<Msg>,
    /// Records lost to a full queue, reported on the next line that gets through
    /// so the gap is visible rather than silent.
    dropped: AtomicU64,
}

/// The writer end: owns the sink and the byte budget.
struct Sink<W: Write> {
    out: W,
    written: u64,
    limit: u64,
    capped: bool,
    recent: Recent,
}

/// Install the global logger, writing to `path`.
///
/// Called from `main` and nowhere else: it creates a file in the user's state
/// directory, and no test may do that.
///
/// # Errors
///
/// If the log directory or the file itself cannot be created, or if a logger has
/// already been installed.
pub fn init(level: LevelFilter, path: &str, recent: &Recent) -> Result<()> {
    // Off means off: no file is created at all, rather than an empty one left
    // behind in the user's state directory.
    if level == LevelFilter::Off {
        return Ok(());
    }
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the log directory {}", dir.display()))?;
    }
    // Truncating at startup is the whole of the rotation policy: one session per
    // file, bounded above by MAX_BYTES. A rotating log would be a dependency.
    let file =
        std::fs::File::create(path).with_context(|| format!("opening the log file {path}"))?;
    // Buffered because `--log-level debug` is thousands of lines a minute and
    // each one would otherwise be its own syscall. What that costs - a line
    // sitting in the buffer when the process dies - is bought back by flushing
    // every warning, and by the panic hook flushing before it lets go.
    let out = std::io::BufWriter::new(file);
    let (logger, _writer) = Logger::with_sink(level, out, MAX_BYTES, recent.clone());
    // The handle is dropped on purpose: the writer thread lives as long as the
    // process, since the installed logger keeps its end of the queue forever.
    log::set_boxed_logger(Box::new(logger)).context("installing the logger")?;
    log::set_max_level(level);
    Ok(())
}

/// Record a panic before the process goes down.
///
/// Release builds are `panic = "abort"`, so this is the only trace a panic on
/// the worker, player or downloader threads will ever leave.
pub fn record_panic(info: &str) {
    // A `priel::` target so the ceiling in `Logger::ceiling` treats it as ours.
    log::error!(target: "priel::panic", "{}", panic_report(info));
    // The process is about to die, so wait for the line to reach the file.
    log::logger().flush();
}

/// The panic report: what panicked, where, and how it got there.
fn panic_report(info: &str) -> String {
    format!(
        "{info}\nbacktrace:\n{}",
        std::backtrace::Backtrace::force_capture()
    )
}

impl Logger {
    /// A logger writing into `out`, plus the writer thread's handle.
    fn with_sink<W: Write + Send + 'static>(
        level: LevelFilter,
        out: W,
        limit: u64,
        recent: Recent,
    ) -> (Self, JoinHandle<()>) {
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        let mut sink = Sink::new(out, limit, recent);
        let writer = std::thread::spawn(move || sink.run(&rx));
        (Self::with_channel(level, tx), writer)
    }

    /// A logger posting to an existing channel, so the queue can be inspected
    /// without a writer thread racing the test that reads it.
    fn with_channel(level: LevelFilter, tx: SyncSender<Msg>) -> Self {
        Self {
            level,
            tx,
            dropped: AtomicU64::new(0),
        }
    }

    /// Post a line, reporting whether the queue had room for it.
    fn send(&self, text: String, urgent: bool) -> bool {
        self.tx.try_send(Msg::Line { text, urgent }).is_ok()
    }

    /// The most detail a target is allowed.
    ///
    /// Other crates log through this same facade, and `ureq` and `rustls` are
    /// chatty: at `debug` their DNS and TLS internals bury priel's own lines,
    /// which is what a bug report is actually for. They are held to warnings -
    /// which still surface a refused connection - until the level is `trace`,
    /// the setting that deliberately holds nothing back.
    fn ceiling(&self, target: &str) -> LevelFilter {
        if self.level == LevelFilter::Trace || target.starts_with("priel") {
            self.level
        } else {
            self.level.min(LevelFilter::Warn)
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.ceiling(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Account for anything the queue had no room for, ahead of the line that
        // proves it has room again. A gap that says how big it is can be read
        // around; a silent one makes the log lie.
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let note = format!("--- {dropped} record(s) dropped: the log queue was full ---\n");
            if !self.send(note, false) {
                self.dropped.fetch_add(dropped, Ordering::Relaxed);
            }
        }
        let thread = std::thread::current();
        let text = format_line(SystemTime::now(), thread.name().unwrap_or("-"), record);
        // A warning is usually the last thing written before something goes
        // wrong, so it is the line that must not sit in a buffer.
        if !self.send(text, record.level() <= Level::Warn) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        // The one path that blocks the caller, and only ever used on the way
        // out. A depth of one so the writer's acknowledgement never blocks it,
        // and a timeout so a wedged writer cannot stop the process from dying.
        let (ack_tx, ack_rx) = sync_channel(1);
        if self.tx.send(Msg::Sync(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(1));
        }
    }
}

impl<W: Write> Sink<W> {
    fn new(out: W, limit: u64, recent: Recent) -> Self {
        Self {
            out,
            written: 0,
            limit,
            capped: false,
            recent,
        }
    }

    /// Write one line, or nothing once the budget is spent.
    ///
    /// I/O errors are dropped on purpose: this *is* the error path, and there is
    /// nowhere left to report to.
    fn write_line(&mut self, text: &str) {
        // Before the cap: the byte budget protects the disk, and blinding the
        // overlay as well would help nobody.
        self.recent.push(text.to_string());
        if self.capped {
            return;
        }
        // Checked before the write rather than after, so a line is never cut in
        // half: the budget is a stopping point, not a hard ceiling.
        if self.written >= self.limit {
            self.capped = true;
            let _ = writeln!(self.out, "--- log truncated at {} bytes ---", self.written);
            let _ = self.out.flush();
            return;
        }
        let _ = self.out.write_all(text.as_bytes());
        self.written += u64::try_from(text.len()).unwrap_or(u64::MAX);
    }

    /// Drain until every producer is gone.
    fn run(&mut self, rx: &Receiver<Msg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Line { text, urgent } => {
                    self.write_line(&text);
                    if urgent {
                        let _ = self.out.flush();
                    }
                }
                Msg::Sync(ack) => {
                    let _ = self.out.flush();
                    let _ = ack.send(());
                }
            }
        }
        let _ = self.out.flush();
    }
}

/// `2026-07-20T09:16:03.123Z`.
///
/// Written out rather than pulling in a date crate: the log needs exactly one
/// format and this is the whole of it.
fn timestamp(now: SystemTime) -> String {
    // A clock set before 1970 reads as the epoch. There is no useful answer, and
    // the log format has no room for a negative year.
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days(secs / 86_400);
    let time_of_day = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time_of_day / 3_600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
        since.subsec_millis()
    )
}

/// Civil date from days since 1970-01-01, after Howard Hinnant's algorithm.
///
/// It counts from March so the leap day lands at the end of the year and needs
/// no special case, which is why the month comes out rotated and is put back.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// One log line. Pure, and the thread name is a parameter, so a test can assert
/// on the exact text without depending on which thread ran it.
fn format_line(now: SystemTime, thread: &str, record: &Record) -> String {
    format!(
        "{} {:<5} [{thread}] {}: {}\n",
        timestamp(now),
        record.level(),
        record.target(),
        record.args()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Logger, MAX_BYTES, Msg, RECENT_MAX, Recent, Sink, format_line, init, panic_report,
        record_panic, timestamp,
    };
    use log::{Level, LevelFilter, Log, Record};
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, UNIX_EPOCH};

    /// A sink a test can read back, since the writer thread owns the real one.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Shared {
        fn text(&self) -> String {
            let guard = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            String::from_utf8_lossy(&guard).into_owned()
        }
    }

    impl std::io::Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn at(secs: u64) -> std::time::SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn record<'a>(level: Level, args: &'a std::fmt::Arguments<'a>) -> Record<'a> {
        at_target(level, "priel_tui::test", args)
    }

    fn at_target<'a>(
        level: Level,
        target: &'a str,
        args: &'a std::fmt::Arguments<'a>,
    ) -> Record<'a> {
        Record::builder()
            .level(level)
            .target(target)
            .args(*args)
            .build()
    }

    /// Everything a logger queued, in order.
    fn drain(rx: &std::sync::mpsc::Receiver<Msg>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Msg::Line { text, .. }) = rx.try_recv() {
            out.push(text);
        }
        out
    }

    #[test]
    fn other_crates_are_held_to_warnings_until_the_level_is_trace() {
        // Goal: ureq and rustls log through this same facade, and at debug they
        // bury priel's own lines under DNS and TLS chatter. A bug report wants
        // priel's detail; a third-party warning still matters, and its detail
        // only when someone deliberately asks for everything.
        let (tx, rx) = sync_channel(8);
        let logger = Logger::with_channel(LevelFilter::Debug, tx);
        logger.log(&at_target(
            Level::Debug,
            "ureq::run",
            &format_args!("noise"),
        ));
        logger.log(&at_target(
            Level::Warn,
            "ureq::run",
            &format_args!("refused"),
        ));
        logger.log(&at_target(
            Level::Debug,
            "priel_core::mpd",
            &format_args!("ours"),
        ));
        let lines = drain(&rx).join("");
        assert!(!lines.contains("noise"), "{lines}");
        assert!(lines.contains("refused"), "{lines}");
        assert!(lines.contains("ours"), "{lines}");
    }

    #[test]
    fn trace_is_the_setting_that_holds_nothing_back() {
        // Goal: when priel's own log is not enough, the next step has to be a
        // level that shows the HTTP layer too, rather than a rebuild.
        let (tx, rx) = sync_channel(8);
        let logger = Logger::with_channel(LevelFilter::Trace, tx);
        logger.log(&at_target(
            Level::Trace,
            "ureq::run",
            &format_args!("noise"),
        ));
        assert!(drain(&rx).join("").contains("noise"));
    }

    #[test]
    fn a_timestamp_is_utc_and_sorts_lexically() {
        // Goal: the log is read by eye and by `sort`, so the format has to be
        // fixed-width ISO 8601. The leap day is the case the date maths gets
        // wrong, so it is here on purpose.
        assert_eq!(timestamp(at(0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(at(951_782_400)), "2000-02-29T00:00:00.000Z");
        assert_eq!(timestamp(at(1_784_538_963)), "2026-07-20T09:16:03.000Z");
    }

    #[test]
    fn a_timestamp_keeps_milliseconds() {
        // Goal: track changes and buffer stalls happen inside one second, so
        // whole seconds would not tell two of them apart.
        let t = UNIX_EPOCH + Duration::from_millis(1_784_538_963_042);
        assert_eq!(timestamp(t), "2026-07-20T09:16:03.042Z");
    }

    #[test]
    fn a_line_carries_the_time_level_thread_target_and_message() {
        // Goal: every field a reader needs to place a line. The thread name is
        // what tells a worker failure from a player one.
        let line = format_line(at(0), "worker", &record(Level::Warn, &format_args!("boom")));
        assert!(line.starts_with("1970-01-01T00:00:00.000Z WARN "), "{line}");
        assert!(line.contains("[worker]"), "{line}");
        assert!(line.contains("priel_tui::test"), "{line}");
        assert!(line.ends_with("boom\n"), "{line}");
    }

    #[test]
    fn the_level_decides_what_is_recorded() {
        // Goal: --log-level is the whole verbosity control, so the filter has to
        // hold for the macros' own check and for ours.
        let (tx, rx) = sync_channel(4);
        let logger = Logger::with_channel(LevelFilter::Warn, tx);
        logger.log(&record(Level::Info, &format_args!("chatty")));
        logger.log(&record(Level::Error, &format_args!("real")));
        let Ok(Msg::Line { text, .. }) = rx.try_recv() else {
            panic!("the error should have been queued")
        };
        assert!(text.contains("real"), "{text}");
        assert!(rx.try_recv().is_err(), "the info line should be dropped");
    }

    #[test]
    fn a_warning_is_marked_urgent_so_it_survives_a_crash() {
        // Goal: a warning is usually the last thing written before something
        // goes wrong, and an unflushed buffer would lose exactly that line.
        let (tx, rx) = sync_channel(4);
        let logger = Logger::with_channel(LevelFilter::Trace, tx);
        logger.log(&record(Level::Debug, &format_args!("routine")));
        logger.log(&record(Level::Warn, &format_args!("trouble")));
        let mut urgency = Vec::new();
        while let Ok(Msg::Line { urgent, .. }) = rx.try_recv() {
            urgency.push(urgent);
        }
        assert_eq!(urgency, vec![false, true]);
    }

    #[test]
    fn a_full_queue_drops_records_and_says_how_many() {
        // Goal: logging must never block the UI or player thread, so a full
        // queue has to drop. A silent drop would make the log lie, so the count
        // is reported on the next line that gets through.
        let (tx, rx) = sync_channel(1);
        let logger = Logger::with_channel(LevelFilter::Trace, tx);
        logger.log(&record(Level::Info, &format_args!("first")));
        logger.log(&record(Level::Info, &format_args!("lost")));
        // Make room, then log again: the report rides out with the next line.
        let _ = rx.try_recv();
        logger.log(&record(Level::Info, &format_args!("later")));
        let Ok(Msg::Line { text, .. }) = rx.try_recv() else {
            panic!("a line should be queued")
        };
        assert!(text.contains("1 record"), "{text}");
        assert!(text.contains("dropped"), "{text}");
    }

    #[test]
    fn the_sink_stops_at_its_budget_and_says_so() {
        // Goal: an unbounded file in the user's state directory is a bug. The
        // note matters as much as the cap: a log that just stops reads as a
        // hang.
        let shared = Shared::default();
        let mut sink = Sink::new(shared.clone(), 32, Recent::default());
        sink.write_line("0123456789012345678901234567890123456789\n");
        sink.write_line("this must not be written\n");
        let text = shared.text();
        assert!(text.contains("0123456789"), "{text}");
        assert!(!text.contains("must not"), "{text}");
        assert!(text.contains("truncated"), "the cap should say so: {text}");
    }

    #[test]
    fn lines_reach_the_sink_and_flush_waits_for_them() {
        // Goal: the end-to-end path, and the one guarantee the panic hook needs
        // - that after flush() returns, the line is really in the sink.
        let shared = Shared::default();
        let (logger, writer) = Logger::with_sink(
            LevelFilter::Info,
            shared.clone(),
            MAX_BYTES,
            Recent::default(),
        );
        logger.log(&record(Level::Error, &format_args!("boom")));
        logger.flush();
        assert!(shared.text().contains("boom"), "{}", shared.text());
        drop(logger);
        writer.join().expect("the writer thread should end cleanly");
    }

    #[test]
    fn the_recent_ring_keeps_the_newest_and_forgets_the_rest() {
        // Goal: this is read on screen, so it is bounded by what a person can
        // scroll through - and what they want is the end, not the beginning.
        let recent = Recent::default();
        for i in 0..(RECENT_MAX + 10) {
            recent.push(format!("line {i}\n"));
        }
        let lines = recent.lines();
        assert_eq!(lines.len(), RECENT_MAX, "the ring is bounded");
        assert!(lines[0].contains(&format!("line {}", 10)), "{}", lines[0]);
        assert!(
            lines[RECENT_MAX - 1].contains(&format!("line {}", RECENT_MAX + 9)),
            "the newest line is the last one"
        );
    }

    #[test]
    fn a_line_reaches_the_screen_as_well_as_the_file() {
        // Goal: the overlay and the file are fed from the same place, so what a
        // user reads on screen is what a bug report will contain.
        let shared = Shared::default();
        let recent = Recent::default();
        let (logger, _writer) =
            Logger::with_sink(LevelFilter::Info, shared.clone(), MAX_BYTES, recent.clone());
        logger.log(&record(Level::Error, &format_args!("boom")));
        logger.flush();
        assert!(shared.text().contains("boom"));
        assert!(recent.lines().iter().any(|l| l.contains("boom")));
    }

    #[test]
    fn the_screen_keeps_filling_after_the_file_is_full() {
        // Goal: the byte cap protects the disk, not the person watching. A
        // capped file must not also blind the overlay.
        let recent = Recent::default();
        let mut sink = Sink::new(Shared::default(), 4, recent.clone());
        sink.write_line("over the budget already\n");
        sink.write_line("and this one is refused\n");
        assert_eq!(recent.lines().len(), 2, "{:?}", recent.lines());
    }

    #[test]
    fn the_off_level_opens_no_file_at_all() {
        // Goal: --log-level off means priel touches nothing, so a user who wants
        // no file gets no file rather than an empty one.
        let path = std::env::temp_dir().join(format!("priel-log-off-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let name = path.to_string_lossy().into_owned();
        init(LevelFilter::Off, &name, &Recent::default()).expect("off is not an error");
        assert!(!path.exists(), "nothing should have been created");
    }

    #[test]
    fn a_log_file_that_cannot_be_opened_is_reported_and_not_fatal() {
        // Goal: --log-file is user input. Priel plays music with or without a
        // log, so a bad path has to come back as an error `main` can report
        // rather than something that stops the program.
        let blocker =
            std::env::temp_dir().join(format!("priel-log-blocker-{}", std::process::id()));
        std::fs::write(&blocker, b"not a directory").expect("the temp file should be writable");
        let path = blocker.join("priel.log");
        let err = init(
            LevelFilter::Info,
            &path.to_string_lossy(),
            &Recent::default(),
        )
        .expect_err("a file is no parent");
        assert!(format!("{err:#}").contains("log"), "{err:#}");
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn a_panic_report_carries_the_message_and_a_backtrace() {
        // Goal: release builds abort on panic, so this text is the only trace a
        // panic on a background thread will ever leave. Without the backtrace it
        // names the panic but not the path that reached it.
        let report = panic_report("panicked at src/app.rs:12:5:\nqueue_pos out of range");
        assert!(report.contains("queue_pos out of range"), "{report}");
        assert!(report.contains("src/app.rs:12:5"), "{report}");
        assert!(report.contains("backtrace"), "{report}");
    }

    #[test]
    fn recording_a_panic_is_safe_with_no_logger_installed() {
        // Goal: the hook runs even when --log-level off left no logger behind,
        // and a panic inside the panic hook aborts with nothing at all.
        record_panic("panicked at src/main.rs:1:1:\nnowhere to write");
    }
}
