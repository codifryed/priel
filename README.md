# priel — hi-res terminal client for TIDAL

A mouse-first terminal client with a complete VIM keyboard, and bit-perfect,
rate-following playback. Blocking HTTP (ureq, no async runtime) + libmpv over a custom
`stream_cb` segment protocol → PipeWire node-targeted output.

> **Unofficial software.** priel is not affiliated with, endorsed by, or
> sponsored by TIDAL or Aspiro AB. TIDAL is a trademark of its respective owner
> and is named here only to describe what this client connects to. An active
> TIDAL subscription is required. priel does not circumvent access controls and
> has no offline-export or download feature.

*A **Priel** is a tidal channel in the Wadden Sea — the creek that carries the
water in and out of the flats twice a day, on the pull of the moon.*

## Why this one is different

- **Bit-perfect on purpose, and it tells you.** mpv runs with
  `gapless-audio=weak`, so a track at a different sample rate *reinitialises the
  output* instead of being resampled to whatever the last one used. You pay a
  short gap at a rate change and get the bits you paid for. Within a sample rate
  the next track is preloaded and the transition is gapless.
- **A bit-perfect indicator that reads the hardware, not the promise.** priel
  reads the ALSA device's live parameters from `/proc/asound` and judges against
  those. That matters: PipeWire will accept a 44.1 kHz stream, report 44.1 kHz
  back, and clock the card at 48 kHz — every player that trusts the audio server
  shows a green light through that. When the device is readable the badge says
  `DAC S32_LE · 48 kHz`; when it is not, it says `OUT` and admits it is only
  reporting what the server accepted.
- **Three honest grades, not a binary.** `✓ bit-perfect` when nothing alters the
  samples. `≈ near bit-perfect` when only the *level* changed — digital
  attenuation costs about one bit per 6 dB, which most people trade for a volume
  key, and lumping it in with a resample would make the indicator useless.
  `⚠ resampled 44→48 kHz` or `⚠ truncated to S16` when the stream is being
  rebuilt. A wider container is not a fault: 24-bit content in an `S32` frame is
  exactly how a USB DAC expects it, so that reads green.
- **Unity gain is a first-class state.** Any software volume below 100%
  multiplies every sample. The header shows `100%` in green at unity and yellow
  otherwise, `0` restores it, and both stages are watched — priel's own volume
  and the audio server's volume for our stream. Enthusiasts: leave both at unity
  and set level on the DAC.
- **Mouse-first, and it shows.** Clickable view tabs and transport controls, a
  scrubbable progress bar, wheel scrolling, double-click to play — and every key
  shown in the bottom hint row is itself a button. If an action cannot be done by
  pointing at it, that is a bug rather than a design choice.
- **A complete VIM keyboard, not the leftovers.** `j`/`k` and the arrow keys,
  `g`/`G`, `J`/`K` and `Ctrl-D`/`Ctrl-U`, `/` to filter. `?` opens the full
  reference rather than making you read this file. Parity runs both ways: there
  is no control that only the mouse can reach, and none that only the keyboard
  can.
- **A dependency list you can actually audit.** No async runtime anywhere in the
  tree — no tokio, no hyper. No OpenSSL: TLS is rustls. Under 40 crates for the
  API library and around 100 for the whole binary. **libmpv is the only non-Rust
  runtime dependency**, and it is the one doing the work that matters.
- **Starts playing quickly.** Segment bytes reach the decoder as they arrive
  rather than a segment at a time, so a track begins when the first chunk lands
  instead of after several megabytes. On a slow link mpv pauses on underrun and
  resumes with a couple of seconds in hand, which adapts to the connection
  instead of taxing every fast one with a fixed pre-buffer.
- **Small and quiet at rest.** A ~5 MiB binary that redraws only when something
  on screen actually changed, backs its player thread off when nothing is
  playing, and holds a bounded window of each track rather than all of it: the
  download parks when it runs too far ahead of the decoder, and bytes that have
  been played are released as playback moves past them. A hi-res track costs
  about 40 MiB while it plays however long it is, rather than hundreds of
  megabytes by the end of it.
- **Built to survive the boring failures.** Zero `.unwrap()` calls in the
  workspace; poisoned locks are recovered rather than propagated, because mpv
  invokes our callbacks across an FFI boundary where unwinding is undefined
  behaviour. The test suite runs with no network, no credentials, no audio
  device and no terminal.
- **Packaged like a native tool.** A generated man page and bash, zsh and fish
  completions, plus a `Makefile` that honours `DESTDIR`/`PREFIX`, so a distro
  packager needs no patch.
- **Stays inside the lines.** No download or offline-export feature, no attempt
  to work around access controls. It plays the subscription you already have.
- **Library-first.** The API and player crates contain no UI code, so a GUI
  frontend can be added later as a second binary sharing both.

Next up, and the reason this list has a gap: **PipeWire setup assistance** —
detecting your `allowed-rates` configuration, explaining what a bit-perfect
chain needs, and showing the sink's *live* bit depth and sample rate. See the
roadmap below.

## Install

Needs Rust ≥ 1.88, `libmpv` (`mpv-devel` / `libmpv-dev` to build), a working
PipeWire or ALSA setup, and a subscription.

**Signing in.** On first run priel opens your browser, you sign in, and you land
on a page that looks like an error — that is expected. Copy its address, paste it
back into priel, and you are in. The session is renewed automatically from then
on; if it ever lapses, `A` signs in again.

priel follows the XDG layout. Your session and the client key it obtained are
runtime state, closer to a persisted cookie than to a setting, so they live in
`~/.local/state/priel/`. priel never reads or writes another application's
files, and it moves its own out of the old location once, so an existing session
is not lost. There is no flag for the session path: pointing priel at a file
another application owns would rewrite it on every token refresh.

`~/.local/state/priel/priel.log` is the diagnostic log, started fresh each run
and holding warnings and errors by default. `--log-level debug` (or
`PRIEL_LOG=debug`) is what to attach to a bug report; `--log-level off` keeps no
file at all. mpv's own messages are recorded in the same file, in order, so a
failed track shows both halves together — `[file] Cannot open file ...` next to
priel's own account of what it was trying to play.

Set `PRIEL_NO_BROWSER=1` to stop priel launching a browser; the sign-in screen
always shows the URL as well, so a headless or remote session still works.

**priel ships no client credentials.** On first run it asks whether to download
one from the open-source project the other native Linux players rely on, saying
plainly what it fetches and where it saves it. Decline and priel still runs, just
without the ability to renew a session. With a client identity present priel
renews the access token by itself — before expiry, and again if a request is
rejected early.

```bash
make check-deps        # verify cargo and libmpv are present
make                   # release build
sudo make install      # binary, man page, completions, licence
```

`make help` lists every target. Install paths follow the GNU conventions, so
`make PREFIX=/usr DESTDIR=/tmp/stage install` stages cleanly for a package.

```bash
make run ARGS="--device pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0"
```

`--device` is optional; the default sink is used when it is omitted. `priel
--list-devices` prints every device with the identifier `--device` takes, and
`d` opens the same list inside the player. Both include the direct hardware
devices — `alsa/hw:CARD=AUDIO,DEV=0` and the like, marked *(direct hardware
access)* — which ALSA advertises nowhere: it publishes only the plugin spellings
of a card, so priel builds those entries from the kernel's own card listing.
They are the outputs where `--exclusive` means something, so they are the last
ones that should have needed looking up. `--log-level` and `--log-file` control
the diagnostic log. See `man priel` or `priel --help`.

```bash
make run ARGS="--device alsa/hw:CARD=AUDIO,DEV=0 --exclusive"
```

`--exclusive` asks for the device to be priel's alone, taking it out of the
sound server's graph so nothing else on the machine can play through it or
reshape the chain underneath you. It is deliberately separate from `--device`:
choosing a hardware device does not imply taking it, and **priel never selects
the exclusive path on its own** — it silences every other application, and that
is not a side effect of pressing play. `x` in the device picker toggles the same
thing for one session. The `D` overlay then says there is no graph at all, which
on this path is the ideal rather than a fault: nothing sits between priel and the
DAC.

If the device will not open exclusively, usually because something else already
holds it, priel says so, records it in the log, and keeps playing. There is no
shared spelling of a `hw:` device — the card is the whole of it — so the fallback
is the sound server's own entry for **the same card**: the same physical DAC,
just shared. Failing that, the system default sink. The track restarts from the
beginning, and the badge reads `⚠ shared · exclusive refused` rather than
claiming a connection it does not have. See
[ADR-0001](docs/adr/0001-exclusive-output-is-asked-for-never-assumed.md).

For UI work or a machine without mpv headers, `make build-nolibmpv` compiles the
interface with playback stubbed out.

## Keys & mouse

Press `?` in the app for the complete reference. Every key listed in the bottom
row of the interface is also clickable.

| Action | Keyboard | Mouse |
|---|---|---|
| Full key reference | `?` | click `[?]` |
| Recent log messages | `M` | scroll to page back |
| Audio graph to the device | `D` | click `[D]` |
| Choose the output device | `d` | click a row in the picker |
| Exclusive output on/off | `x` in the picker | click the toggle |
| Sign in again | `A` | — |
| Switch view | `Tab` cycles, `1`/`2`/`3` | click a tab |
| Move selection | `j`/`k`, `↑`/`↓` | scroll wheel |
| First / last | `g` / `G` | click `[g/G]` |
| Page up/down | `J`/`K` full, `Ctrl-U`/`Ctrl-D` half | — |
| Open playlist / back | `Enter` / `Esc` | double-click |
| Play selected | `Enter` | double-click a row |
| Play / pause | `Space` | click `▷` / `‖` |
| Seek ±5s | `h`/`l`, `←`/`→` | click or drag the progress bar |
| Previous / next track | `H`/`L`, or `p`/`n` | click `|◁` / `▷|` |
| Filter the current list | `/`, type, `Enter`/`Esc` | click `[/]` |
| Reload the current list | `r` | click `↻` |
| Search the catalogue | `3`, type, `Enter`; `i` to re-edit | — |
| Shuffle the current view | `s` | click `⇄` |
| Volume | `+` / `-` | click `-` / `+` |
| Restore unity gain | `0` | click the percentage |
| Quit | `q` | click `[q]` |

`Esc` cancels: it leaves a filter or search box, and steps back out of an opened
playlist. It never quits.

## Status

Working: favorites, playlists and catalogue search, each paged in as the
selection nears the end of the loaded rows and reloadable with `r`; local
filtering; hi-res resolution and playback (24/192 via progressive segment
streaming); a gapless
play queue with a preloaded next track; shuffle with auto-advance; play, pause,
seek, skip and volume; a now-playing bar with a scrubbable progress bar, a live
DAC badge and a bit-perfect indicator; the `?` reference overlay; a diagnostic
log with an `M` overlay for reading it without leaving the player; a `D` overlay
listing the PipeWire nodes between priel and the device with the rate and format
each one negotiated, marking the node where the track's rate or width is first
lost with a `⚠`; a `d` picker for moving the output between devices, with an `x`
toggle for taking a device exclusively.

The `D` overlay names a node only when the chain accounts for what was measured.
Where the device is clocked elsewhere and every node on the path still reports
the track's own rate — a resample the sound server did inside a node rather than
between two of them — it says the change is unaccounted for rather than blaming
the nearest candidate. A wrong name would send you to reconfigure something that
was working.

Roadmap, roughly in order:

- **PipeWire configuration help.** The `D` overlay names the node that altered
  the samples. What is left is the advice: detect and explain the
  `allowed-rates` setup a bit-perfect chain needs, and say which application is
  holding the device when one is.
- **ALSA setup helpers, for true bit-perfect.** The direct path itself is
  built — `--device alsa/hw:...` with `--exclusive`, or the `x` toggle in the
  picker. What is left is the guidance around it: detect when a device is
  already claimed by PipeWire, and explain how to reserve it.
- **A cleaner sign-in.** The redirect lands on the vendor's own page, which priel
  cannot listen on, so the flow ends with a paste. A client registered with a
  loopback redirect would remove that step; the developer terms do not currently
  permit a native player, so it stands.
- MPRIS, cover art (kitty/sixel).
- **Spectrum visualiser**, if it can coexist with bit-perfect output.

## Workspace

```
priel-core     lib  — API access + hi-res stream resolution (blocking, UI-agnostic)
priel-player   lib  — embedded libmpv player, thread-owned, stream_cb protocol
priel-tui      bin  — ratatui frontend (builds the `priel` binary)
```

Development uses `make check` (formatting, clippy at pedantic over both feature
configurations, and the full test suite) and `make coverage`. Style and testing
rules live in [`RUST_STYLE.md`](RUST_STYLE.md).

## Packaging (Linux-first)

- **Single binary** `priel`; runtime dependencies are libmpv (`libmpv.so.2`) and
  a working PipeWire/ALSA. TLS is rustls, so there is **no OpenSSL** dependency
  and **no async runtime** in the tree.
- `make install` places the binary, `priel.1`, bash/zsh/fish completions, the
  licence and the README, honouring `DESTDIR`, `PREFIX`, `BINDIR`, `MANDIR` and
  the completion directories.
- The man page and completions are **generated from the same clap definition the
  binary parses with** (`make assets`), so they cannot drift from reality.
- `make dist` produces a source tarball; `make vendor` vendors the crates for an
  offline build. `CARGO_FLAGS` defaults to `--locked`.
- Minimum supported Rust is 1.88, declared as `rust-version` in the workspace.
- No trademarked term appears in the package name, binary name, crate names, or
  any identifier — the service is named only in prose describing what the client
  talks to. Do not ship the TIDAL logo, wordmark, or brand typography with the
  package, and keep the disclaimer above in the RPM `%description`.
- **Cross-OS later** is feasible: crossterm, ratatui, ureq, rustls and libmpv are
  all cross-platform (libmpv via brew/scoop). There is no Linux-only code in the
  crates; the player reads the output rate from mpv properties.

## License

Copyright (C) 2026 Guy Boldon

This program is free software: you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <https://www.gnu.org/licenses/>.

The full license text is in [`COPYING`](COPYING).
