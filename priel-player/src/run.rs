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

//! Run a command under a deadline and keep what it wrote to stdout.
//!
//! The one place in the workspace that shells out and waits for an answer, so
//! the rules live here rather than at the call site: a missing program is an
//! answer rather than an error to propagate, the wait is bounded, and a child
//! that outlives its deadline is killed rather than left behind.
//!
//! It **must not run on the render thread.** Waiting up to the timeout is the
//! whole point of it, and that is exactly what the UI thread may not do.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Why a command produced no usable output.
///
/// Every variant is something the caller shows rather than something it can
/// retry, but they stay distinct so the sentence on screen can say which of
/// them happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunError {
    /// The program is not on `PATH`. The common case, and not a fault.
    NotFound,
    /// It exists but could not be started - permissions, usually.
    Spawn(String),
    /// It was still running at the deadline and has been killed.
    TimedOut,
    /// It ran and exited non-zero.
    Failed(Option<i32>),
    /// It ran, but the pipe could not be drained.
    Unreadable,
}

/// Anything past this is not a readout any more, it is a runaway.
///
/// A `pw-dump` of a busy graph is a few hundred kilobytes; this leaves two
/// orders of magnitude of headroom and still bounds a program that never stops
/// writing.
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

/// How long a child gets to exit after it has closed stdout, and how often that
/// is checked. A well-behaved one is already gone by the first poll.
const REAP_GRACE: Duration = Duration::from_millis(200);
const REAP_POLL: Duration = Duration::from_millis(5);

/// Run `program` with `args`, waiting at most `timeout` for its stdout.
///
/// stderr is discarded: the caller has an exit status to report and no screen
/// space for a subprocess's own diagnostics.
pub(crate) fn capture(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Vec<u8>, RunError> {
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NotFound),
        Err(e) => return Err(RunError::Spawn(e.to_string())),
    };

    // The pipe has to be drained by someone other than whoever is watching the
    // clock: a child writing more than the pipe buffer blocks until it is read,
    // so polling for exit without reading would hang on exactly the large
    // outputs this exists to collect.
    let Some(stdout) = child.stdout.take() else {
        return Err(abandon(&mut child, RunError::Unreadable));
    };
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut buf = Vec::new();
            let read = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
            let _ = tx.send(read.map(|_| buf));
        });
    if reader.is_err() {
        return Err(abandon(&mut child, RunError::Unreadable));
    }

    // The reader closes the channel at end of file, so this returns as soon as
    // the child stops writing - the timeout is a ceiling, not a delay.
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(abandon(&mut child, RunError::Unreadable));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(abandon(&mut child, RunError::TimedOut));
        }
    };
    reap(&mut child).map(|()| out)
}

/// Wait for a child that has closed stdout, without waiting forever.
///
/// `wait` on its own would be simpler and is a hang: a child still writing past
/// `MAX_OUTPUT_BYTES` blocks on a pipe nobody is draining any more, and never
/// reaches the exit this would be waiting for.
fn reap(child: &mut std::process::Child) -> Result<(), RunError> {
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(RunError::Failed(status.code())),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(REAP_POLL),
            Ok(None) => return Err(abandon(child, RunError::TimedOut)),
            Err(e) => return Err(RunError::Spawn(e.to_string())),
        }
    }
}

/// Kill a child and reap it, so nothing is left running behind a failure.
fn abandon(child: &mut std::process::Child, why: RunError) -> RunError {
    let _ = child.kill();
    let _ = child.wait();
    why
}

#[cfg(test)]
mod tests {
    use super::{RunError, capture};
    use std::time::{Duration, Instant};

    #[test]
    fn a_program_that_is_not_installed_is_an_answer_not_a_hang() {
        // Goal: the likeliest outcome on a machine without the PipeWire tools.
        // It must come back at once and say which failure it was, so the
        // sentence on screen can be the useful one.
        let e = capture("priel-no-such-program-exists", &[], Duration::from_secs(5));
        assert_eq!(e, Err(RunError::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_overruns_its_deadline_is_killed_rather_than_waited_on() {
        // Goal: the deadline is the only thing keeping a wedged subprocess off
        // the worker thread, so it has to fire, and the call has to return near
        // the deadline rather than near the child's own lifetime. Uses `sleep`,
        // never `pw-dump`: the suite must not need a PipeWire session.
        let start = Instant::now();
        let e = capture("sleep", &["30"], Duration::from_millis(150));
        assert_eq!(e, Err(RunError::TimedOut));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the deadline must cut it short, not the child finishing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_is_reported_with_its_status() {
        // Goal: pw-dump exits non-zero when there is no session to dump, which
        // wants a different sentence from it not being installed.
        let e = capture("false", &[], Duration::from_secs(5));
        assert!(
            matches!(e, Err(RunError::Failed(_))),
            "expected a failed exit, got {e:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdout_larger_than_a_pipe_buffer_comes_back_whole() {
        // Goal: a real pw-dump is hundreds of kilobytes, far past the 64 KiB
        // pipe buffer. Watching the clock without draining the pipe deadlocks
        // on exactly that, so the regression is worth pinning.
        let out = capture(
            "sh",
            &["-c", "yes priel | head -c 200000"],
            Duration::from_secs(10),
        );
        assert_eq!(
            out.map(|b| b.len()),
            Ok(200_000),
            "every byte, not one pipe buffer's worth"
        );
    }
}
