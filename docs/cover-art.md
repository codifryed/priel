# Album cover art in a terminal

The design for drawing the album cover in the now-playing box, and now the
record of what was built. Recorded here rather than in the issue thread because
`tea` truncates every comment body at about 80 characters, so a design that
lives only in a comment cannot be read by anyone working from the tracker.

Every heading below was settled with the maintainer question by question. Where
the answer went against a recommendation, both the answer and the recommendation
are recorded, so the choice reads as made rather than as overlooked.

**Status: built.** The renderer, the layout and the fold are in `art.rs` and
`ui.rs`; the cover id and byte fetch are in `priel-core`; the fetch, decode and
wiring are in `worker.rs` and `app.rs`. The one thing that could not be verified
here - the cover URL pattern - is called out again under *Open* at the foot, and
is isolated in `priel_core::cover_url` so that a correction touches one function
and nothing downstream.

## The technique: half blocks, not a terminal protocol

`▀` (U+2580, upper half block) painted with a foreground colour and a background
colour is **two pixels in one cell**: the foreground fills the top half, the
background the bottom. Nothing but coloured text is involved, so it survives
`ssh`, `tmux`, `script`, and a plain text dump of a frame.

Kitty's graphics protocol and sixel both look better and were both rejected:
they draw outside ratatui's cell grid, so a redraw of the cells underneath does
not repaint them and the image tears. They would also make the feature
conditional on a terminal priel cannot detect reliably.

`▀` is single width in every font that has it, which matters here: this
repository already has a rule against glyphs with emoji presentation, because a
terminal that paints one two cells wide while `unicode-width` calls it one puts
every hit box after it a cell out.

### What that costs in resolution

A cell is **one pixel wide and two tall**, and terminal cells are about twice as
tall as they are wide, so those pixels come out roughly square. An area N rows
tall and M columns wide is therefore `M × 2N` pixels, and a square cover wants
`M = 2N`.

| rows | columns | pixels | reads as |
|---|---|---|---|
| 3 | 6 | 6 × 6 | a colour smear |
| 5 | 10 | 10 × 10 | a shape, maybe |
| 8 | 16 | 16 × 16 | recognisably the cover |
| 12 | 24 | 24 × 24 | comfortable |

Three rows is what the now-playing box already had, and it is not enough. That
one fact drove every layout decision below.

## Where it goes: left of the text, and the text stays put

The art occupies a column on the **left** of the now-playing box; the three
existing rows sit to its right, **bottom-aligned**.

```
┌ Now playing ────────────────────────────────────┐
│ ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄  In Rainbows                   │
│ ████████████████  2007 · FLAC 24-bit · 192 kHz  │
│ ████████████████                                │
│ ████████████████                                │
│ ████████████████  ▶ ♡ Nude — Radiohead          │  h-5
│ ████████████████  1:01 ███████░░░░░░░░░░  4:05  │  h-4
│ ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀  OUT — ✓ bit-perfect           │  h-3
└─────────────────────────────────────────────────┘
 [space] play  …  [q] quit                           h-1
```

**Bottom-aligned is the load-bearing word.** Settled decision 10 of
`interface-readability-and-flow.md` says the four facts a listener glances at
without looking for them must not move when the terminal changes, and that
decision was reached by *reversing* a layout that had moved them. The box is
anchored to the bottom of the screen and grows upward, so with the text pinned
to the bottom of the box those three rows stay at `h-3`, `h-4` and `h-5`
whether the art is on, off, or a different size. Top-aligning the text would
undo decision 10 by a side door.

The rows beside the art's upper half are free, and are where the "more track
detail" request belongs: album, year, format, anything that does not fit on the
one title row.

## When it is shown: a height breakpoint, and a fold key

The same shape the queue column already uses, one axis over:

| queue column | cover art |
|---|---|
| shown at `WIDE_COLS` (120) columns and up | shown at a row breakpoint and up |
| folded with `W`, and a `▤` control | folded with a key, and a control |
| a folded column publishes no rect | folded art publishes no rect |

A listener on a 24-row terminal never pays for it; one on a 50-row terminal gets
it without asking; anyone can fold it away at any size. The breakpoint is one
number in one place, and gets a test either side of it.

Rejected: scaling the art to whatever rows are spare. It gives a tall terminal a
bigger cover, but the art then changes size on every resize and the list gains
and loses rows continuously.

## Colour depth: always truecolor, no detection

**Settled against the recommendation, deliberately.** The art is always drawn
with `Color::Rgb`. `COLORTERM` is not consulted.

The recommendation was to detect and skip, following the precedent of the
`terminal` theme, which declines to draw a zebra stripe because it cannot know
the surface it is painting on. The maintainer's call is that modern terminals
are overwhelmingly 24-bit and that detection's false negatives - `COLORTERM` is
unset in plenty of terminals that do support truecolor, and multiplexer configs
drop it - would deny art to terminals that would render it perfectly.

The consequence, stated plainly: **an 8- or 256-colour terminal shows a block of
mud where a cover should be.** The remedy is the fold key rather than automatic
detection. If that turns out to bite in practice, detection can be added behind
the same key.

Note that the `terminal` theme's own objection does not apply here. It cannot
draw a stripe because a stripe is a *tint* of a background it cannot read; the
art sets both foreground and background of every cell it touches, so it needs to
know nothing about what it is painting over.

## The decoder: `zune-jpeg`, and only in `priel-tui`

Roughly two new crates (`zune-jpeg`, `zune-core`), pure Rust, no C, no
build-time tooling. This is the workspace's **first genuinely new dependency** -
every other entry in `Cargo.toml` is either load-bearing or was chosen precisely
because it was already in the tree, and each carries a comment saying so. This
one needs the same comment and the same justification.

`image` with `default-features = false, features = ["jpeg"]` was the alternative
and would bring proper resampling filters and other formats, at eight to ten
more crates. Not worth it while the service will serve the cover at a size we
choose: fetching close to the size we need makes the downscale a box average of
a handful of pixels, which is about twenty lines and is testable with no
network.

Hand-rolling a baseline JPEG decoder was considered and rejected: a few hundred
lines of Huffman and IDCT to own and test, no progressive support, to save two
crates.

### Where each piece lives

```
priel-core   the cover URL, and fetching the bytes      no new dependency
priel-tui    zune-jpeg → RGB, downscale, half blocks    +2 crates
```

`priel-core` stays UI-agnostic and gains no decoder, because a future
iced/libcosmic frontend wants the JPEG bytes and will decode them itself. The
`--no-default-features` UI-only build is unaffected either way.

`docs/track-fields.md` records `album.cover` as **deliberately discarded**, with
the reason "art and a theme colour ... a terminal cannot use them". That reason
is what this feature overturns, and the field is one line away, exactly as that
document predicted. The inventory needs updating when it lands.

## Threading, and the rules that do not bend

- **The fetch goes to the worker**, like every other blocking call. There is no
  async runtime in this tree and this does not introduce one.
- **The decode goes to the worker too.** Decoding a JPEG on the UI thread would
  break the render loop's no-blocking rule for a picture.
- **The UI thread receives cells, not pixels.** What crosses the channel is
  either the decoded RGB or the finished span grid; either way the render
  thread does no decoding and no allocation per frame beyond what it already
  does.
- **Nothing is fetched for a track that is not playing**, and the art is keyed
  by track id like every other reply the worker sends, because the app
  correlates replies by id and never by request order.
- **A cover that fails to fetch or decode is absent, not an error banner.** The
  box renders exactly as it does today with the art folded away.

## Open, still

- **The cover URL pattern is unverified.** `priel_core::cover_url` builds the
  documented public form - the cover id with its dashes turned into path
  separators, under `resources.tidal.com/images`, at a square size - but there
  are no API captures in this repository to check it against a live response.
  It **must be confirmed on real hardware**. Its shape is pinned by a test, so a
  correction keeps everything that test asserts; a wrong host or path template
  is a one-line change in that one function, and every stage downstream of it -
  fetch, decode, draw - is proven independently. A cover that will not fetch or
  decode is drawn as absent, so getting this wrong degrades to today's
  behaviour rather than to an error.
- **The row breakpoint** is `COVER_MIN_HEIGHT = 30` and the block size is
  `COVER_ROWS = 8`, both in `ui.rs`. Chosen rather than measured; they want a
  look on a real terminal, and each is one constant in one place.
- **What goes in the free rows** beside the art's upper half is still open. The
  art takes a column on the left and the three facts sit bottom-aligned beside
  it, so the rows above them are empty for now. That space is the separate
  "more track detail" request and should be settled on its own terms.

## What was built, in one place

- `art.rs` - `Image`, `decode_jpeg` (zune-jpeg, RGB), `draw` (half blocks, box
  filter). Pure and fully unit-tested, including a real embedded JPEG.
- `priel-core` - `Track::cover` (the id, off the wire at last), `cover_url` (the
  unverified pattern, isolated), `fetch_bytes` (an unauthenticated capped GET of
  an absolute URL).
- `worker.rs` - `ToWorker::FetchCover` / `FromWorker::Cover`: the fetch and the
  decode both happen here, off the render thread, and only a success is sent.
- `app.rs` - `poll_cover` asks once per track however the track started;
  `on_cover` keeps a reply only while its track still plays; `cover_for_now_playing`
  answers the renderer.
- `ui.rs` - `now_playing_rows` / `cover_wanted` size the box; `cover_column`
  draws the art and bottom-aligns the text; a header control (`▣`) and the `C`
  key fold it, gated to a tall enough terminal.

zune-jpeg is the workspace's first genuinely new dependency; its justification
lives in the workspace `Cargo.toml` beside the entry.
