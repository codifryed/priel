# A display of the audio costs the gapless transition

Status: accepted

A spectrum needs the decoded samples, and the ordinary way to reach them puts
something in the audio path - which is the one thing this player exists not to
do. A display that moved so much as a bit of what the device receives would
contradict the badge above it, and the badge is the feature. So the question was
never whether a spectrum can be drawn; it was what has to be given up to draw
one. The answer is that the samples are safe and the gapless transition is not.

The evidence below was taken against mpv 0.41.0, libmpv 2.5.0 (`client.h`,
`render.h` and the shipped manual page) and the libmpv2 6.0.0 wrapper. Every
measurement is committed and re-runnable; the commands are at the end.

## What the player will and will not hand over

**libmpv has no way to give decoded audio to the program embedding it.** There
is no audio counterpart to the render API, and `client.h` says why in passing,
where it explains that `MPV_EVENT_AUDIO_RECONFIG` is uninteresting "because
there is no such thing as audio output embedding". Nor is there a filter that
exports samples: mpv 0.41 offers `lavfi`, `lavfi-bridge`, `scaletempo`,
`scaletempo2`, `format`, `rubberband`, `lavcac3enc` and `drop`, and none of them
hands anything back to the caller. There is exactly one audio output, so a
second one that wrote samples somewhere priel could read them is not available
either.

What mpv does offer is `--lavfi-complex`, and the manual's own example is the
shape this needs: `[aid1] asplit [t1] [ao] ; [t1] showvolume [t2] ; [vid1] [t2]
overlay [vo]`. The decoded stream is split, one copy goes to the output and the
other into an analysis filter. **The analysis result is a video stream**, which
turns out to matter more than anything about the audio.

## The samples are not touched, and that is measured rather than argued

`measure_what_an_analysis_branch_does_to_the_output` plays a synthesised 24/96
signal through mpv's PCM writer, which stands exactly where the device would,
twice without an analysis branch and once with one, and compares the bytes. All
three captures are identical, to 4 798 020 bytes. The third capture is not
padding: without it, two matching captures could mean nothing more than that the
source repeats, and two differing ones nothing more than that it does not.

The reported parameters agree. With the branch attached the decoded and the
output format both read 96000/s32, the same as with no graph at all - and they
still do when the analysis branch is deliberately made hostile, resampled to
8 kHz and narrowed to `u8` before the filter. libavfilter converts per link, so
the conversion lands after the split, on the copy nobody listens to.

This is not the grading being blind to filters in general. A filter that really
does stand in the path is reported: `--af=lavfi=[aresample=48000]` on 44.1 kHz
content reads 44100 in and 48000 out, because `audio-params` is what the decoder
produced and `audio-out-params` is what was written to the audio API. The
distinction the indicator rests on survives.

## Would the fidelity grading change while a display is running? No.

That is the acceptance test for the whole idea, so it is stated flatly.
`PlaybackStatus::fidelity` reads five things: the decoded rate, the output rate
and format (from the ALSA readout where there is one), priel's own volume and
the audio server's. An analysis branch moves none of them. A track graded
`BitPerfect` with the display off is graded `BitPerfect` with it on, and no
track moves into `Altered` or `NearBitPerfect` because of it. The hardware
readout is downstream of all of this and sees nothing either, since the bytes
handed to the audio API are the same bytes.

## What it costs to run

**The analysis itself, measured.** `measure_the_cpu_an_analysis_branch_costs`
plays in real time on the null output and reads the process's own CPU counters.
Decoding 24/96 alone costs 0.1% to 0.2% of one core; with a constant-Q analysis
of the split copy at 120x32 it costs 1.4% to 1.5%. The branch is therefore about
ten times the decode it is added to. Small in absolute terms, and not small
relative to what the player otherwise does. The filter chosen dominates that
figure: at the same size and the same graph, run through the mpv binary,
`showcqt` costs about 2.3% of a core and `showspectrum` about 5.9%, and both
roughly double at 24/192.

**The redraw, measured.** `measure_what_redrawing_on_every_tick_would_cost`
builds a full 120x40 frame from nothing in about 161 microseconds, which is 0.2%
of one core at the ten frames a second the event loop's 100 ms poll allows. So
the drawing is not what a display would cost, and the "redraw only when
something changed" rule is not the saving at risk - though it would be disabled
for as long as a track plays, because a display driven by the audio marks the
screen changed every tick. Ten frames a second is also visibly coarse for a
spectrum, and going faster means shortening the poll for everything.

**Getting the numbers back, not measured, because it is a design cost rather
than a runtime one.** The analysis arrives as video. mpv's terminal video
outputs (`tct`, `kitty`, `sixel`) write escape sequences into the terminal priel
already owns, so they are out. That leaves the render API, and libmpv2 6.0 wraps
only its OpenGL half - a GPU context for a terminal program. The software
renderer that would actually apply is reachable only through `libmpv2-sys`,
would be a third `unsafe` block in the workspace, and `render.h` introduces it
as "extremely simple (but slow) ... You probably don't want to use this".
The bars would then have to be measured back out of a bitmap that a filter had
just drawn them into.

## The cost that decides it

**A complex filter graph does not survive a playlist transition with the audio
output open.** `measure_whether_an_analysis_branch_keeps_the_transition_gapless`
plays two files back to back and counts the times mpv opens the output: once
without the branch, twice with it. That second open is the gap.

It is not a setting that could be traded away. The count is two with
`gapless-audio` set to `weak`, to `yes` and to `no` alike, so the "weak" choice
that keeps playback bit-perfect across a sample-rate change is not what causes
it. The graph is rebuilt for every playlist entry and takes the output with it.
An `--af` chain does not do this - two files, one open - but an `--af` chain
cannot produce a picture and cannot hand samples over either, so it is not a
route to anything.

Gapless is a stated invariant of this player, with a whole preload mechanism
built around it. Trading it for a decoration is the wrong way round.

## The alternatives, and what each gives up instead

**A second player instance decoding the same bytes**, writing PCM to a pipe
priel reads. It leaves the playing instance untouched, so gapless survives and
so does the badge. It costs a second decode of every track; it reaches only the
segmented sources, because the other kind hands mpv a URL priel never reads and
would have to be fetched from the network twice; it has its own clock to keep in
step with the one being listened to; and priel would then need an FFT, which is
a new dependency in a library crate - and so a dependency for every future
frontend - or a hand-rolled one, either way for a decoration.

**Decoding in priel directly** is the same list plus a FLAC decoder.

## Could it be done without a new dependency?

For the route that keeps the samples intact, yes. libmpv2's `render` feature is
on by default and pulls in no crate, and `libmpv2-sys` is already a direct
dependency for `mpv_request_log_messages`. The price of that route is not a
crate; it is unsafe FFI and the gapless transition. The pipe route is the one
that would need a new crate, in `priel-player`, and that is where the dependency
policy bites hardest.

## The decision

**Not built.** The README roadmap entry said "if it can coexist with bit-perfect
output"; it can, and that is not the constraint that stops it. The entry is
rewritten to say what does, so the question is not reopened from the beginning.

No follow-up implementation issue is opened. If this is picked up again, the
thing to solve first is not the analysis and not the drawing: it is that a
complex filter graph reopens the audio output at every track boundary. Should
that change in mpv, everything else here is favourable and the measurements can
be re-run to confirm it.

## Consequences

`vid` stays `no`. Turning video on is not a free switch: it is what the
`[vo]` half of a complex graph requires, and this decision is the reason there
is no call for it.

The measurements stay in the tree as ignored tests, so the finding can be
checked rather than believed:

```
cargo test -p priel-player -- --ignored --nocapture measure_
cargo test -p priel-tui    -- --ignored --nocapture measure_
```

They need no media file, no network, no credentials and no audio device: the
signal is synthesised by the ffmpeg already inside libmpv, and everything plays
to the null output or to a temporary file.
