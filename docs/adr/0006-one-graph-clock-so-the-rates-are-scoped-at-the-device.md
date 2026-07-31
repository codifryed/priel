# One graph clock, so the rates are scoped at the device

Status: accepted

A "set up audio" pass writes `default.clock.allowed-rates`, and that setting
only ever grows. Every rate priel adds for a DAC stays in the list when the DAC
is unplugged, because a rate priel did not add may be there for something else
on the machine and taking one away could break it. Switching DACs is routine -
a desk DAC, a portable one, Bluetooth headphones - and after a few of them the
list holds rates that nothing currently connected can do.

The obvious ask is to make the setting per device. It cannot be, and the reason
is worth writing down once because the question will be asked again.

## `default.clock.allowed-rates` is global, and there is no per-device version

`PipeWire` has **one graph clock**. `default.clock.allowed-rates` is the set of
rates that clock may switch between, and it is a `context.properties` setting on
the daemon. There is no per-device spelling of it, and there could not be one:
two devices at two rates in one graph is not a thing the clock can express.

So the permitted list must hold the *union* of every rate any device should be
able to run at, and priel keeps writing it that way.

## `audio.allowed-rates` is per node, and narrows

What does exist is `audio.allowed-rates`, a **node** property. `pipewire-props(7)`:

> The allowed audio rates to open the device with. Default is `[ ]`, which means
> the device can be opened in any supported rate. Only rates from the array will
> be used to open the device. When the graph is running with a rate not listed in
> the allowed-rates, the resampler will be used to resample to the nearest
> allowed rate.

In `spa/plugins/alsa/alsa-pcm.c` it changes what the node advertises: with the
property set the node publishes a discrete `SPA_CHOICE_Enum` of exactly those
rates instead of a min/max range, and every entry is `SPA_CLAMP`ed to the
hardware's own range first. **It can only narrow.** A rate the device does not
have cannot be added this way, which is exactly the property that makes it safe
to write on the listener's behalf.

That is the tool for the problem. The union stays global; the per-device rule
keeps the union from being applied to a DAC that cannot do all of it.

## The decision

A rates pass writes **two files** on one approval, one preview and one restart,
because they are two halves of one setup:

- `99-priel-rates.conf` in `pipewire.conf.d` - the permitted list, the union, as
  before.
- `99-priel-rates-<node>.conf` in `wireplumber.conf.d` - a `monitor.alsa.rules`
  entry matching the active sink's `node.name` exactly, setting
  `audio.allowed-rates` to the rates that node supports.

The session manager's directory, because it is the session manager that applies
node properties - the same reason the reservation rule lives there.

### One file per node, not one file with a rule list

Switching DACs is the case this exists for, so setting up the second must not
disturb the first. A single shared file would mean reading back what priel wrote
for the last DAC and merging into it, which is either SPA-JSON parsing or a
state file to regenerate from. A file per node keeps every rule independently
authored and independently deletable, and keeps the module's standing rule -
priel only ever writes files it is the sole author of - free of any
parse-and-merge machinery. The basename is the node's own name, sanitised to
what a path may hold, so the file is recognisable in a directory listing and two
nodes can never claim the same one.

### Only from rates that were read

`audio.allowed-rates` is priel asserting what a device takes, so it is written
only from the exact list `/proc/asound/card<N>/stream*` gave. That descriptor is
USB-Audio only, which covers the hi-res DACs priel exists for and nothing else.
Where it is absent - a Bluetooth sink, an HDA codec - no rule is written at all.

Every ALSA node does publish an `EnumFormat` rate *range*, and
`AudioGraph::sink_ceiling_hz` reads it. It is **not** promoted into a list.
Filling the 44.1k and 48k families in between would put rates in the assertion
that the device may not support, which is the precise failure this rule exists to
prevent, and priel's name would be on it.

What the ceiling *is* used for is narrowing the list that was read, which is a
different thing from filling a gap in it. `stream*` is per USB **interface** and
unioned across the card, so a card whose outputs differ from one another reports
the best of them for all of them. A measured example has 192 kHz speakers and
384 kHz headphones on one card: without the ceiling the speakers' rule would have
carried 384 kHz, which is priel asserting a rate that output does not have. With
it the speakers get their six rates and the headphones their seven, from the same
unioned list.

`mediaSubtype: "dsd"` entries are skipped when taking that ceiling. A hi-res DAC
advertises DSD beside PCM and the DSD figure is the bit rate of a one-bit stream
- the SMSL publishes 3.072 MHz next to its 768 kHz of PCM. It is not a sample
rate anything is compared against, and counting it would both put an
unreachable number on screen and wave every PCM rate past the filter.

## The line-length consequence

Both files are shown whole in the preview before anything is written, and that
box clips rather than wrapping. A full hi-res rate list is ten entries, and ten
on one line ran twenty columns past the box - so the existing permitted-list file
was already losing its tail on exactly the machine this feature is for. Both rate
lists now carry onto further lines past `RATES_PER_LINE`, and a test holds every
line of every drop-in inside the preview's width. Whitespace separates array
elements in SPA-JSON, so a newline changes nothing about how they parse.

The previewed path is printed as its directory and then its filename, on two
lines, for the same reason: a per-device rule is named after a node the listener
plugged in, so neither half of that path has a bound priel controls.

## What is deliberately not here

- **Pruning the permitted list.** priel adds and never removes. A rate it did
  not add may be there for something else, and this rule is what makes the
  accumulation harmless.
- **A per-device rule for a Bluetooth sink.** Its rates are the codec's, not a
  card's, and `monitor.alsa.rules` does not reach it.
- **Mapping a `stream*` file back to the PCM a node opened.** That would give
  each output its own list at the source rather than a unioned one narrowed by
  the node's ceiling. The file numbering is per USB interface and does not follow
  the PCM index reliably enough to key on, and the ceiling already lands the same
  answer on the card that motivated it.

## Consequences

- A listener who sets up a DAC gets every rate it supports in one pass, and that
  DAC held to exactly those rates, without touching a config file.
- The permitted list still grows across DACs, and that is now harmless rather
  than something to apologise for.
- `ToWorker::SetUpAudio` carries an optional device, `FromWorker::AudioSetUp`
  carries every path written rather than one, and `SetupStep::Restart` lists
  them - a pass that wrote two files and reported one would leave the second
  unaccounted for on the only screen that says what priel put on the machine.
