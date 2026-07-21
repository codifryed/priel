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
  `⚠ resampled` or `⚠ truncated` when the stream is being rebuilt. A wider
  container is not a fault: 24-bit content in an `S32` frame is exactly how a USB
  DAC expects it, so that reads green. The row carries the word and nothing else;
  the numbers behind it are in the `D` report.
- **The grade says what it rests on.** `✓?` rather than `✓` when a stage exists
  and could not be read. A tick that quietly means "as far as I looked" is the
  one failure this indicator exists to avoid, and a stage that *cannot* exist —
  there is no sound server between priel and a directly held card — counts as
  fully evidenced rather than unread, so the cleanest chain on the machine gets a
  clean tick. It is a glyph rather than a colour, so it survives a light theme, a
  dark one, a monochrome terminal, and the red/green deficiency the grades
  already lean on. The theme picker previews all three grades in the palette each
  row offers, so the choice is made on the thing that matters.
- **Unity gain is a first-class state.** Any software volume below 100%
  multiplies every sample. The header shows `100%` in green at unity and yellow
  otherwise, `0` restores it, and **all three stages are watched** — priel's own
  volume, the audio server's volume for our stream, and the volume on the sink
  everything on the machine is mixed into, which nothing else can see. That third
  one is read from the graph dump, and from the field that says whether the
  server actually multiplied a sample rather than the one that says what the
  control was set to: those two disagree, and a level applied by a hardware mixer
  costs no bits where the same figure applied in software costs about one per
  6 dB. Where a level is applied in software the `D` report gives the percentage,
  the decibels, and the bits. Enthusiasts: leave all three at unity and set level
  on the DAC.
- **Mouse-first, and it shows.** Clickable view tabs and transport controls, a
  scrubbable progress bar, wheel scrolling, double-click to play — and every key
  shown in the bottom hint row is itself a button. If an action cannot be done by
  pointing at it, that is a bug rather than a design choice.
- **A complete VIM keyboard, not the leftovers.** `j`/`k` and the arrow keys,
  `g`/`G`, `J`/`K` and `Ctrl-D`/`Ctrl-U`, `/` to filter. `?` opens the full
  reference rather than making you read this file. Parity runs both ways: there
  is no control that only the mouse can reach, and none that only the keyboard
  can.
- **Built-in themes, light and dark, that keep the indicator honest.** Ten
  published palettes — `nord` (the default), `gruvbox-dark`, `gruvbox-light`,
  `one-light`, `dracula`, `one-dark`, `catppuccin` (Mocha), `tokyo-night` and
  `tokyo-night-day`, plus `true-black` for an OLED panel, where the background is
  `#000000` and costs no light — and `terminal`, which paints with your
  terminal's own sixteen colours and follows a palette you have already chosen
  rather than fighting it.
  Every colour priel draws comes from one table of *roles*, so a theme is
  complete by construction and a test refuses any bare colour written into the
  renderer. The fidelity grades are held to a contrast floor on their own
  background, which is what makes `≈ near` and `⚠ resampled` as distinguishable
  on cream as they are on charcoal — and they carry a glyph each, so they still
  read with no colour at all.
  Every other row of a list is backed by a **stripe** a whisper away from the
  surface, so an eye that starts at a title and ends at a duration two hundred
  cells later stays on the same track. Each palette picks its own, some a step
  up and some a step down, and everything painted on it clears the same
  contrast floor it clears on the surface. The tabs you are *not* on sit on
  that same stripe and the one you are on is lifted off it, so the strip itself
  says where you are rather than leaving it to the colour of four words.
  `terminal` is the exception and says so in the picker: it cannot see the
  background it is painting on, so it draws no stripe rather than guessing at
  one.
- **On your media keys and your lock screen, with no bus library.** priel
  publishes MPRIS, so the desktop's own media controls, a panel applet and
  `playerctl` all drive it, and the track shows up where the desktop shows
  tracks. It speaks the D-Bus wire protocol itself rather than linking one:
  every bus library either brings an async runtime or brings `libdbus`, and one
  binary has to run on a media-server box with no desktop libraries on it. With
  no session bus there is simply no bus — no fifth thread, no failure, and the
  reason in the log. `TrackList` and `Playlists` are deliberately out of scope,
  and there is no action on the bus that the keyboard and the mouse do not
  already have. See
  [`docs/adr/0003`](docs/adr/0003-the-session-bus-is-spoken-directly-or-not-at-all.md).
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

The **PipeWire setup assistance** this list used to have a gap for is now
built: the `allowed-rates` half, and the half that names what has the output
device open and what it would take to reserve it. See the roadmap below for what
is next.

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

**Settings live in `~/.config/priel/settings.conf`** (or `$XDG_CONFIG_HOME`),
and hold the four things a flag can also set: the palette, the output device,
whether that device is taken exclusively, and how much the log records. A flag
wins over the file for that run, the file wins over the default, and the file is
plain `key = value` with `#` comments:

```ini
theme = gruvbox-dark
device = pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0
exclusive = false
log_level = warn
```

The values are spelled exactly as the flags spell them. Choosing a palette with
`t`, a device with `d` or exclusivity with `x` writes that one line back when
priel exits - the rest of the file, comments included, comes back untouched, and
a run in which no picker was used does not write at all. A missing, unreadable
or half-written file never stops priel starting: the bad lines are skipped, the
reason goes in the diagnostic log, and everything else still applies. Nothing
else is kept there - the session and the client key are state, not settings, and
stay under `~/.local/state/priel/`.

`~/.local/state/priel/priel.log` is the diagnostic log, started fresh each run
and holding warnings and errors by default. `--log-level debug` (or
`PRIEL_LOG=debug`) is what to attach to a bug report; `--log-level off` keeps no
file at all. mpv's own messages are recorded in the same file, in order, so a
failed track shows both halves together — `[file] Cannot open file ...` next to
priel's own account of what it was trying to play.

`--theme` picks the palette: `nord` (the default), `gruvbox-dark`,
`gruvbox-light`, `one-light`, `dracula`, `one-dark`, `true-black`, `catppuccin`,
`tokyo-night`, `tokyo-night-day`, or `terminal` to defer to your terminal's own
colours — which is also the one palette that draws no row stripe, since it
cannot see the background it would be striping against. `t` opens the same list
while priel is running, and what is chosen there is remembered in
`settings.conf`; the flag overrides it for one run.

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
ones that should have needed looking up; `--shared` is the way back to a shared
device when the file remembers an exclusive one. `--log-level` and `--log-file`
control the diagnostic log. See `man priel` or `priel --help`.

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
beginning, and the `D` report reads `shared - exclusive was refused` rather than
claiming a connection it does not have. See
[ADR-0001](docs/adr/0001-exclusive-output-is-asked-for-never-assumed.md).

For UI work or a machine without mpv headers, `make build-nolibmpv` compiles the
interface with playback stubbed out.

## Keys & mouse

Every action has a key binding and something to point at; parity runs both ways.
Every key listed in the bottom row of the interface is clickable, and so is
every key listed in the `?` reference — that overlay is priel's menu, which is
how the rarely-used actions stay off a bottom row narrow terminals would clip.

| Action | Keyboard | Mouse |
|---|---|---|
| Full key reference | `?` | click `[?]` |
| Recent log messages | `M` | `[?]`, then `M` |
| Output report | `D` | click the verdict, click `[D]`, or `[?]` then `D` |
| Choose the output device | `d` | click `◎`, then a row in the picker |
| Exclusive output on/off | `x` in the picker | click the toggle |
| Choose a colour theme | `t` | click `◐`, then a row in the picker |
| Sign in again | `A` | `[?]`, then `A` |
| Switch view | `Tab` cycles, `1`/`2`/`3`/`4` | click a tab |
| Move selection | `j`/`k`, `↑`/`↓` | scroll wheel |
| Browse list / queue | `Ctrl-W` | click into either box |
| First / last | `g` / `G` | click `[g/G]` |
| Page up/down | `J`/`K` full, `Ctrl-U`/`Ctrl-D` half | `[?]`, then the same keys |
| Open playlist or mix / back | `Enter` / `Esc` | double-click |
| Play selected | `Enter` | double-click a row |
| Play a queue entry | `Ctrl-W`, then `Enter` on it | double-click it in the panel |
| Play / pause | `Space` | click `▷` / `‖` |
| Seek ±5s | `h`/`l`, `←`/`→` | click or drag the progress bar |
| Previous / next track | `H`/`L`, or `p`/`n` | click `|◁` / `▷|` |
| Filter the current list | `/`, type, `Enter`/`Esc` | click `[/]` |
| Reload the current list | `r` | click `↻` |
| New playlist | `N`, type, `Enter` | `[?]`, then `N` |
| Rename this playlist | `R`, edit, `Enter` | `[?]`, then `R` |
| Add track to a playlist | `a`, then a row | `[?]` then `a`; then click a row |
| Delete playlist / remove track | `X`, then `y` | `[?]` then `X`; then click `[y]` |
| Answer a confirmation | `y` do it, `n` or `Esc` don't | click `[y]` / `[n]` |
| Scroll the `?` reference | `j`/`k`, `g`/`G` | `[?]`, then the same keys |
| Search the catalogue | `3`, type, `Enter`; `i` to re-edit | click the `3` tab; `[?]` then `i` |
| Shuffle the current view | `s` | click `⇄` |
| Keep playing when the queue ends | `c` | click `∞` |
| Favorite the selected track | `f` | click `[f]` |
| Favorite the playing track | `F` | click the `♥` beside the title |
| Volume | `+` / `-` | click `-` / `+` |
| Restore unity gain | `0` | click the percentage |
| Quit | `q` | click `[q]` |

Typing is the one thing the mouse is not asked to do: the filter box, the search
query and the pasted sign-in address are text, so the keys that accept or cancel
them belong to the box being typed in and have no control of their own.

The line along the top of the list says where you are and how much of the list
is here — `Favorites — 42 of 417 tracks`, and `Playlists › Deep Cuts — 18 tracks`
when you have opened one, so the way back out is named rather than remembered.
The second figure appears only while there is more still to page in. It names no
keys: the bottom row and the `?` reference are where those are, and the hints
that used to sit here were clipped mid-word on an eighty-column terminal.

A track row spends the width the terminal actually has. The title and the
duration are on every row, the duration against the right-hand edge so the times
read as a column; the artist, the album and the quality tier appear as the width
allows and are given up in that order — the album first, then the artist, then
the tier — as it shrinks. A column is either wide enough to read or it is not
drawn, so nothing is clipped away without the row changing shape to say so.

On a terminal 120 columns or wider, what is playing moves out of the three rows
along the bottom and into a panel down the right-hand side: the track and the
artist, the progress bar and the two times, what it is being played into, and
the verdict on what arrives there. The bottom row is then the keyboard reference
and nothing else, which gives the list two more rows to use — and the bar is
still click-and-drag to seek, in its new place. Below 120 columns the three rows
along the bottom are exactly as they were. That one width is the only
breakpoint; the list simply has less room once the panel is there, and the row
gives its columns up in the order above.

Under those readouts the panel carries **the play queue**, with the tracks
already played above the current one and dimmed, and what is still to come below
it. It is a second focusable region: `Ctrl-W` moves the keyboard between the
browse list and the queue — vim's own window key — and clicking into either box
does the same thing with the pointer. Whichever box holds the keyboard is drawn
with a heavier border, so which one `j`/`k`, `g`/`G` and `Enter` will act on is
readable with no colour at all; the box that does not hold it still shows where
its cursor is, in a quieter backing of its own. `Enter` on a queue entry plays
it, forwards or back, which is what makes the history above the current track
navigation rather than a picture of it.

`F` still favorites the playing track and the `♥` beside the title is still
clickable, whichever box has the keyboard.

Entries the radio added when the queue ran out are marked `~`, and the heading
above the queue says so — `Queue 4/9  ~ radio` — for as long as there is a mark
to explain. Music you chose and music the service suggested are never blurred
together: the mark is a column of its own, so an entry can be a suggestion and
be in the history at the same time and say both.

The queue is the snapshot taken when you pressed `Enter`, and showing it makes
that visible: a page of the listing that lands later does not join it, and
pressing `Enter` again in the list is how you take the larger set. The panel
draws straight from that one queue every frame, so what is on screen and what
plays next cannot disagree.

**Below 120 columns there is no panel and so no queue view.** That is the
decision rather than an omission: it is one breakpoint for the whole interface,
and a queue squeezed into a narrow terminal would be taking rows from the list
that is the reason for the narrow terminal. `Ctrl-W` there says which width
brings the queue back, and the queue itself is still driven by `H`/`L`, `s` and
`c` exactly as before.

A `♥` on a row means the track is in your favorites and a `♡` means it is not,
as far as priel has been told. The service reports no favorite flag on a track,
so the favorites listing itself is the only thing that ever says so, and priel
knows what it has loaded: a favorited track met in the search results, whose own
page of the listing has not been reached, wears a hollow heart until you press
`f` on it or reload the favorites.

The heart changes the moment you press the key, before the service has answered,
and changes back with a message on the notice line if the change is refused. A
track you take off the favorites keeps its row in the list until the list is
reloaded with `r`: removing the row would move every row below it out from under
the cursor, including the one you just acted on.

`Esc` cancels: it leaves a filter or search box, backs out of a picker or a
confirmation, and steps back out of an opened playlist or mix — to the list it
was opened from, whichever that was. It never quits.

`N` makes a playlist, `R` renames the highlighted one and `a` puts the
highlighted track into one you pick from a list. `X` takes away whatever is
highlighted: the playlist itself in the `2` tab, the track in an opened
playlist.

**The two that take something away ask first, and `Enter` is not the answer.**
The question names what it is about to remove and, for a playlist, says that it
goes from the account rather than only from priel and cannot be brought back.
Only `y` — or a click on the `[y]` control itself — goes through with it; `n`,
`Esc` and every other key leave things alone, and a click anywhere but on those
two controls does nothing at all. `Enter` is deliberately not a yes: it is what
opens a playlist, and pressing it twice out of rhythm should not be the
difference between reading a question and answering it.

A rename shows up straight away and is put back with a message if the service
refuses it, the way a heart is. The two removals do not: the row stays where it
is until the service confirms it is gone, because a row that vanished on hope
and quietly reappeared would read as a glitch rather than as the refusal it was.
Creating a playlist waits too — the service chooses its identifier, so there is
nothing to show until it answers — and the new playlist then appears at the top
of the list without the list being reloaded. Adding a track to a playlist is the
one change that reports its own success, since the playlist you added to is
usually not the one you are looking at.

The `4` tab holds the mixes the service builds for you, kept apart from the
playlists you wrote rather than mingled with them: nobody can edit a mix, and it
is rebuilt under you. That last part is why this is the one list priel fetches
again every time you open the tab, where the playlists are fetched once — a copy
of a mix held from your last visit is stale by construction, not by bad luck.
`r` refreshes it again without leaving. A mix row shows what the mix was built
from instead of a track count and a running time, because the service sends
neither for a mix; its length is only knowable once you open it.

## Status

Working: favorites, playlists — including making, renaming and deleting them and
changing what is in them — the service's own mixes and catalogue search,
each paged in as the selection nears the end of the loaded rows and reloadable
with `r`; local
filtering; hi-res resolution and playback (24/192 via progressive segment
streaming); a gapless
play queue with a preloaded next track; shuffle with auto-advance; an optional
`c` toggle that carries the queue on with the service's radio for the track that
ended it, off unless it is asked for and saying `radio` in place of `queue` for
as long as what is playing is a suggestion rather than a choice; play, pause,
seek, skip and volume; a now-playing bar with a scrubbable progress bar, a live
DAC badge and a bit-perfect verdict; the `?` reference overlay; a diagnostic
log with an `M` overlay for reading it without leaving the player; a `D` output
report, in sections that each answer for themselves — the verdict, the device
and how it is held, every volume stage that can alter the samples, and the chain
of PipeWire nodes between priel and the device with the rate and format each one
negotiated, marking the node where the track's rate or width is first lost with a
`⚠`, reporting the rates the sound server is permitted to clock at with the
change to make when the track's rate is not one of them, and naming what has the
output device open with what it would take to reserve it; a `d` picker for moving
the output between devices, with an `x` toggle for taking a device exclusively; favoriting and unfavoriting the selected or playing track; MPRIS, so the
media keys, the desktop's own controls and `playerctl` drive the same actions
the keyboard and the mouse do.

Each section renders on its own evidence, so a directly held card — which has no
graph by design — still gets its verdict, its device readout and its volume
stages rather than one sentence saying there is nothing to show.

The volume section lists every stage, including the ones that are absent and the
ones that could not be read, because a stage missing from a list reads as a stage
at unity. The sink's level is quoted as the control shows it, and a loss is only
ever claimed where the server was found to be applying it: measured on a real
machine, a USB DAC sink sat at 2.7% with the server multiplying nothing at all,
on a card exposing no ALSA volume control. Quoting 31 dB of loss there would have
invented a fault; saying nothing would have hidden a control that was plainly
set. So it shows the control, says the server is not applying it, and marks the
tick — because where it *is* applied is not in the graph.

The `D` report names a node only when the chain accounts for what was measured.
Where the device is clocked elsewhere and every node on the path still reports
the track's own rate — a resample the sound server did inside a node rather than
between two of them — it says the change is unaccounted for rather than blaming
the nearest candidate. A wrong name would send you to reconfigure something that
was working.

The same overlay then answers *why*. It reads the rates the sound server is
permitted to clock at out of the same dump, next to the rate the playing track
needs, and when the two do not meet it quotes the exact setting that would add
the rate, the file it goes in, and the fact that the server has to be restarted
for it to apply. That is the usual cause of a resample no node accounts for: the
server was never allowed to run at that rate, so it resampled before any node on
the path saw a sample. Where the setting cannot be read the overlay says
`unknown` rather than guessing at one, and a device held directly has no server
between priel and the DAC to have a setting at all.

The last section of that overlay says who has the output device. A chain that
alters nothing is still a chain the sound server owns, and it can be reshaped by
the next application that starts, so the holder is named whether or not anything
is wrong — read from the same dump: the sink, the process that opened it, the
PCM behind it, and the card. Where the server has it, the overlay gives the
WirePlumber rule that stops it claiming that card, the file the rule goes in, and
the thing that is given up by writing it: nothing else on the machine will be
able to play through that device. Where the card cannot be named from the dump
there is no rule to copy, because a rule matching a name priel guessed at would
disable something that was working. Where priel already holds the device itself
there is nothing to hand over, and the overlay offers no advice at all.

Roadmap, roughly in order:

- **A cleaner sign-in.** The redirect lands on the vendor's own page, which priel
  cannot listen on, so the flow ends with a paste. A client registered with a
  loopback redirect would remove that step; the developer terms do not currently
  permit a native player, so it stands.
- Cover art (kitty/sixel). The listing carries none today, which is also why
  MPRIS publishes no `mpris:artUrl`: an invented one is a broken image where an
  absent one is a placeholder.

**Not planned: a spectrum visualiser.** It was on this list for as long as the
open question was whether it could coexist with bit-perfect output. It can:
splitting the decoded stream and analysing one copy leaves the other byte for
byte what the device would have received, and the fidelity badge reads exactly
the same with it running. What stops it is elsewhere. The filter graph that
performs the split is rebuilt for every track and takes the audio output with
it, so every transition costs the gap that gapless playback exists to remove -
whatever `gapless-audio` is set to. The measurements and the alternatives are in
[`docs/adr/0002`](docs/adr/0002-a-display-of-the-audio-costs-the-gapless-transition.md).

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
