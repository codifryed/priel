# priel — hi-res terminal client for TIDAL

A mouse-first terminal client for TIDAL with a complete VIM keyboard and
bit-perfect, rate-following playback. Blocking HTTP (ureq, no async runtime) and
embedded libmpv over a custom `stream_cb` segment protocol into PipeWire.

> **Unofficial software.** priel is not affiliated with, endorsed by, or
> sponsored by TIDAL or Aspiro AB. TIDAL is a trademark of its respective owner
> and is named here only to describe what this client connects to. An active
> subscription is required. priel does not circumvent access controls and has no
> download or offline-export feature.

*A **priel** is a tidal channel in the Wadden Sea — the creek that carries the
water in and out of the flats twice a day, on the pull of the moon.*

## Features

- **Bit-perfect playback that reads the hardware, not the promise.** priel judges
  fidelity against the ALSA device's live parameters from `/proc/asound`, not
  against what the sound server reports — because PipeWire will accept a 44.1 kHz
  stream, report 44.1 kHz, and clock the card at 48 kHz. The badge shows
  `DAC S32_LE · 48 kHz` when the device is readable and `OUT` when it can only
  report what the server accepted.
- **Three honest grades, not a binary.** `✓ bit-perfect`, `≈ near bit-perfect`
  (only the level changed), `⚠ resampled` / `⚠ truncated`. `✓?` marks a grade
  reached without reading every stage. Each is a glyph as well as a colour, so it
  survives a monochrome terminal. The `D` report shows the numbers behind the
  word, including every volume stage that can alter a sample.
- **Gapless queue.** The next track is preloaded and transitions are gapless
  within a sample rate; a rate change reinitialises the output (a short gap) so
  playback stays bit-perfect rather than resampling to match.
- **Mouse-first, with a complete VIM keyboard.** Clickable tabs, transport and a
  scrubbable progress bar; `j`/`k`, `g`/`G`, `/` to filter, `?` for the full
  reference. Parity runs both ways — no action is reachable by only one of them.
- **Themes, light and dark.** Eleven palettes including `nord` (default),
  `gruvbox`, `dracula`, `catppuccin`, `tokyo-night`, `true-black` for OLED, and
  `terminal` to defer to your own sixteen colours. Every colour comes from a
  table of roles, so the fidelity grades keep a contrast floor on any background.
- **MPRIS without a bus library.** The desktop's media keys, panel applets and
  `playerctl` drive it. priel speaks the D-Bus wire protocol directly rather than
  linking one — every bus library brings either an async runtime or `libdbus`.
  With no session bus there is simply no bus, and a line in the log.
- **A dependency list you can audit.** No async runtime anywhere in the tree, no
  OpenSSL (TLS is rustls). Around 100 crates for the whole binary; **libmpv is
  the only non-Rust runtime dependency**. A ~5 MiB binary that redraws only on
  change and holds a bounded ~40 MiB window of each track however long it is.

The design decisions behind these live in [`docs/adr/`](docs/adr/).

## Install

Requires Rust ≥ 1.88, `libmpv` (`mpv-devel` / `libmpv-dev` to build), a working
PipeWire or ALSA setup, and a subscription.

```bash
make check-deps        # verify cargo and libmpv are present
make                   # release build
sudo make install      # binary, man page, completions, licence
make run ARGS="--device pipewire/alsa_output.usb-..."
```

`make help` lists every target. Install paths follow the GNU conventions
(`DESTDIR`, `PREFIX`, `BINDIR`, `MANDIR`). For UI work without mpv headers,
`make build-nolibmpv` compiles the interface with playback stubbed out.

**Signing in.** On first run priel opens your browser; you sign in and land on a
page that looks like an error. Copy its address, paste it back into priel, and
you are in. The session renews automatically; `A` signs in again if it lapses.
priel ships no client credentials — on first run it offers to download one from
the open-source project the other native Linux players use, and runs without it
(minus session renewal) if you decline. Set `PRIEL_NO_BROWSER=1` for a headless
session; the sign-in screen always shows the URL too.

## Configuration

priel follows the XDG layout:

- `~/.config/priel/settings.conf` — theme, device, exclusive, log level
- `~/.local/state/priel/token.json`, `credentials.json` — session and client key
  (runtime state, not settings; no flag moves them)
- `~/.local/state/priel/priel.log` — diagnostics, fresh each run; mpv's own
  messages land here too

`settings.conf` holds only what a flag can also set, spelled the way the flags
spell it. A flag wins for that run; the file wins over the default; a malformed
line is skipped with a warning rather than stopping startup. Choosing a theme
(`t`), device (`d`) or exclusivity (`x`) writes that one line back on exit and
leaves the rest of the file, comments included, untouched.

```ini
theme = gruvbox-dark
device = pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0
exclusive = false
log_level = warn
```

`priel --list-devices` prints every device with the identifier `--device` takes,
including the direct hardware devices (`alsa/hw:CARD=...`) that ALSA advertises
nowhere. `--exclusive` takes a device out of the sound server's graph so nothing
else can play through it; priel never selects that path on its own, and falls
back to the same card shared if the device will not open exclusively. See
[ADR-0001](docs/adr/0001-exclusive-output-is-asked-for-never-assumed.md).

Run `man priel` or `priel --help` for the full flag list.

## Keys & mouse

Every action has a key and something to point at. The bottom row and the `?`
overlay list them all and every key shown there is itself clickable; the table
below is the common subset.

| Action | Keyboard | Mouse |
|---|---|---|
| Full key reference | `?` | click `[?]` |
| Switch view | `Tab`, or `1`/`2`/`3`/`4` | click a tab |
| Move selection | `j`/`k`, `↑`/`↓` | scroll wheel |
| Open / back | `Enter` / `Esc` | double-click |
| Play selected | `Enter` | double-click a row |
| Play / pause | `Space` | click `▷` / `‖` |
| Seek ±5s | `h`/`l`, `←`/`→` | click or drag the bar |
| Previous / next | `H`/`L`, or `p`/`n` | click `\|◁` / `▷\|` |
| Filter the list | `/`, type, `Enter`/`Esc` | click `[/]` |
| Search the catalogue | `3`, type, `Enter` | click the `3` tab |
| Shuffle / repeat | `s` / `e` cycles | click `⇄` / `⟳` |
| Keep playing at queue end | `c` | click `∞` |
| Show / hide queue column | `W` | click `▤` |
| Browse list ↔ queue | `Ctrl-W` | click into either box |
| Favorite selected / playing | `f` / `F` | click `[f]` / the `♥` |
| Volume / unity gain | `+` / `-` / `0` | click `-` / `+` / the % |
| Output report | `D` | click the verdict |
| Choose device / theme | `d` / `t` | click `◎` / `◐` |
| Recent log | `M` | `[?]`, then `M` |
| New / rename / delete playlist | `N` / `R` / `X` | `[?]`, then the key |
| Add track to a playlist | `a`, then a row | `[?]`, then `a` |
| Quit | `q` | click `[q]` |

Typing is the one thing the mouse is not asked to do: the filter, search and
sign-in boxes accept and cancel with their own keys.

The now-playing block is always the bottom of the screen — track, progress bar,
output badge and fidelity verdict — so those four facts never move with the
width. On a terminal 120 columns or wider the play queue takes a column of its
own on the right (fold it with `W`). Playing a listing that is only partly paged
in fills the queue with the rest of it in the background, so a shuffle covers the
whole listing rather than the rows that were on screen. Shuffle deals a play
order the queue panel shows without reordering the queue itself; a repeating
queue suppresses the radio, and the `∞` control dims to say so.

## Status

Working: favorites and playlists (create, rename, delete, edit contents), the
service's mixes and catalogue search (paged and reloadable with `r`), local
filtering, hi-res resolution and playback (24/192 via progressive segment
streaming), a gapless queue with shuffle and repeat, an optional radio
continuation, full transport and volume, MPRIS, and a sectioned `D` output
report that grades each stage on its own evidence and names what would need
changing — the sound server's allowed rates, the volume stage applying a loss,
and what holds the output device.

Album cover art (drawn as terminal half-blocks in the now-playing box) is
implemented but its image URL is not yet verified against a live response; a
cover that will not fetch is simply absent. See [`docs/cover-art.md`](docs/cover-art.md).

**Not planned: a spectrum visualiser.** It can coexist with bit-perfect output,
but the filter graph that splits the stream is rebuilt per track and takes the
audio output with it, costing the gap gapless playback exists to remove. See
[ADR-0002](docs/adr/0002-a-display-of-the-audio-costs-the-gapless-transition.md).

## Development

```
priel-core     lib  — API access + hi-res stream resolution (blocking, UI-agnostic)
priel-player   lib  — embedded libmpv player, thread-owned, stream_cb protocol
priel-tui      bin  — ratatui frontend (builds the `priel` binary)
```

`make check` runs formatting, clippy at pedantic over both feature
configurations, and the test suite (which needs no network, credentials, audio
device or terminal); `make coverage` reports line coverage. The two library
crates hold no UI code, so a GUI frontend can be added later as a second binary.
Style and testing rules are in [`RUST_STYLE.md`](RUST_STYLE.md).

## Packaging

- Single binary `priel`; runtime deps are `libmpv.so.2` and PipeWire/ALSA. No
  OpenSSL, no async runtime.
- `make install` places the binary, `priel.1`, bash/zsh/fish completions, the
  licence and README, honouring `DESTDIR`/`PREFIX`/`BINDIR`/`MANDIR`.
- The man page and completions are generated from the same clap definition the
  binary parses with (`make assets`), so they cannot drift.
- `make dist` produces a source tarball; `make vendor` vendors crates for an
  offline build. Minimum supported Rust is 1.88.
- No trademarked term appears in the package name, binary, crate names or any
  identifier — the service is named only in prose. Do not ship the TIDAL logo or
  wordmark, and keep the disclaimer above in the RPM `%description`.

## License

GPL-3.0-or-later. Copyright (C) 2026 Guy Boldon. This program comes with no
warranty; see [`COPYING`](COPYING) for the full text.
