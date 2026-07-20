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

## Consequences

Requesting exclusivity and holding it are two different facts, and only the
second may be shown. That is why the player publishes an `OutputAccess` of
`Shared`, `Exclusive` or `Refused` rather than echoing the request back: the
interface renders what the player *achieved*, and the flag the user set lives
separately, in the picker, as the thing they can still toggle.

A refusal is judged by the same symptom a bad device change already is - a file
loaded with no output open - so it reuses the existing switch-and-settle
machinery rather than growing a second, competing one. Only the arming differs:
a change made while something is playing is judged on the short reinit grace,
while a request made before anything has loaded stays unjudged until a track
exercises the output.

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
