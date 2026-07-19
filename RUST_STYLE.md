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

The workspace currently has **no tests**. Filling them in is active work, so new code is held to
TDD from now on and existing code gets covered as it is touched. Do not treat the absence of
neighbouring tests as precedent.

### Where the seams are

Test at the seam, not through the whole app. In priority order:

1. **`priel_core::mpd::parse`** - a pure `&str -> MpdInfo` function. Zero setup. Segment-count
   arithmetic is exactly the off-by-one-prone logic that earns a test per branch: no `<S>` element,
   `<S>` without `r=`, `<S r="N">`, several `<S>` runs, and a missing `media=` template (error case).
2. **`quality_label` and `decode_manifest`** - private, so test them in a `#[cfg(test)] mod tests`
   in the same file. `decode_manifest` covers both manifest arms (BTS and DASH) plus the
   unknown-mime and empty-urls errors from a base64 string fixture.
3. **The queue state machine** in `priel-tui/src/app.rs`. Highest value in the repo: this is where
   the runaway-advance bug lived. It is driven entirely by `PlaybackStatus` in and player commands
   out, so it can be tested as a pure transition function once extracted (see below).
4. **`visible()` / filter and selection index math** - pure over the backing `Vec`, and the
   visible-index indirection is easy to get wrong.

### Let testability drive the design

Where a seam does not exist yet, adding it is the preferred fix. Two known cases:

- **`App::refresh` should not need an `App`.** The advance logic reads `status`, `queue_pos`,
  `expected_id`, `current_target`, `next_intended` and `advanced`, and decides "preload", "advance
  fresh" or "do nothing". Extract that into a function taking those inputs and returning an intent
  enum; `App` then applies the intent. The guards documented in `CLAUDE.md` become table-driven
  tests instead of comments pleading with the reader.
- **`Client` hardcodes `const API` and builds its own `ureq::Agent`**, so nothing can point it at a
  local mock. Give it an injectable base URL and let the API tests run against a stub server.

### Running tests

- `cargo test` must pass with no network, no credentials, no audio device, and no libmpv.
- Tests needing a live account, real hardware output, or the network are `#[ignore]`d, with the
  reason in the ignore reason string. They are run deliberately, never in the default suite.
- `cargo test -p priel-tui --no-default-features` exercises app logic against the stub backend with
  no mpv headers present. Keep that path green: it is the one that works everywhere.
- Each test opens with a short comment stating its goal and method, so intent survives without
  reverse-engineering the assertions.
- Cover the negative space too: malformed manifests, an empty queue, a filter matching nothing, a
  zero-length track. The API lies and the network truncates.

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

- Three long-lived threads (UI, worker, player) plus one downloader per buffered track. Ownership
  is strict: only the worker touches `Client`, only the player thread touches `Mpv`. Cross-thread
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

- The only `unsafe` in the workspace is `Protocol::new` in `backend_mpv.rs`, inherent to the libmpv
  custom-protocol API. It carries a `// SAFETY:` comment covering the three things that make it
  sound: the callbacks capture nothing and receive an `Arc` that outlives them, all shared state is
  behind mutexes so concurrent calls from mpv's threads are fine, and `protocol` is declared after
  `mpv` so it unregisters first. Keep that comment true if the code around it moves.
- Any new `unsafe` needs a `// SAFETY:` comment naming the invariant that makes it sound.
- Nothing reachable from an FFI callback may panic. That constraint is why the poison-tolerant
  `lock`/`wait` helpers exist; do not reintroduce a panicking path there.
- Confine `unsafe` to the FFI boundary. It must not appear in `priel-core` or `priel-tui`.

### Memory

- A full track currently lives in RAM per play. This is a known limitation; do not make it worse by
  buffering more tracks ahead than the queue needs. The preload depth is one.
- Give capacity hints when the size is known: `Vec::with_capacity(limit)` when building results
  from a listing whose limit you passed in.
- `LazyLock` for anything compiled once and reused. The `Regex`es in `mpd::parse` are rebuilt on
  every call and belong in `LazyLock` statics; that also removes a fallible path from a hot-ish
  function.
- Per-tick allocation in the player loop or the render path is a red flag. Allocation in `fn new`
  or a one-shot resolve is fine.

## Developer Experience

### Naming

- `snake_case` for functions, variables, modules; `CamelCase` for types. Proper acronym casing:
  `MpdInfo`, `BtsManifest`, not `MPDInfo`.
- **Include units in names**, most significant first: `cache_secs`, `sample_rate`, `bit_depth`,
  `position` are right. `Track::duration: u32` carries its unit in a trailing comment instead of
  the name; prefer `duration_secs` when that struct is next touched. Note that `Track::duration` is
  seconds as `u32` while `PlaybackStatus::duration` is seconds as `f64` - exactly the confusion
  units-in-names prevents.
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
- The libraries contain **no UI code and no printing**. Errors are returned, never printed. The one
  exception is the player thread's `eprintln!` on a failed mpv init, which has no channel to report
  through; do not add more.

### Function length and ordering

- **Soft limit of 70 lines per function.** Push `if`/`match` up into the parent, push loops and
  iterator chains down into helpers. `backend_mpv::spawn` is the current outlier at ~75 lines and
  should shed its setup block into an `init_mpv` helper when next touched.
- `pub fn new` leads the `impl` block, then core logic and per-tick paths, then rare helpers.
- Keep leaf functions pure where you can; let the parent own the state. This is also what makes
  them testable, so it pays twice.

### Errors across layers

- `anyhow` with `.context()` is the house error type. Context strings say what was being attempted
  and, where useful, what the user can do: the existing "is hiresTI logged in?" and "token expired?
  re-login in hiresTI" are the model.
- **Do not stringify errors to pass them between layers** where the receiver needs to branch.
  `FromWorker::Error(String)` currently flattens everything into a notice line, so the UI cannot
  distinguish an expired token from a dropped network. When auth work lands, that becomes a typed
  variant so the UI can prompt a re-login. New worker messages should not add to the string pile.
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
  because it is blocking and pulls in no executor, libmpv behind a default-on feature. Adding a
  crate needs a reason that beats writing the small version in-house.
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
- MSRV is 1.85, the edition-2024 floor, declared as `rust-version` in `[workspace.package]`. Do not
  raise it for convenience - clippy honours it and will stop suggesting newer APIs. Distro
  packaging is a goal, and distro toolchains lag.

### Naming and the trademark constraint

The service name must not appear in crate names, binary names, module names, type names, field
names, or feature names. It appears only in prose that describes what the client connects to.
`prielseg://`, `priel-core` and `PlayableSource` are the pattern. This applies to test names and
fixture filenames too.
