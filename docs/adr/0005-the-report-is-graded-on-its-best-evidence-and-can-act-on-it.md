# The report is graded on its best evidence, and can act on what it finds

Status: accepted

The output report on a Bluetooth link showed this, in one box, at one moment:

    Verdict                                          ≈ bluetooth (aptX HD)
    Device
      output                                              OUT S16 · 48 kHz
    Chain
    mpv  (priel)                                     44.1 kHz  S16LE  2 ch
    Px7 S3  (device)                                 44.1 kHz  S24LE  2 ch
    Server clock
      permitted                                                    48 kHz
      this track                                  44.1 kHz  not permitted
    This rate is not one the server is permitted to use.
    Put this in ~/.config/pipewire/pipewire.conf.d/10-rates.conf:
      ...

Four things are wrong with that, and they are four separate defects that
happened to land on one screen. The device is reported at 48 kHz directly above
a chain in which every node says 44.1 kHz. The verdict is yellow - an accepted,
inherent limit with nothing to be done - while the section below it says the
rate was refused. The chain accuses nobody, though something evidently moved.
And the one line that says what to change is prose, naming a file priel does
not write, with no key that writes it.

## 1. The output rate is read off the strongest evidence, in a stated order

`effective_output` had two rungs: the ALSA readout, and then the sound server's
*global* clock settings. There was no third, so on any sink with no ALSA readout
- every Bluetooth and network output - the global clock decided the verdict.

`default.clock.allowed-rates` governs the driver rate of an ALSA sink. A bluez
sink is its own driver at the rate bluez negotiated with the device, and the
global list does not govern it at all. So on the one class of output where the
clock was load-bearing, it was also least applicable, and the report asserted a
rate that the dump it was built from contradicted two rows further down.

The ladder is now four rungs, each further from the samples than the one above:

    hw            the ALSA device itself, the only unmediated view
    sink_rate_hz  what the node at the end of the graph negotiated
    clock         what the server's global settings say it may run at
    sample_rate   the rate priel handed over, which hides anything below it

The sink node's rate was already in every graph read - the chain has always
drawn it - and only the codec was being threaded to the player from those reads.
It now rides the same wire.

**`observed_output_rate` is separate from `effective_output`, and the split is
load-bearing.** The fourth rung is not an observation of the output; it is an
echo of the input. Anything asking "is this rate evidently in use?" that reads it
answers yes by construction. `effective_output` needs *a* rate to display and so
takes the echo; every judgement takes `observed_output_rate`, which is `None`
when nothing below priel said anything.

**The advice defers to that same observation.** A rate the output is observed
running at is a rate this graph may use, whatever the global list names, so no
change is advised over it. The deference reads the ranked answer and never the
sink node alone - which is what keeps the case an ALSA readout catches: a card
clocked away from the rate its own node negotiated *is* resampling, the clock is
what explains it, and that advice has to survive.

## 2. The link is graded beside the sample stream, never instead of it

`Fidelity::Bluetooth` was a grade, and it was returned before the rate and depth
were compared at all. The reasoning at the time was that the link loss dominates:
an A2DP link re-encodes every sample, so nothing beyond it is bit-perfect and no
setting here changes that.

That is true and it is not a reason to hide anything. A lossy transport and a
rebuilt sample stream are findings about two different things - what carries the
samples, and whether something above rewrote them - and they are independent.
Folding one into the other made them exclusive, so an output with both reported
only the smaller: the one nothing can be done about.

It cost a second thing that no one would predict from the badge. The chain is
only asked to name a culprit for an alteration the grade admits to, so a
Bluetooth output could never have an accused node either. The reader was shown a
chain, a rate that moved inside it, and nothing joining the two.

So `Verdict` carries a third field beside `fidelity` and `evidence`:

    Verdict { fidelity, link: Option<Link>, evidence }

with the colour rule stated once: **a rebuilt sample stream is red whatever
carries it.** The link is graded on its own only where the stream above it is
intact - yellow on the best codec the device offers, red when a better one exists
or cannot be told, exactly as before. The words name both findings in the order
the samples meet them: `⚠ resampled → bluetooth (aptX HD)`.

## 3. Every change the report describes has a key that makes it

The report knew five changes and could make two of them, and the gate on one of
those two disagreed with the advice printed above it.

`RateAdvice::Missing` is decided from the clock alone, but the offer behind `[A]`
hung on `blocked_supported_hz` - the device's own rates, read from
`/proc/asound`, and absent on every sink that names no ALSA card. On exactly the
outputs where the clock is the only evidence there is, the report printed the
file to write and the setting to put in it and offered no way to write it. The
offer now has both sources, and takes the track's rate from the same advice and
the same observation the report is written from, so an offer cannot appear over a
section that says there is nothing to change. The prose also named
`10-rates.conf` while priel writes `99-priel-rates.conf`; the server takes the
last value of a property rather than the union, so a reader who followed the
words wrote a file that whichever sorted later would silently override.

The two remaining prose-only changes - clearing `clock.force-rate`, and reserving
the card from the session manager - are now actions. Both were already exact
command strings and exact file contents in the report; they are the same bytes
with a confirm in front of them.

**One flow, not one per action.** All of them ask before touching anything, all
report what came of it, and two of the three land a drop-in that a restart has to
pick up. `Setup` carries a `SetupWhat` saying which it is; giving each its own
struct, overlay and step machine would be three copies of that shape drifting
apart, and the confirm is the part that must never be the one that drifts.

## 4. The actions section is always drawn, and the actions in it are not

Every other section of the report is silent where it has nothing to report, by
the rule that advice printed over a working setup teaches the reader to ignore
it. The actions section is the deliberate exception: **an action that appears
only when it is needed cannot be found before it is needed.** A heading that is
always there, with one line saying there is nothing to change, is what makes a
short list the rest of the time mean something.

The rows under it stay conditional. A permanent list of everything priel could
theoretically do, greyed out, would be that same noise with a mouse target
attached.

Each row carries the key that runs it, and the renderer registers a hit box over
the key it painted - so the row *is* the button and a click runs the key handler
rather than a second implementation of the action. That is also why the actions
left the footer: the footer drops what does not fit as the terminal narrows,
which would delete an offer from the one screen that exists to say what can be
done.

## What is deliberately not here

**No PulseAudio and no raw-ALSA equivalent.** Pulse proper has no
`allowed-rates`; the nearest thing is `/etc/pulse/daemon.conf`, system-wide and
root-owned, and priel writing there breaks the rule that it only ever authors
its own drop-ins. On the direct path there is nothing to configure - priel
already owns the card.

**priel still writes only files it is the sole author of**, now two of them
rather than one: `99-priel-rates.conf` under `pipewire.conf.d` and
`99-priel-reserve.conf` under `wireplumber.conf.d`. Neither edits the sound
server's own configuration or one the listener wrote. Clearing the rate pin
writes nothing at all - `clock.force-rate` is live metadata, so it is set back to
zero, takes effect at once, and leaves nothing behind to delete.

## Consequences

- A test fixture whose sink node sits at a rate the server refuses is not a
  machine that exists, and it is the one shape that hides the advice. Three
  fixtures had it by accident and were corrected; `chain_clocked` now puts its
  sink where the clock leaves it.
- `Fidelity` no longer has a `Bluetooth` variant. Anything matching on the grade
  is matching on what happened to the samples, and asks `verdict.link`
  separately about what carries them.
- The report is roughly six rows longer. It scrolls, as it always could.
