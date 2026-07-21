# Rust Style for priel

The authoritative style guide for this workspace: `priel-core`, `priel-player`, `priel-tui`.

Design goals in priority order: **correctness, responsiveness, developer experience.**

"Responsiveness" is the domain constraint here. Two deadlines must never be missed: the render
thread must never block on I/O, and mpv's read callback must never stall longer than the audio
buffer. Everything below serves those two.

## Test-Driven Development

**TDD is the preferred workflow.** Write the failing test first, make it pass, then refactor.

- Red: write a test that expresses the intended behaviour and watch it fail for the right reason.
  A test that passes before the implementation is testing nothing.
- Green: the simplest change that passes.
- Refactor: with the test as a net. This is where the design improves, not an optional third step.
- When fixing a bug, the regression test comes first and must fail against the unfixed code.

**Line coverage stays above 80%**, which `make coverage` reports. That is a floor to notice, not a
number to chase: a change that takes it under wants a reason, and the last few points are usually
glue that needs a TTY or a real device to reach. New code is held to TDD regardless of what the
figure says.

### The seams that exist

Test at the seam, not through the whole app. The ones already built, which new tests should reuse
rather than reinvent:

- **`Client::with_base_url`** points the API client at a stub origin. The `priel-core` tests run a
  `std::net` HTTP stub; no mock framework, no dependency.
- **`Player::new(Some("null"))`** gives a real mpv handle on the null output, so command handling,
  property reads and the protocol callbacks are all testable headlessly.
- **`App::rigged()`** returns an app with a silent player plus both ends of the worker channels, so
  a test can post `FromWorker` replies and assert on the `ToWorker` requests the app makes.
- **`worker::spawn_with`** takes a client factory, so the worker loop runs against a stub.
- **`EventSource`** (in `main.rs`) lets the event loop be driven by a scripted sequence.
- **`TestBackend`** renders real frames; `ui` tests assert on the resulting text and on the hit
  boxes the renderer publishes.

- **`App::decide`** takes a `Tick` snapshot and returns a `Plan`, so the queue-advance guards are a
  table of tests rather than comments pleading with the reader. Note what that extraction found:
  the three decisions are *independent*, not a priority chain, and collapsing them into one - which
  reads better - stalls a track whose preload failed and, with shuffle on, stops advancing
  altogether.

Where a seam does not exist yet, adding one is the preferred fix.

### Running tests

- `make test-all` runs both feature configurations; `make coverage` reports line coverage.
- Tests must pass with no network, no credentials, no audio device and no TTY. Anything reaching
  the network points at `127.0.0.1`, and mpv runs on the null output.
- Tests needing a live account, real hardware output, or the network are `#[ignore]`d, with the
  reason in the ignore reason string. They are run deliberately, never in the default suite.
- `cargo test -p priel-tui --no-default-features` exercises app logic against the stub backend with
  no mpv headers present. Keep that path green: it is the one that works everywhere.
- Each test opens with a short comment stating its goal and method, so intent survives without
  reverse-engineering the assertions.
- Cover the negative space too: malformed manifests, an empty queue, a filter matching nothing, a
  zero-length track. The API lies and the network truncates.
- **A test binary can die on a signal instead of failing a test.** `cargo` then prints no `FAILED`
  line and no assertion - only a non-zero exit and a `signal: 11` buried in its error. Grepping the
  log for a failing test finds nothing, which reads as "could not reproduce" and is how a
  use-after-free across the mpv FFI boundary survived a day and six core dumps. If the gate fails
  with nothing named, run `make check-signals`, and look at `coredumpctl list | grep priel` before
  concluding anything. The stack trace names the faulting frame; guessing does not.

## Correctness

### Control flow

- Simple, explicit control flow only. No recursion.
- Put a limit on everything. Every loop and every collection that grows from external input needs a
  bound. `favorite_tracks`, `playlist_tracks` and `search` already take explicit limits; keep that
  pattern rather than fetching "everything".
- State invariants positively: `if index < len` reads better than `if index >= len`.
- Split compound conditions where each case deserves its own reasoning. The four-term condition
  guarding the end-of-track fallback in `refresh()` is at the limit of what is readable, and is the
  strongest argument for extracting it per the TDD section above.

### Errors, panics and `.unwrap()`

- **Avoid `.unwrap()` entirely.** Not just on domain data - API responses, manifests, user input
  and file contents are obviously `Result` with `.context()`, but lock poisoning is not an excuse
  either. The workspace currently contains **zero** `.unwrap()` calls. Keep that number at zero.
- Poisoned locks are recovered, not propagated: `.unwrap_or_else(PoisonError::into_inner)`. The
  `lock()` / `wait()` helpers in `backend_mpv.rs` exist for exactly this and document why it is
  sound. This is not merely tidier - mpv invokes the protocol callbacks across an FFI boundary,
  where unwinding is undefined behaviour, so a panicking lock there is a real bug.
- Where an unwrap is genuinely unavoidable or expensive to remove, use `.expect("...")` with a
  message stating the invariant, plus a comment saying why it cannot fail. An `.expect` is a claim
  you are making to the next reader; make it checkable.
- Errors from the network layer stay errors. Do not `assert!` your way out of a bad API response.
- `panic!`/`assert!` are for programmer errors only. Note that `main.rs` installs a panic hook that
  restores the terminal, so a failed assertion prints normally instead of wrecking the user's shell.
  That is a safety net, not a licence.

### Assertions

- Assert internal invariants, never external data. Good targets: `queue_pos < queue.len()`, the
  player thread's `entries` staying in lockstep with mpv's playlist, visible-index bounds before
  indexing the backing vec.
- Split compound assertions: `assert!(a); assert!(b);` localises the failure.
- Prefer `debug_assert!` inside the player tick and the mpv read callback; prefer `assert!` at the
  boundaries where state is handed between threads.
- An invariant worth asserting is usually worth a test. Write both.

### Threads, locks and the audio deadline

priel is **blocking and thread-based; there is no async runtime anywhere in the tree.** Not in the
code, and not transitively - the HTTP client is `ureq` precisely so that no executor is linked in.
Do not introduce one, and treat a dependency that drags in Tokio as a dependency you are not adding.

That is a deliberate fit for the domain, not inertia. libmpv's `stream_cb` `read` callback is a
synchronous C function invoked from mpv's own demuxer thread, and it must block until bytes are
available - you cannot `.await` there. The buffer would stay `Mutex` + `Condvar` under any model, so
async would buy nothing at the one boundary where it would have to earn its keep.

- Four long-lived threads (UI, worker, player, log writer), a fifth for the session bus **only when
  there is one**, plus one downloader per buffered track.
  Ownership is strict: only the worker touches `Client`, only the player thread touches `Mpv`. Cross-thread
  communication is `mpsc` plus the `Arc<Mutex<PlaybackStatus>>` snapshot. Keep it that way; a second
  path to the same state is how the queue desynchronises.
- **Never block the UI thread.** No HTTP, no file I/O, no waiting on a lock the player thread can
  hold for long. `Player` commands are fire-and-forget sends by design; do not add a
  request/response call that makes the UI wait for the player.
- **The mpv `read`/`seek` callbacks run on mpv's thread and block playback.** Hold exactly one lock,
  do no I/O, allocate nothing beyond the copy itself, and wait only on the buffer's own `Condvar`.
  Taking any other lock in there risks an audible dropout or a deadlock.
- Keep lock scopes tight and never hold two at once. The registry lock and a buffer lock must not
  nest.
- Do not react directly to external events by spawning work. Downloader threads are spawned per
  registered source and bounded by the playlist depth; keep it that way.

### `unsafe` and FFI

- There are two `unsafe` blocks in the workspace, both in `backend_mpv.rs` and both inherent to a
  libmpv API that libmpv2 does not wrap safely. The second is `request_log_messages`, a single
  `mpv_request_log_messages` call whose `// SAFETY:` comment covers the handle's lifetime and the
  C string's. The first is `Protocol::new`, whose comment covers the three things that make it
  sound: the callbacks capture nothing and receive an `Arc` that outlives them, all shared state is
  behind mutexes so concurrent calls from mpv's threads are fine, and **the registration is never
  dropped**. Keep that comment true if the code around it moves.
- **A registration handed to libmpv is leaked deliberately, and must stay leaked.** libmpv has
  `mpv_stream_cb_add_ro` and no remove: a protocol lives as long as the handle. libmpv2's
  `Protocol` frees the callback data in `Drop` regardless, and borrows the `Mpv`, so the borrow
  checker forces it to drop *first* - freeing that data while mpv's demuxer threads can still call
  `open`. `std::mem::forget` is the fix and the leak is one small box per handle. This is not
  theoretical: it produced six SIGSEGV core dumps in a day, every one faulting in `Mutex::lock`
  inside `open`, called from mpv's `open_demux_thread`. An earlier version of this guide advised
  the opposite ordering, which is how it survived.
- Any new `unsafe` needs a `// SAFETY:` comment naming the invariant that makes it sound.
- Nothing reachable from an FFI callback may panic. That constraint is why the poison-tolerant
  `lock`/`wait` helpers exist; do not reintroduce a panicking path there.
- Confine `unsafe` to the FFI boundary. It must not appear in `priel-core` or `priel-tui`.

### Memory

- A track costs a bounded window of RAM per play, not all of itself: the downloader parks when it
  is `DOWNLOAD_AHEAD_MAX` ahead of the reader and `trim` releases everything but `KEEP_BEHIND_MAX`
  behind it. Do not make it worse by buffering more tracks ahead than the queue needs. The preload
  depth is one.
- Give capacity hints when the size is known: `Vec::with_capacity(limit)` when building results
  from a listing whose limit you passed in.
- `LazyLock` for anything compiled once and reused, as `mpd`'s `Regex`es are. Note the second
  benefit there: with the patterns compiled once, the only way `parse` can fail is the one that
  means something - a manifest with no media template - instead of also carrying "a literal regex
  did not compile" that no caller could ever act on.
- Per-tick allocation in the player loop or the render path is a red flag. Allocation in `fn new`
  or a one-shot resolve is fine.

### Diagnostics

priel owns the whole terminal, so anything printed to stderr lands on the alternate screen
and is lost. Diagnostics go to `$XDG_STATE_HOME/priel/priel.log` through the sink in
`priel-tui/src/logging.rs`.

- The libraries use the `log` facade and nothing else. They still return errors rather than
  printing them, and the facade costs a future GUI frontend nothing: it installs its own sink.
- **Logging never does I/O on the calling thread.** A record is formatted and posted to a
  bounded queue that one writer thread drains. A full queue drops records and reports how
  many; it must never block the UI or the player. `Logger::flush` is the single deliberate
  round-trip, and it exists for the way out.
- **Nothing may log from mpv's `read`/`seek` callbacks.** Formatting allocates, and those
  callbacks may not. Log at the edges instead - the protocol open, the segment fetched, the
  starved buffer - which is where the useful data is anyway.
- **Every thread priel starts is named**, via `thread::Builder::new().name(..)`: the log records
  which thread wrote each line, and `-` places nothing. That makes spawning fallible, and none of
  those failures is a panic - each site has a caller that already copes with the thread being
  absent, from `Player::with_config` returning `Err` to the app's own worker-disconnect check.
- A log line is developer-facing and `App::notice` is user-facing. A failure the user has to
  act on needs both; neither substitutes for the other, and neither substitutes for a typed
  error where the caller has to branch.
- Release builds are `panic = "abort"`, so the panic hook in `main.rs` is the only chance to
  record a panic on any thread. It writes and flushes before restoring the terminal.

## Developer Experience

### Naming

- `snake_case` for functions, variables, modules; `CamelCase` for types. Proper acronym casing:
  `MpdInfo`, `BtsManifest`, not `MPDInfo`.
- **Include units in names**, most significant first: `cache_secs`, `sample_rate`, `bit_depth`,
  `position`, `Track::duration_secs` and `Playlist::duration_secs`. The last two used to be
  `duration` with the unit in a trailing comment, next to a `PlaybackStatus::duration` that is
  seconds as `f64` rather than `u32` - two fields that read as the same thing and were not. The
  wire DTOs keep the name the service sends; the rename happens where the domain type is built.
- Do not abbreviate outside well-known domain terms (`mpd`, `dash`, `bts`, `flac`, `mpv`, `pcm`).
- `index` (0-based), `count` (1-based) and `size` are distinct concepts; do not let them share a
  name. The visible-index vs backing-index distinction in `app.rs` is the live example - name those
  variables so the reader can tell which space they are in.

### Visibility and API surface

- Two of these crates are libraries with a deliberate public surface, and a GUI frontend is planned
  against them. Export what a frontend needs and no more.
- `pub(crate)` is correct and encouraged for internals - `Cmd` in `priel-player` is exactly right.
  Do not widen it to `pub` for convenience.
- Every `pub` item in `priel-core` and `priel-player` carries a doc comment. `priel-tui` internals
  need doc comments only where the reason is non-obvious.
- The libraries contain **no UI code and no printing**. Errors are returned to the caller, or - on
  the player thread and the downloaders, which have no caller to return to - recorded with `log`.
  Nothing in `priel-core` or `priel-player` writes to stdout or stderr: the terminal belongs to the
  TUI, and an `eprintln!` there lands on the alternate screen and is lost.

### Function length and ordering

- **Soft limit of 70 lines per function.** Push `if`/`match` up into the parent, push loops and
  iterator chains down into helpers. `backend_mpv::spawn` is the standing outlier at ~90 lines, and
  the reason is worth knowing before trying again: `Protocol<'parent>` borrows the `Mpv`, so the two
  cannot be built in a helper and returned together, and their drop order is what unregisters the
  protocol while the handle is still alive. About a third of what is left is the `// SAFETY:` block
  explaining that. Getting under the limit needs the thread's mutable state moved into a struct so
  the loop can take `&mut self`, not another extraction.
- `pub fn new` leads the `impl` block, then core logic and per-tick paths, then rare helpers.
- Keep leaf functions pure where you can; let the parent own the state. This is also what makes
  them testable, so it pays twice.

### Errors across layers

- `anyhow` with `.context()` is the house error type. Context strings say what was being attempted
  and, where useful, what the user can do: the existing "not signed in?" and "session expired? log
  in again" are the model.
- **Do not stringify errors to pass them between layers** where the receiver needs to branch.
  `FromWorker::Failed { fault, detail }` is the pattern: `priel_core::Fault` says what kind of
  failure it was and the string is only ever displayed. The classification belongs in the layer
  that *knows* - only the core can tell a refused session from a dropped connection - and nothing
  above it may match on `detail`. This replaced `e.contains("log in again")`, which made a sentence
  in the core load-bearing: rewording it would have silently stopped the login screen appearing.
- Simplify return types. `()` beats `bool` beats `Option<T>` beats `Result<Option<T>, E>`. Every
  extra dimension multiplies the branches at the call site.

### Comments

- Say **why**, not what. The guards in `refresh()` are the standard to match: they record the bug
  that motivated them, so nobody "simplifies" them back into existence.
- Comments are sentences with a capital and a full stop; end-of-line comments may be phrases.
- Every file keeps its SPDX header and GPL banner.
- Module-level `//!` docs state what the module owns and what it must not do.

### Dependencies

- The dependency list is small and deliberate: rustls over OpenSSL to keep packaging simple, `ureq`
  because it is blocking and pulls in no executor, libmpv behind a default-on feature, `clap`
  because a generated man page and shell completions are worth more than a hand-rolled parser.
  Adding a crate needs a reason that beats writing the small version in-house.
- `libmpv2-sys` is a direct dependency of `priel-player` only because libmpv2 leaves
  `mpv_request_log_messages` unwrapped. It is not a new crate or a new system library - libmpv2 is a
  thin wrapper over it and nothing else - but it is a version to keep in step with libmpv2's own.
  A mismatch is a compile error, not a silent one.
- Build-time-only crates belong behind a feature, as `clap_mangen` and `clap_complete` are behind
  `gen-assets`. A tool that produces packaging artefacts has no business in the shipped binary.
- Anything new must not break: the `--no-default-features` build, the no-OpenSSL guarantee,
  the no-async-runtime guarantee, or cross-platform buildability (crossterm, ratatui, rustls and
  libmpv are all portable; keep it so).
- **HTTP is HTTP/1.1 only, and that is the right tool.** Segment fetches are a handful of
  multi-megabyte GETs to one CDN host; parallel keep-alive connections give each its own congestion
  window, where HTTP/2 would multiplex them onto one and add connection-level head-of-line
  blocking. Concurrency for downloads comes from more connections, not from multiplexing.
- A new dependency in `priel-core` or `priel-player` is also a new dependency for every future
  frontend. Weigh it there especially.

### Formatting and lints

- `rustfmt.toml` pins `style_edition = "2024"`, 100 columns, 4-space indent. It uses stable options
  only, so `cargo fmt` behaves identically on stable and nightly. **Run `cargo fmt` before every
  commit**; `cargo fmt --check` must pass.
- **Clippy pedantic is on**, via `[workspace.lints.clippy]` in the root manifest with
  `[lints] workspace = true` in each crate. `cargo clippy --all-targets` must be warning-free, and
  so must `cargo clippy -p priel-tui --no-default-features --all-targets`.
- Suppress a lint only at the narrowest scope that works - the item, never the crate - and always
  with `reason = "..."`. The existing allows are the model: they name the ABI or the display-only
  rounding that makes the cast correct. A bare `#[allow]` with no reason is not acceptable.
- Prefer fixing over allowing where a real conversion exists: `u32::try_from(..).ok()` for a value
  that could be out of range, `i8::from_ne_bytes([b])` when reinterpreting a byte rather than
  numerically converting it.
- MSRV is 1.88, declared as `rust-version` in `[workspace.package]`. It is **the lowest toolchain
  that builds, not the newest one installed**: the source uses let chains (stable from 1.88 in
  edition 2024) and the dependency tree declares 1.88 in seven places. Raise it only when something
  concrete needs it, and say what in the commit subject - every version above the floor excludes a
  packager for nothing. Verify a change with `cargo +<msrv> check --workspace --all-targets
  --locked`; the metadata alone will not tell you, since a language feature can raise the floor
  without any dependency doing so. Do not raise it for convenience - clippy honours it and will stop suggesting newer APIs. Distro
  packaging is a goal, and distro toolchains lag.

### Naming and the trademark constraint

The service name must not appear in crate names, binary names, module names, type names, field
names, or feature names. It appears only in prose that describes what the client connects to.
`prielseg://`, `priel-core` and `PlayableSource` are the pattern. This applies to test names and
fixture filenames too.
