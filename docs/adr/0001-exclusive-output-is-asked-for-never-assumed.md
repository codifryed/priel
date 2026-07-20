# Exclusive output is asked for, never assumed

Status: accepted

The sound server is the right default and stays it, but it is a mixer: even
configured well it owns the device, and any other application that starts can
reshape the graph. The only way to remove that variable is to open the device
exclusively. priel therefore offers that path, and the user-facing shape of it
is settled as follows.

## The decisions

**Requesting it is a separate switch.** A boolean `--exclusive` flag, used
alongside the existing `--device`, plus a toggle in the output picker (`x`, and
the same control is clickable) for interactive use. Both drive the same code
path. Exclusivity is deliberately *orthogonal* to which device is chosen:
selecting a hardware device does not imply exclusive access, so opening one
non-exclusively stays possible. Spelling it as a variant of `--device` would
have welded the two together and taken that away.

**priel never selects it on its own.** Under no circumstance - including when
the sound server is known to be resampling, which is exactly the situation the
bit-perfect indicator exists to reveal - does priel choose the exclusive path
by itself. Taking exclusive control of a device silences every other
application on the machine, and that is not a side effect of pressing play. The
indicator names the problem; the user decides whether to take the device.

**A refused open falls back to the shared path, loudly.** Exclusive access
fails when something else already holds the device, and the underlying player
does not degrade gracefully - it abandons the file and stops. So priel drops
the request, reopens through the shared path so the music keeps playing, and
says so three ways: a visible notice, a line in the diagnostic log, and the
output badge, which reports shared output. **The indicator never claims
exclusivity it did not get.** A player that silently fell back to the mixer
while the badge still implied a direct connection would be worse than not
having the feature at all.

## What "the shared path" resolves to

"Reopen through the shared path" is not a single thing, and getting it wrong is
silent. A hardware device has **no shared spelling**: `alsa/hw:CARD=AUDIO,DEV=0`
*is* the card, it admits one opener, and clearing the exclusive flag changes
nothing about it - the flag was never what made it exclusive. Reapplying the
same device after a refusal therefore lands on the same held card again, which
is exactly what hardware testing found.

So the fallback resolves in this order:

1. **A device the sound server owns** was refused *by* the sound server. The
   same device shared is what was wanted, so the device does not move and
   dropping the request is the whole of the fallback.
2. **A hardware device** falls back to **the sound server's own entry for the
   same card** - the same physical DAC, just shared. The two identifiers share
   no substring, so they are paired through the card the server publishes on its
   sink node (`alsa.id` and `alsa.card`), never by matching the strings.
3. **No entry for that card, or no sound server at all**, leaves nothing to map
   onto, so the output falls through to the system default sink.

**The track is loaded again and plays from the start.** The player abandons the
file when the output will not open, so restoring a working device recovers
nothing on its own - without the reload the interface waits forever for a track
that is no longer loaded. The position is lost and that is accepted; the bytes
are still buffered, so this costs no refetch, only the seconds already heard.

None of this is particular to exclusivity. An ordinary device change that fails
abandons the track in exactly the same way, and reloads it in exactly the same
way; only the choice of where to fall back to differs.

## Consequences

Requesting exclusivity and holding it are two different facts, and only the
second may be shown. That is why the player publishes an `OutputAccess` of
`Shared`, `Exclusive` or `Refused` rather than echoing the request back: the
interface renders what the player *achieved*, and the flag the user set lives
separately, in the picker, as the thing they can still toggle.

A refusal is judged by the same symptom a bad device change already is - a file
loaded with no output open - so it reuses the existing switch-and-settle
machinery rather than growing a second, competing one.

**The verdict is reached when the player says it gave up on the file, not when a
timer expires.** That was not the first shape: a fixed grace period decided it,
and hardware testing heard the whole of it - eight seconds of silence with a
buffering indicator over it, the indicator itself lying, because nothing was
buffering and the load had already failed. Giving up on a file is deterministic
and is the only place a failed load is visible at all, so it drives the
recovery, and the timer stays only as a backstop for a failure that produces no
event.

Two things follow, and both are load-bearing. **A failed load is not proof of a
refusal** - a missing file, a corrupt stream and an aborted buffer all arrive
identically - so what decides is still whether an output is open, the same
question a device change was always judged by. And **judging a change consumes
it**, which is what makes a reload loop impossible: the reload may fail in turn
and produce another event, and that one arrives with nothing pending, moves no
device and reloads nothing.

A request made before anything has loaded stays unjudged until a track exercises
the output, since an idle player and a refused device look identical.

The direct path also puts priel **outside the sound server**, so there is no
graph between it and the DAC to report on. That is a distinct answer from "priel
has no stream in the graph", which means the graph exists and priel is not in it
yet - it reads as "nothing is playing", the opposite of the truth here. The
player knows which device it holds, so the overlay is told rather than left to
infer an absence.

The direct ALSA path itself needs no new mechanism: it is `--device alsa/...`,
which the picker and `--list-devices` already offer. What `--exclusive` adds is
the request for the device to be priel's alone. On ALSA that is inherent in a
`hw:` device; on the sound server it is a request the server may refuse, which
is the case the fallback above exists for.

One consequence reaches into the hardware readout. The live device parameters
are found by matching the device identifier against the ALSA card, and that
match was a plain substring test - which works for a sound-server identifier
carrying the card name, and fails for `alsa/hw:2,0`, which carries the card
*index* instead. The readout is the entire justification for the exclusive
path, so the match now understands `CARD=<id>` and `hw:<index>` as well.
