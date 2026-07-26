# priel: hi-res terminal client for TIDAL

A mouse-first terminal client for TIDAL with a complete VIM keyboard and
bit-perfect, rate-following playback, straight into PipeWire or an exclusive
ALSA device.

> **Unofficial software.** priel is not affiliated with, endorsed by, or
> sponsored by TIDAL or Aspiro AB. TIDAL is a trademark of its respective owner
> and is named here only to describe what this client connects to. An active
> subscription is required. priel does not circumvent access controls and has no
> download or offline-export feature.

On Linux, audio normally passes through the sound server's shared mixer, which
can resample it to whatever rate the graph is running, so you can be paying for
hi-res and hearing a resampled copy without any indication. priel plays straight
to the hardware and shows you, per track, exactly what the device is doing.

priel is a full TIDAL client: your favorites, playlists (create, edit, delete),
mixes, and catalogue search, with shuffle, repeat, and an optional radio
continuation.

*A **priel** is a tidal channel in the Wadden Sea, the creek that carries the
water in and out of the flats twice a day, on the pull of the moon.*

## What makes this program different?

- **Helps set up your hardware.** When your system is set for lower quality than
  the track can deliver, priel flags it and, with your OK, changes the PipeWire
  or Bluetooth setting that's holding it back, instead of leaving you to hand-edit
  configs.
- **Bit-perfect playback that reads the hardware.** priel grades fidelity against
  the audio device's live parameters, not what the sound server *claims*,
  because a shared server can report one rate while clocking the card at another.
  Hi-res is reachable on both shared PipeWire and exclusive ALSA; priel shows you,
  per track, which you're getting.
- **Three honest grades.** `✓ bit-perfect`, `≈ near bit-perfect` (only the level
  changed), `⚠ resampled` / `⚠ truncated`. `✓?` marks a grade reached without
  reading every stage. The `D` report shows the audio device details, including
  every volume stage that can alter a sample.
- **Gapless queue.** The next track is preloaded and transitions are gapless
  within a sample rate; a rate change reinitialises the output (a short gap) so
  playback stays bit-perfect rather than resampling to match.
- **Mouse-first, with a complete VIM keyboard.** Clickable tabs, transport and a
  scrubbable progress bar; `j`/`k`, `g`/`G`, `/` to filter, `?` for the full
  reference. Parity runs both ways: no action is reachable by only one of them.
- **MPRIS support.** The desktop's media keys, panel applets and `playerctl`
  drive it. priel speaks the D-Bus wire protocol directly.
- **A dependency list you can audit.** Around 100 crates for the whole binary;
  **libmpv is the only non-Rust runtime dependency**. A ~5 MiB binary that
  redraws only on change and holds a bounded ~40 MiB window of each track however
  long it is.

## Features

- **A now-playing queue.** See what's playing and what's coming up, and favorite
  or save any track right from the list.
- **Live quality readings.** See how good the sound is and whether your gear is
  really playing it at full quality, as it plays.
- **Your whole library.** Favorites, playlists, Mixes, and search, all in one
  place.
- **Album art in any terminal.** Cover art at different sizes, even in a plain
  text terminal.

## Install

Linux only. Needs `libmpv` at runtime, a working PipeWire or ALSA setup, and a
TIDAL subscription.

### Quick install

```bash
curl -fsSL https://raw.githubusercontent.com/codifryed/priel/main/install.sh | sh
```

**Signing in.** On first run priel asks to open your browser; you sign in and
land on a page that looks like an error. Copy its address, paste it back into
priel, and you are in. The session renews automatically.

### From a clone

```bash
make check-deps        # verify cargo and libmpv are present
make                   # release build
sudo make install      # system-wide: binary, man page, completions, licence
make run ARGS="--device pipewire/alsa_output.usb-..."
```

`make help` lists every target. Install paths follow the GNU conventions
(`DESTDIR`, `PREFIX`, `BINDIR`, `MANDIR`), so `sudo make install PREFIX=/usr/local`
places it system-wide and `make install PREFIX=~/.local` matches the quick
installer. For UI work without mpv headers, `make build-nolibmpv` compiles the
interface with playback stubbed out.

## Configuration

priel follows the XDG layout:

- `~/.config/priel/settings.conf`: theme, device, exclusive, log level
- `~/.local/state/priel/token.json`, `credentials.json`: session and client key
  (runtime state, not settings; no flag moves them)
- `~/.local/state/priel/priel.log`: diagnostics, fresh each run; mpv's own
  messages land here too

```ini
theme = gruvbox-dark
device = pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0
exclusive = false
log_level = warn
update_check = true
```

`priel --list-devices` prints every device with the identifier `--device` takes,
including the direct hardware devices (`alsa/hw:CARD=...`) that ALSA advertises
nowhere. `--exclusive` takes a device out of the sound server's graph so nothing
else can play through it; priel never selects that path on its own, and falls
back to the same card shared if the device will not open exclusively.

Run `priel --help` for the full flag list.

## Updates

At startup priel checks for the latest release version, and if a newer one exists
it says so on the notice line. `priel --update` then runs the installer to fetch
and replace the binary. Turn the check off for a run with `--no-update-check`,
from a launcher with `$PRIEL_NO_UPDATE_CHECK`, or for good with
`update_check = false` in the settings file.

## Keys & mouse

Every action has a key and something to click, and it's all discoverable in-app:
the bottom row and the `?` overlay list every binding, and each key shown is
itself clickable. The essentials:

| Action | Key | Mouse |
|---|---|---|
| Play / pause | `Space` | click `▷` / `‖` |
| Previous / next | `H` / `L` | click `\|◁` / `▷\|` |
| Seek | `h` / `l` | click or drag the bar |
| Filter the list | `/` | click `[/]` |
| Full key reference | `?` | click `[?]` |
| Quit | `q` | click `[q]` |

## Development

```
priel-core     lib  - API access + hi-res stream resolution (blocking, UI-agnostic)
priel-player   lib  - embedded libmpv player, thread-owned, stream_cb protocol
priel-tui      bin  - ratatui frontend (builds the `priel` binary)
```

`make check` runs formatting, clippy at pedantic over both feature
configurations, and the test suite (which needs no network, credentials, audio
device or terminal); `make coverage` reports line coverage. The two library
crates hold no UI code, so a GUI frontend can be added later as a second binary.
Style and testing rules are in [`RUST_STYLE.md`](RUST_STYLE.md).

## License

GPL-3.0-or-later. Copyright (C) 2026 Guy Boldon. This program comes with no
warranty; see [`COPYING`](COPYING) for the full text.
