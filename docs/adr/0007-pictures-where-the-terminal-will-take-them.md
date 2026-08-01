# Pictures where the terminal will take them

Status: accepted

The album cover was drawn as half blocks, everywhere, and `docs/cover-art.md`
recorded why the picture protocols were rejected: they draw outside ratatui's
cell grid and would tear, and they depend on detecting a terminal priel could not
detect reliably. Both objections were correct. Both are now answerable, so all
three protocols are built and half blocks are the fallback rather than the only
path.

## What it cost: one crate

kitty's protocol takes **raw RGB**, which is exactly what `art::Image` already
holds - `zune-jpeg` is asked for `ColorSpace::RGB` and the buffer is
`width * height * 3`, row-major, unpadded. iTerm2's takes a picture **file**, and
the cover arrives from the service as a JPEG that is already fetched and cached.
`base64` was already in the tree via `priel-core`, which decodes stream manifests
with it. So two of the three protocols cost nothing at all.

Sixel needed an encoder, because it is a palette format and a cover has to be
quantised to at most 256 colours. `icy_sixel` is one crate with no dependencies
of its own. It is what buys **foot** - a common Wayland terminal that takes no
other picture protocol - and xterm built for sixel.

For comparison, `ratatui-image` was measured at **13** crates with default
features off and **77** with them on, because it pulls `image` whole: `rav1e`,
`ravif`, `exr`, `rayon`. An AV1 encoder to draw an album cover.

The binary went from 102 crates to 103.

## The tearing objection: nothing is drawn per frame

The cover changes when the track does and hardly ever otherwise, so a picture put
on screen once should stay there for the thousands of frames that follow without
a byte written for it. ratatui repaints only the cells that changed, so a
placement survives every frame that does not touch its rect.

`App::cover_paint` is what decides the frames that do. It compares what is wanted
against what is believed to be on screen and answers `Nothing` for almost all of
them; a run where only the clock ticked writes **zero** bytes, which is asserted
on the real loop. The renderer publishes `cover_rect` and paints nothing into it,
the same way `list_inner` and `queue_inner` are already published; `run` writes
the escape after `terminal.draw` has flushed, because ratatui owns the frame
until then.

A zero rect is the one reading that covers all four ways the art leaves the
screen - folded away with `C`, a terminal below `COVER_MIN_HEIGHT`, an overlay
standing over it, and a track with no art.

kitty's protocol is preferred where a terminal speaks more than one, because it
is the only one that transmits against an id: a resize costs a dozen bytes rather
than a megabyte. The other two write into the cell grid like text, so the
renderer painting over them *is* the erasure, and `clear` deliberately writes
nothing for them.

## The detection objection: the environment cannot answer it

It was right, and worse than it looked. **Environment variables describe the
terminal a session started under, not what sits between priel and the screen.**
Measured: kitty running a multiplexer passes `KITTY_WINDOW_ID` straight into the
pane. Every variable said kitty, the pictures were written, the multiplexer
swallowed them, and the cover was a blank box - strictly worse than the mosaic it
replaced.

So priel asks the terminal instead, at startup: a kitty capability query that
draws nothing, followed by a primary device attributes request. The second is a
fence - every terminal answers it, so there is always exactly one arrival to wait
for rather than a timeout on every start - and it is worth having for itself,
because a `4` among its parameters is the terminal's own statement that it does
sixel, which beats matching names against `TERM`.

**A multiplexer is refused before the answer is even read**, and that ordering is
the point. Measured on herdr: it forwards the query, answers `OK` on the
terminal's behalf, and then drops the picture on the floor. Answering the
question is not a promise to carry the payload. `TMUX`, `STY`, `HERDR_ENV`,
`ZELLIJ` and `ABDUCO_SOCKET` all take half blocks, which work in all of them.

iTerm2's dialect has no query to ask, so it is the one still guessed from the
environment - and only with nothing in the way.

`q=2` on every kitty escape is load-bearing for an unrelated reason: without it
kitty writes its reply on **stdin**, where the event loop is reading keys, and
crossterm delivers it as a burst of junk key presses. The cost is that a
malformed escape fails silently, which is worth knowing when debugging one.

## Two sizes of cover

`COVER_FETCH_PX` is 160, chosen for a mosaic of a few dozen cells. Rendered as
real pixels on a large screen that is visibly soft - it was the first thing said
about the feature. A terminal that draws pictures fetches **640** instead.

The size is part of the cache key. Keyed on the cover alone, whichever size was
fetched first would be served to both paths, so opening priel in kitty after a
plain terminal would quietly hand the photograph path a 160-pixel picture. The
only symptom is "the art is soft", which is why there is a test on it.

640 rather than the 1280 the service will also serve: the picture goes to the
terminal as raw pixels, and 1280 square is five megabytes before base64. Once per
track, but a hitch at every track change is a poor trade for detail past what the
box can show.

## What is deliberately not here

- **Scaling a sixel to the cell box.** Sixel has no scaling parameter, so it
  lands at whatever cells its own pixels come to. `COVER_FETCH_PX` happens to be
  close to the box's pixel size, so it is about right; a terminal with unusual
  cells draws it a little off. Fixing it properly means plumbing
  `crossterm::terminal::window_size` through to the encoder.
- **Unicode placeholder placement** (kitty's `a=U`), which would let the picture
  live in the cell grid and scroll with it. priel's cover sits in a fixed rect,
  so the id-and-place path is far less code for the same result.
- **Passthrough wrapping for multiplexers.** Even with `allow-passthrough on`,
  their own redraws move and corrupt what was placed.

## Consequences

- The cover is a photograph on kitty, Ghostty, `WezTerm`, Konsole, iTerm2,
  mintty, Rio, Warp, foot and xterm-with-sixel, and a mosaic everywhere else.
- `--no-cover-graphics` and `cover_graphics = false` turn it off; neither can
  turn it *on* at a terminal that will not take one, because the terminal's own
  answer decides that.
- Encoding happens on the worker. base64 of a megabyte and a palette
  quantisation are not work for the thread that draws frames.
