# Interface readability and flow: an audit and a proposal

Status: proposed. Nothing here is settled; the ranked list below is for the
maintainer to choose from. A small subset marked **done** was implemented
alongside this document because it needed no decision.

Raised by issue #15, which asks for a pass on layout, readability and
information architecture rather than a feature, and which says to propose before
building because this is the easiest place to make the interface worse while
making each part better.

## What I consulted

- **The `ui-ux-pro-max` skill's UX guideline database** (`ux-guidelines.csv`,
  99 guidelines), queried for truncation, hierarchy, scanning, consistency and
  modal escape. The rules it returned that actually bear on a terminal grid are
  named inline below: `truncation-strategy`, `visual-hierarchy`,
  `number-tabular`, `whitespace-balance`, `color-not-only`, `escape-routes`,
  `nav-state-active`, `breadcrumb`, `navigation-consistency`,
  `content-priority`, `empty-states`, `line-length-control`.
- **The repository's own written rules**, which are stronger and more specific
  than anything a general database can say: `.claude/CLAUDE.md` (mouse-first
  with a complete VIM keyboard, parity both ways; the hint row's width
  reservation; hit boxes registered in the walk that paints them; no glyphs with
  emoji presentation), `RUST_STYLE.md`, and `docs/adr/0001` and `0002`.
- **The rendered interface itself.** `TestBackend` frames at 60x20, 80x24,
  120x30 and 200x40, plus every one of the six overlays at 100x30, dumped from a
  throwaway `#[ignore]` test and read as text. Several findings below are only
  visible that way and are invisible in the source.

Everything marked **done** below arrived test-first: a test that failed against
the unfixed code for the stated reason, then the change. Each was then read back
out of a rendered frame, because a passing assertion about a substring is not the
same thing as having looked at the row.

Where the skill's database and the repository's rules disagree about a terminal,
the repository wins. Much of that database is about touch targets, safe areas
and web typography and does not transfer; I have not cited it where it does not.

## Two ADRs constrain what the interface must keep saying

- **ADR-0001**: the output badge never claims exclusivity it did not get, and
  every access state is named including the ordinary shared one. Nothing below
  removes or softens a word about access.
- **ADR-0002**: the fidelity grading is the feature. The verdict badge's four
  words, and the fact that `✓ ≈ ⚠ ✓?` carry their meaning with no colour at all,
  are load-bearing. Nothing below touches the badge's vocabulary or its glyphs.

## The findings, ranked by value against disruption

| # | Finding | Value | Disruption | Verdict | Status |
|---|---------|-------|------------|---------|--------|
| 1 | Track rows are a fixed 72-cell block | high | medium | guidance | **done** |
| 2 | The list title carries hints and is clipped | high | medium | guidance | **done** |
| 3 | `HI_RES_LOSSLESS` shown raw in the list | medium | none | guidance | **done** |
| 4 | The bottom row shifts one cell when output opens | medium | none | guidance | **done** |
| 5 | Truncation counts characters, not cells | medium | low | guidance | **done** |
| 6 | No breadcrumb when a playlist or mix is open | medium | low | guidance | **done** |
| 7 | Now-playing says "Artist — Title"; rows say the reverse | medium | low | taste | propose |
| 8 | The elapsed time is centred and drifts | medium | medium | guidance | **done** |
| 9 | Overlay body text hugs the border, footers do not | low | none | guidance | **done** |
| 10 | Overlay widths are an arbitrary ladder | low | low | taste | propose |
| 11 | The report and the device picker are both "Output" | low | low | taste | propose |
| 12 | Overlay footers are dead text | low | medium | taste | propose |
| 13 | Small wording and spacing inconsistencies | low | none | mixed | **done** |

"guidance" means I can point at a rule I consulted or at a rule this repository
already wrote down. "taste" means I cannot, and the maintainer should treat it
as one person's opinion.

---

## 1. Track rows are a fixed 72-cell block in a box of any width — **done**

**Guidance**, not taste: `content-priority` (show the important thing first, and
do not let the layout decide silently) and `truncation-strategy` (truncate
deliberately, do not overflow). It is also the exact failure mode this
repository already fixed once on the bottom row, where `[q]` vanished on a
narrow terminal and `push_hints` gained its width reservation.

`row_text` formats `"{mark}{kept} {:<32} {:<20} {:<8}{:>6}"`. That is 72 cells,
always, whatever the terminal is. Two consequences, both real:

At 60 columns the quality and the duration are simply gone. Nothing warns; the
paragraph clips and the two right-hand columns cease to exist:

```
before, 60 columns
┌Favorites — 12 tracks   (Tab views · j/k move · Enter play┐
│  ♥ Everything In Its Right Place    Radiohead            │
│  ♥ A Very Long Track Title That Wi… Some Extremely Long… │
│  ♥ Nude                             Bonobo               │
```

At 200 columns, 116 cells of every row are blank:

```
before, 200 columns (truncated here for the page; the box really is that wide)
│  ♥ Everything In Its Right Place    Radiohead            HI_RES_…  3:00                    …blank to column 198…
│  ♥ Nude                             Bonobo               HI_RES_…  4:14                    …blank to column 198…
```

**Proposed after.** Make the row a budget rather than a constant. Duration is
pinned to the right edge; quality sits immediately left of it; title and artist
share whatever is left, at roughly 60/40; below the width at which artist would
fall under about twelve cells, drop the artist column rather than shave every
column into uselessness.

```
after, 200 columns
│  ♥ Everything In Its Right Place                     Radiohead                      HI-RES    3:00 │
│  ♥ A Very Long Track Title That Will Certainly Need… Some Extremely Long Artist Na… LOSSLESS  3:37 │

after, 60 columns
│  ♥ Everything In Its Right…  Radiohead     HI-RES    3:00 │
│  ♥ A Very Long Track Title…  Some Extreme… LOSSLESS  3:37 │

after, 46 columns (artist dropped, never the duration)
│  ♥ Everything In Its Right…  HI-RES    3:00 │
```

**Why the duration is pinned right and not the title.** `number-tabular`: a
column of times is only scannable when the digits line up, and a duration that
floats at column 66 in a 198-cell box is not a column at all. It is also the
figure a listener actually compares between rows.

**Why this is proposed and not done.** Which column absorbs the slack is a
design decision, the drop order at narrow widths is a second one, and the album
is currently not shown at all even though the issue lists it - so "what belongs
on a row" is open. All three want the maintainer, not me.

**Note on the album.** Issue #15 describes rows as carrying "title, artist,
album, duration and a quality tag". They do not: `row_text` shows title, artist,
quality and duration, and `Track::album` is fetched, stored and never drawn.
That is worth a decision either way - draw it in the width finding 1 frees up,
or drop the field.

## 2. The list title carries key hints, and is clipped on an 80-column terminal — **done**

**Guidance**: the same rule as finding 1, and the repository's own statement
that the bottom row is how bindings are discovered.

```
before, 80 columns - the title runs out of box mid-word
┌Favorites — 12 tracks   (Tab views · j/k move · Enter play · / filter · s shuf┐
```

The title is about 85 cells wide and the box is 78. What gets deleted is a key
hint, silently, on the single most common terminal width there is. The hints
themselves duplicate the bottom row and `?`, and they are phrased differently in
every view:

```
Favorites — 12 tracks   (Tab views · j/k move · Enter play · / filter · s shuffle)
Playlists — 0   (Enter to open · j/k move)
▸ Name — 40 tracks   (Esc back · Enter play)
Mixes — 5   (Enter to open · r refresh · j/k move)
Search: query — 12 results   (i to edit)
```

Five grammars, three of which name `j/k` that the bottom row already names, and
one of which (`r refresh`) names a key no other title mentions though it works
everywhere.

**Proposed after.** The title says where you are and how much there is; the
bottom row and `?` say what the keys are, which is what they are for.

```
after
┌ Favorites · 12 of 431 ───────────────────────────────────────────────────────┐
┌ Playlists · 5 ───────────────────────────────────────────────────────────────┐
┌ Playlists › Weekend · 40 tracks ─────────────────────────────────────────────┐
┌ Search "radiohead" · 12 results ─────────────────────────────────────────────┐
```

This is proposed rather than done because deleting hints decides where those
hints live, which is exactly the "where something lives" line I was told not to
cross. It is also the single highest-value change on this list: it fixes a
silent clip, it removes five grammars, and it buys back a whole row's worth of
width for finding 6.

## 3. `HI_RES_LOSSLESS` is shown raw in the list — **done**

**Guidance**: `navigation-consistency` in spirit - one fact should not have two
spellings in one interface.

`short_quality()` already exists and already turns the wire's
`HI_RES_LOSSLESS` into `HI-RES`. The now-playing badge calls it. The list row
does not, so the list shows the raw wire token, truncated by the 8-cell field
into `HI_RES_…`, while the row two lines below it says `HI-RES`.

```
before                                          after
│  ♥ Nude          Bonobo      HI_RES_…  4:14   │  ♥ Nude          Bonobo      HI-RES    4:14
│  ♥ Weird Fishes  Radiohead   LOSSLESS  4:51   │  ♥ Weird Fishes  Radiohead   LOSSLESS  4:51
```

No decision needed: the tidy spelling already exists, is already the one the
user sees elsewhere, and the ellipsis it removes was carrying no information.

## 4. The bottom row shifts one cell the moment an output opens — **done**

**Guidance**: `visual-hierarchy` and, more to the point, this repository's rule
that a hit box is registered in the walk that paints it - which is satisfied
here, but the whole row still moves.

`device_readout` returned `"OUT —"` with no leading space in the no-output case
and `" DAC S32 · 44.1 kHz"` with one in every other case, and `dac_badge`
prepended a space to whatever came back. So the bottom row began at column 1
with nothing playing and at column 2 as soon as an output opened, taking the
verdict badge, the activity slot, every key hint and every one of their hit
boxes one cell to the right with it.

```
before, nothing playing
 OUT —                    [space] play  [h/l] seek  [?] keys  [q] quit
 ^ column 1

before, playing - one cell right, and so is everything after it
  OUT S32 · 96 kHz  ✓ bit-perfect     [space] play  [h/l] seek  [?] keys …
  ^ column 2

after, playing
 OUT S32 · 96 kHz  ✓ bit-perfect     [space] play  [h/l] seek  [?] keys …
 ^ column 1, as it is when nothing is playing
```

The same asymmetry showed in the `D` report, where the readout is right-aligned
and the leading space put `DAC` one cell left of where `OUT —` sat. Fixed by
making `device_readout` return no leading space in either case and letting
`dac_badge` supply exactly one.

## 5. Truncation counts characters, not cells — **done**

**Guidance**: this repository's own rule, stated twice in `ui.rs` - widths come
from `Span::width`, "the same unicode-width measurement ratatui uses to draw",
because counting `char`s misplaces everything to the right of the first wide
glyph. `ControlBar` obeys it. `graph_line` obeys it. `trunc` does not:

```rust
fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { ... }
    let mut r: String = s.chars().take(n.saturating_sub(1)).collect();
```

A Japanese, Korean or Chinese title - entirely ordinary in a music catalogue -
paints two cells per character while `trunc` counts one, so a 32-cell title
field paints up to 64 cells and every column to its right is destroyed:

```
before, a title of sixteen wide characters in a 32-cell field
│  ♥ 夜に駆ける夜に駆ける夜に駆ける夜に駆ける夜に駆ける夜に駆ける夜に駆ける夜に… Radiohead …
                                              ^ the artist column should have started here
```

**Proposed after**: measure and cut in cells.

```
after
│  ♥ 夜に駆ける夜に駆ける夜に駆…   Radiohead            HI-RES    4:14
```

**Why proposed and not done.** The obvious implementation names
`unicode-width` as a direct dependency. It is already in the tree beneath
ratatui, and this repository has precedent for naming an already-present crate
directly (`ring`, `getrandom`, `libmpv2-sys` all carry a manifest comment saying
so) - but I was told to add no dependencies, and whether this one qualifies as
"already in the tree" is the maintainer's call, not mine.

There is a dependency-free version. `Span::raw(&s[..i]).width()` is the same
measurement with no allocation, so `trunc` can walk `char_indices` and stop at
the last boundary whose prefix still fits. It is quadratic in the field width,
but the field is at most a few dozen cells and there are at most a screen's
worth of rows, so it is a few thousand cheap scans per frame at ten frames a
second. I would still rather the maintainer picked between the two.

A related one-liner in the same function: `trunc(s, 0)` returns `"…"`, one cell
wider than the field it was asked to fit. Reachable from `device_rows`
(`body.width / 2`) and `add_to_rows` (`body.width - 14`) on a very narrow
overlay. That guard is **done**.

## 6. There is no breadcrumb when a playlist or mix is open — **done**

**Guidance**: `nav-state-active` (the current location must be visually
highlighted) and `breadcrumb` (use it at three or more levels). The issue names
this directly: "there is no sense of hierarchy or of where you are beyond the
tab highlight".

Opening a playlist changes the list title to `▸ Name` and leaves everything else
alone. The tab strip still highlights `2 Playlists` exactly as it did one level
up, so the two states are told apart by a single `▸` at the far left of a title
that, per finding 2, may be clipped anyway.

```
before, one level down
 1 Favorites  2 Playlists  3 Search  4 Mixes   ↻    |◁  ▷  ▷|  …
┌▸ Weekend — 40 tracks   (Esc back · Enter play)───────────────┐
```

**Proposed after.** Two candidates, in order of my preference:

```
after A - the trail in the title, with the parent named
┌ Playlists › Weekend · 40 tracks ─────────────────────────────┐

after B - the trail in the tab strip, so the header carries depth
 1 Favorites  2 Playlists › Weekend  3 Search  4 Mixes   ↻  …
```

A is cheaper and does not fight the header for width, which the header does not
have. B puts the depth where the eye already tracks, which is this repository's
own stated reason for putting the controls there - but it makes the tab strip's
width vary with a playlist name, and the tab strip is already losing the theme
control at 80 columns. I recommend A, and would make `Esc` visible as part of
the trail rather than as a hint. Either way this is a decision about where
something lives, so it is proposed.

## 7. The now-playing line and the track rows order the same pair differently

**Taste.** I could not find a rule for this, and the argument cuts both ways.

```
before
 · ♡ Radiohead — Everything In Its Right Place   ·  24-bit · 192 kHz · FLAC · HI-RES
│  ♥ Everything In Its Right Place    Radiohead            HI-RES    3:00
```

The list leads with the title; the now-playing line leads with the artist. Both
are defensible in isolation: a list is scanned down the title column, and a
"now playing" announcement conventionally reads artist-first. The service's own
web client puts the title first in both places, which is the issue's stated
reason for borrowing its shapes. My preference is title-first in both, because
one pair of facts in one interface should have one order and the list is the
place the order matters more.

## 8. The elapsed time is centred and drifts with the terminal width — **done**

**Guidance**: `number-tabular` again, and `visual-hierarchy`. ratatui's `Gauge`
centres its label, so the one number a listener glances at repeatedly sits at
column 34 in an 80-cell terminal and column 94 in a 200-cell one, and moves
while the window is resized.

```
before, 80 columns
                                  0:00 / 0:00
before, 200 columns
                                                          0:00 / 0:00
```

**After**: elapsed at the left edge, the length at the right, bar between - the
shape every player uses, and the shape that keeps both figures in a fixed place.

```
after
 1:01 ████████████                                                      4:05
```

**Done**, with two departures from the proposal above.

*Not hand-rendered.* The row is a three-way `Layout::horizontal` - a fixed
column for each figure, the rest to a label-less `Gauge` - so the bar is still
drawn by the widget that was already drawing it, and only its rect changed.
`progress_rect` becomes the middle chunk rather than the whole row, which is
what keeps click-to-seek honest: the bar a click is measured within is now
exactly the bar that was painted, where before the rect also covered the cells
the centred label sat on. A click on either figure no longer seeks, which is the
cost, and it is the right one - those cells are not part of the bar.

*Length, not remaining.* The right-hand figure stays the track's length. The
defect was where the numbers sat, not which numbers they were, and swapping a
fact out while moving it would have hidden one change inside another. Remaining
is still available as a later taste question.

Both time columns are sized by the **length**, never by the elapsed figure. That
is load-bearing rather than tidy: sized by the elapsed, the left column widens
as `9:59` becomes `10:00`, the bar shifts a cell under a pointer already aiming
at it, and every seek after that is measured within a different rect. It is the
same drift the item is about, in time instead of in width, and it has its own
test.

## 9. Overlay body text hugs the border while footers are indented — **done**

**Guidance**: `whitespace-balance`.

Inside one box, two left edges:

```
before                                          after
┌ Recent log ──────────────────────┐            ┌ Recent log ──────────────────────┐
│Nothing recorded yet.             │            │  Nothing recorded yet.           │
│                                  │            │                                  │
│  j k scroll · g G oldest / …     │            │  j k scroll · g G oldest / …     │
└──────────────────────────────────┘            └──────────────────────────────────┘
```

The same in the device picker's empty state. Every other overlay body already
starts at two cells; these two did not. Pure spacing, no decision. It cost the
log two cells of line width, which is the only thing given up.

**What I left, because it is a judgement rather than a slip.** The two overlays
that have section headings indent them differently: the keyboard reference puts
a heading at column 0 and its rows at 2, and the output report puts a heading at
2 and its readings at 4. Both are a legible two-step hierarchy and neither is
wrong; they are simply not the same step. Worth one decision if the overlays are
ever unified per finding 10.

## 10. The overlay widths are an arbitrary ladder

**Taste**, though `navigation-consistency` gestures at it.

Six overlays, and counting the four screens that are modal in the same way, nine
different width caps and two different margin rules:

| overlay | width cap | side margin | height |
|---|---|---|---|
| recent log | 120 | 2 | terminal − 2 |
| output device | 110 | 2 | terminal − 2 |
| keyboard reference | 84 | 0 | content |
| add to playlist | 80 | 2 | terminal − 2 |
| client identity | 78 | 0 | content |
| output report | 76 | 2 | content |
| sign in | 76 | 0 | content |
| colour theme | 72 | 2 | content |
| prompt / confirm | 64 | 2 | content |

Three of them touch the screen edges on a narrow terminal while the other six
keep a two-cell margin. **Proposed**: three sizes - wide (a list that can be
long: log, devices, add-to), medium (a fixed body: report, help, sign in,
identity, themes), narrow (a question: prompt, confirm) - one margin rule, and
content-height wherever the content is bounded. I have no evidence any specific
number is better than any other, which is why this is taste.

## 11. The report and the device picker are both called "Output"

**Taste.** The `D` overlay is titled `" Output "` and the `d` overlay is titled
`" Output device "`. The keyboard reference already calls the first one "the
output report". Retitling it `" Output report "` would remove a collision
between two adjacent keys, at the cost of changing a label a user has learned.
I did not do it because the maintainer's labels are clearly deliberate.

While reading the report: the `Verdict` section renders its heading with an
empty value when nothing is playing, because `verdict_words` returns an empty
string for `Fidelity::Unknown`. A heading with nothing under it reads as a
failure to load (`empty-states`). Worth a word like "nothing playing", which is
the same answer `access_words` already gives.

## 12. Overlay footers are dead text where the bottom row's keys are live

**Taste**, and the weakest item here.

The bottom row's rule is that every key printed is itself the button. The
keyboard reference obeys it. The other five overlays print their keys as flat
dim text:

```
  j k move · g G ends · Enter choose · x exclusive · click · d, Esc or q to close
```

Nothing there is clickable. This is not a parity defect - every action in those
overlays is reachable with the mouse by other means (the wheel scrolls, a row
click chooses, a click off the rows closes, and the exclusive toggle has its own
hit box) - so the keyboard-only rule is satisfied. It is a consistency question
about idiom, and making five footers into `ControlBar` walks is a medium change
for a small gain. I would leave it, and note it here so it is a decision rather
than an oversight.

## 13. Small wording and spacing — **done**

- The keyboard reference's footer read `press ?, Esc or q to close` where the
  other four read `M, Esc or q to close`. The stray `press ` is gone, so all
  five now share one shape.
- `source_badge` joined the title to the badge with `"   ·  "` - three spaces,
  separator, two spaces - while every separator inside the badge is `" · "`.
  Now `"  ·  "`, symmetric.

## What I did not change and why

Everything in the table marked "propose". In particular I did not touch: the
verdict badge's words, glyphs or colours (ADR-0002); anything about how access
is reported (ADR-0001); the header control cluster's contents or order; which
keys exist; where any action lives; the hint row's reservation logic; or the
theme palettes. Each of those either has a written decision behind it or is a
question about where something lives, which issue #15 explicitly reserved.

## Suggested order, if the maintainer wants one

1. Finding 2 (list titles). Highest value, unblocks width for everything else,
   and fixes a silent clip at the commonest terminal width.
2. Finding 1 (responsive rows). The change the issue is really about.
3. Finding 6 (breadcrumb), which is cheap once 2 is done.
4. Finding 5 (cell-accurate truncation), which finding 1 makes more visible.
5. Finding 8 (progress row), separately and carefully, because seeking.
6. Findings 7, 10, 11, 12 as taste, in any order or not at all.

That order was followed. Steps 1 to 5 are done; what is left of this audit is the
last line, the four taste findings, and nothing else.

---

## Settled decisions

Worked through with the maintainer question by question. These supersede the
"left for a decision" items above. **They are recorded here rather than only in
the issue, because `tea` truncates every comment body at about 80 characters in
all of its output formats including JSON - so a decision that lives only in a
comment cannot be read by anyone working from the tracker.** Two implementations
proceeded partly blind before that was noticed.

1. **The row is a verdict; the overlay is the report.** The bottom row had grown
   to ~108 columns of badges before a single key hint, so at 80 columns every
   hint was already being dropped. The row answers *whether*; `D` answers *why*.

2. **The row keeps the verdict and the device readout; access moved to the
   overlay.** Access is a session-long setting rather than something that
   changes per track.

3. **One short vocabulary, no numbers**: `✓ bit-perfect`, `≈ near bit-perfect`,
   `⚠ resampled`, `⚠ truncated`. The rates are already on screen twice - the
   source badge carries the track, the device readout carries the output - so
   repeating them is redundant. What cannot be derived at a glance is *which
   kind* of alteration.

4. **No remedy on the row.** Dropping the inline `0 for unity` is not about
   width: that remedy only ever fixed priel's own volume, and once the sink is a
   possible cause an inline remedy would be actively wrong.

5. **`D` is a sectioned output report**, each section rendering on its own
   evidence. It previously short-circuited entirely on the direct path, which
   would have hidden the volume section from exclusive users.

6. **Grade on what was read, and label it**: `✓?` when a stage exists and could
   not be read. Follows the `DAC`/`OUT` precedent - still grade, but say what the
   grade rests on.

7. **Absent is not unreadable.** A stage that cannot exist counts as fully
   evidenced, so the direct path keeps a clean `✓`. Without this the cleanest
   chain the player can produce would carry a permanent question mark.

8. **The partial mark is a glyph, not a colour**, so it survives a light theme,
   a dark theme, a monochrome terminal, and the red/green deficiency.

9. **More detail on the main screen.** A row was a fixed 72-cell block: 116
   cells blank at 200 columns, while below 74 columns quality and duration were
   silently clipped. Progressive disclosure by width, documented drop order
   (album → artist → tier), duration pinned right.

10. **The now-playing block is three rows along the bottom at every width, and
    the right-hand column belongs to the queue alone** (issue #27). Three parts,
    and the first of them is a correction:

    - **Nothing about now-playing depends on the width any more.** For one
      release it became a side panel at 120 columns and up, and this entry said
      that freed two of the three chrome lines. **Those two rows are spent
      again, deliberately.** What the panel bought in height it cost in
      steadiness: the four facts a listener glances at without looking for them
      - what is playing, where it has got to, what it is going into, and the
      verdict on what arrives there - moved to a different edge of the screen
      depending on the terminal, and a fact that moves has to be looked for. One
      place at every width beats two rows.

      The route this arrived by is worth recording, because it is the failure
      mode and not the layout. Two layouts were offered and a question asked
      about which was wanted; no answer chose one, and a *follow-up* question
      about focus carried the panel layout in its mockup and was approved. A
      mockup inside a question about something else is framing, not a decision.

    - **The column at 120 columns and up is the queue and nothing else**, full
      height beside the list, 36 cells including its borders. Fixed rather than
      a share of the width, for the reason the panel's width was fixed: a queue
      row is a mark and a title, and a column that grew with the terminal would
      pad short lines with width the list has five columns to spend.

    - **It is shown by default and can be folded away.** `W` hides and restores
      it - the shifted sibling of the `Ctrl-W` that moves the keyboard between
      the two regions, so the pair names the window rather than spending a
      second idiom - and a `▤` in the header does the same through the same
      method. That control is drawn only at 120 columns and up, because below
      that it would do nothing where it was clicked. A folded column publishes
      no `queue_inner`, which is what makes it unfocusable and unclickable by
      exactly the route a narrowed terminal already used; `Ctrl-W` on one says
      which key brings it back rather than blaming a width that is not the
      problem.

11. **Zebra striping is a theme role, and `terminal` opts out** - it inherits a
    palette priel cannot read, so guessing a stripe there is the overstatement
    that theme exists to avoid. Precedence is cursor > playing > stripe.

12. **The active tab carries a background**, herdr-style, and the title carries
    the path (`Playlists › Deep Cuts`).

13. **The list title's per-view key hints are deleted.** They were clipped
    mid-word at 80 columns, deleting a binding, and duplicated the bottom row.

14. **Truncation measures display width**, via `unicode-width` named directly -
    already in the tree via ratatui, so no new crate compiles.

15. **The verdict badge opens the report when clicked**, through the same method
    `[D]` uses.

16. **The queue is a second focusable region** (issue #25). Six answers were
    settled with it, and each is here because the alternative was reachable.
    It arrived in the now-playing panel; decision 10 above has since given it
    the column to itself, which changes where it is drawn and nothing about the
    six:

    - **No new region and no second breakpoint.** The column exists at 120
      columns and the queue is what is in it. **Below 120 columns there is no
      queue view at all** - not a narrower one, and not a modal. That is the
      decision: a queue squeezed into a narrow terminal would be taking cells
      from the list, which is the part of the screen a narrow terminal can least
      afford to shrink and the reason someone is running one. `Ctrl-W` there
      names the width that brings it back rather than doing nothing.
    - **`Ctrl-W` moves the keyboard between the two.** `Tab` is taken by the
      view cycle. `Ctrl-W` is vim's own window key, so a VIM-first client spends
      no letter on it - and because priel has exactly two regions rather than
      vim's arbitrarily many, the *prefix* is the whole move and no direction
      follows it. A third region would have to grow one.
    - **A click into either box does the same thing**, which is the gesture
      nobody has to be taught, and the wheel follows the pointer for the same
      reason. Both go through one method, so the key and the pointer cannot
      leave the focus in two places.
    - **Which box has the keyboard is said twice, and one of the two carries no
      colour.** The cursors differ by a backing - `selection_idle_bg`, a new
      theme role measured in all eleven palettes - and the focused box is drawn
      in the heavy box-drawing set. A backing is the one thing a monochrome
      terminal cannot show, so the border is what makes the answer survive one.
      The unfocused box keeps its cursor and its ordinary text: it is not being
      driven, which is not the same thing as being disabled.
    - **History above, dimmed, and `Enter` on it plays it.** That is what makes
      "backward" navigation rather than a second spelling of the
      previous-track key.
    - **Provenance is a column, not a shade.** An entry the radio added is
      marked `~` whatever else is true of it, because it can be a suggestion and
      be in the history at once. The rule is the positional one
      `playing_from_radio` already used, moved into `App::suggested` so the mark
      on a row and the word beside the playing track are one rule with two
      callers.
