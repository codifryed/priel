# The session bus is spoken directly, or not at all

Status: accepted

Putting priel on media keys, on the lock screen and in a panel applet means
implementing MPRIS, and MPRIS is a D-Bus service. There is no bus code in the
workspace, so the first question is not what to publish but what to publish it
*over*. Four candidate transports were measured rather than guessed at, and the
answer changes an architectural guarantee if it is got wrong.

The goal that decides it: **one binary that runs on a small media-server box
with no desktop libraries installed, and simply does not offer MPRIS there.**
Not a second build, not a second package. That is a runtime question, and three
of the four candidates answer it at link time or not at all.

## What was measured

- **zbus** cannot be had without an async runtime. 5.18 carries a literal
  `compile_error!("Either \"async-io\" (default) or \"tokio\" must be
  enabled.")`; `zbus::blocking` is a synchronous face on an async core, not an
  alternative to it. It adds **65 crates** to a tree of 103, among them
  `async-executor`, `async-io`, `async-task`, `polling` and `futures-*`.
- **dbus-rs** is genuinely synchronous and costs only 2 crates priel does not
  already have. It links `libdbus-1` through pkg-config, which puts a
  `DT_NEEDED` entry in the binary: on a machine without `libdbus-1.so.3` the
  process **fails at exec, before `main`**. There is no code that can degrade
  gracefully from behind a missing shared object. `libdbus-sys` offers a
  `vendored` feature that compiles bundled C, which trades the runtime failure
  for a bundled C library and a build-time compiler - both things distro
  packaging exists to remove.
- **libdbus is not structurally present.** The development machine runs
  `dbus-broker`, which does not need it; `libdbus-1` is installed only because
  sixty desktop packages happen to pull it in. A headless box has no such
  packages.
- **rustbus** was the option not on the list, and it clears every constraint:
  pure Rust, no libdbus, no `DT_NEEDED`, no executor, 7 new crate names. It is
  also dormant - last release 2023-08-29, last commit 2024-06-22 - and pins
  `nix` 0.26 and `bitflags` 1.x, which would put a second major of `bitflags`
  in a tree that already carries 2.13.

## The decision

**priel speaks the D-Bus wire protocol itself, over the session socket, with no
new dependency.** The transport is a Unix stream socket, a SASL EXTERNAL
handshake and a marshaller; the specifications are the D-Bus Specification
**0.43** and the MPRIS D-Bus Interface Specification **2.2**, both of which
describe a protocol that has not changed shape since 2006.

**The no-async-runtime guarantee is unaffected, and that is not a coincidence.**
It survives because nothing about publishing playback state is concurrent: a
blocking read on one socket, answered from a state snapshot, is the whole
program. The guarantee was never the reason to avoid a bus library - the reason
is that every bus library either brings an executor or brings a shared object,
and priel wants neither. Speaking the protocol is what removes both at once.

**Bus code lives in the binary, at `priel-tui/src/bus/`, never in the two
libraries.** A dependency in `priel-core` or `priel-player` is a dependency for
every future frontend, and a GUI frontend built on iced or libcosmic will
already have an executor and should use zbus rather than this. If a second
frontend ever wants the wire layer, it can be lifted into a crate then; it is
not one now.

**The bus is a fifth long-lived thread, and only when there is a bus.** It
blocks on `read`, so it cannot live on the render thread - that is the same rule
that put HTTP on the worker thread. It is spawned only after a session address
is found, authenticated and a name acquired, so on the media-server box the
thread does not exist. It reads and writes one socket, holds no lock the audio
path wants, and does no work between messages.

**The socket carries a read timeout rather than a second thread or a self-pipe.**
Outbound `PropertiesChanged` originates on the UI thread and inbound method
calls arrive on the socket; a plain blocking read cannot see both. A ~100 ms
read timeout lets one thread alternate between them, at the tick rate the player
already runs at. The read buffer accumulates across timeouts and only complete
frames are parsed, so a timeout landing mid-message is not a special case.

**The bus may only expose actions that already have a key binding and a click
target.** MPRIS is a remote control, not a third interaction surface, and it must
not become the back door through which an action arrives with no way to reach it
from the terminal. Every member priel implements maps onto a method the keyboard
and the mouse already call - `Next` is what `n` and the header control do - so
there is one implementation of each action and three callers.

**Absolute where the spec is absolute.** `Play`, `Pause` and `Shuffle` are
absolute in MPRIS and priel has only toggles, so `Player::set_paused(bool)` and
`App::set_shuffle(bool)` are added and the existing toggles become calls to
them. Answering `Play` with a toggle would pause a playing track when a media
key is pressed twice, which is exactly the bug a panel applet produces.

**`SetPosition` is ignored when its track id is stale**, which is the entire
reason it takes one. Seeking is otherwise not symmetric and the difference is
easy to miss: `Seek` clamps a negative overshoot to 0 and treats a positive one
as `Next`, while `SetPosition` does *nothing* when the position is out of range.
Both are microseconds, as are `mpris:length`, `Position` and `Seeked`.

**Missing pieces degrade, they do not fail.** No `DBUS_SESSION_BUS_ADDRESS`, a
socket that will not connect, a refused handshake, or a name already owned
leaves priel a working player that is not on the bus, with the reason in the
diagnostic log. A name collision is retried once as
`org.mpris.MediaPlayer2.priel.instance<pid>`, which the spec provides for, before
giving up. The suffix carries no further dot, because playerctl derives the
player name by splitting on the first one: `playerctl -p priel` has to match
both spellings.

## Consequences

**This is roughly 1,500 lines of production code and a similar volume of tests,
owned forever.** That is not a small module; it is comparable to `backend_mpv.rs`.
It is accepted because the thing being implemented is frozen. Owned code against
a specification that has not moved in twenty years accrues almost no maintenance,
which is not true of owned code against a moving service.

**The subset is smaller than the specification, but not where it looks.** The
read side is genuinely trivial: priel is a server, so the only bodies it ever
parses are `s`, `ss`, `ssv`, `x`, `ox` and the empty one, plus the fixed header.
The write side is not: `Properties.GetAll` returns `a{sv}` and `Metadata` is
`a{sv}` containing `as`, so arrays, dict entries, variants and structs are all
required. Dropping `n`, `q`, `t` and `h` from the type system saves four leaf
writers and nothing structural. Anyone scoping this on "only the types MPRIS
needs" will find that clause buys less than it promises.

**The container padding rules are where this goes wrong, and they fail silently.**
An array's length excludes the alignment padding that follows it; an empty array
of an 8-aligned element still needs that padding; each `(yv)` header field starts
on an 8-byte boundary; a variant and a signature align to 1 but their contents do
not; and every offset is computed from byte 0 of the *message*, not of whichever
buffer it is being built in. The specification's answer to a malformed message is
to drop the connection without notice, so a padding bug presents as the bus
hanging up with no diagnostic at all. The mitigation is golden byte vectors
captured from a real bus, compared against in unit tests.

**`Position` is never emitted in `PropertiesChanged`** - the spec says so
explicitly - so a panel applet's moving progress bar is its own extrapolation
between polls. Emitting it at the player's tick rate would make priel a bus
traffic hog and is the standard version of this bug. `Seeked` is what a jump
announces.

**Gapless makes the transition announcement one signal, not two.** priel advances
inside mpv's playlist without stopping, so `Metadata` changes while
`PlaybackStatus` stays `Playing`. Both go in a single `PropertiesChanged` for the
tick that adopts the new track; two signals let a consumer render the old title
against the new position.

**What consumers need is smaller than the specification, but the specification
marks nothing on the player interface as optional.** Chromium, sitting on this
machine's bus, publishes no `LoopStatus`, no `Shuffle`, no `Fullscreen`, no
`CanSetFullscreen` and no `DesktopEntry`, and answers `Introspect` with an empty
`<node></node>` - and the desktop's media applet renders it correctly. So a
reduced set works, but it is a judgement about consumers rather than a licence
the spec grants, and the properties that look most skippable are the ones that
make priel invisible:

- **`CanPlay` false or absent hides priel from GNOME Shell entirely.** Its
  player list is filtered on exactly that property. It is true whenever there is
  a current track, and it does not track paused versus playing.
- **An empty `Identity` makes Plasma discard the player** as non-compliant.
- **`DesktopEntry` is what supplies the icon and the app name**; without it
  GNOME falls back to `Identity` and shows no icon.
- **`CanGoNext` and `CanGoPrevious` are read directly to enable the skip
  buttons**, and a missing property reads as false.
- **`Rate`, `MinimumRate` and `MaximumRate` are all 1.0**, always. priel does
  not vary rate, and a `Rate` of 0.0 freezes a consumer's extrapolated progress
  bar.

**`org.freedesktop.DBus.Properties` is the actual wire contract; the MPRIS
methods are the easy part.** GNOME Shell, Plasma, playerctl and waybar all read
state through `GetAll` plus `PropertiesChanged`, and Plasma issues one `GetAll`
per interface and drops the player if either errors. `Position` must additionally
answer a plain `Get`, which is how Plasma's seek bar and playerctl read it.
`PropertiesChanged` must carry values in `changed_properties`; Plasma treats a
non-empty `invalidated_properties` as a defect and re-syncs.

**No consumer calls `Introspect`.** All four use generated or hardcoded interface
definitions. It is implemented anyway, as twenty lines of constant, because
`busctl`, `gdbus` and `d-feet` are how this will be debugged.

**`xesam:artist` is `as`, not `s`.** GNOME type-checks it, logs a fault and shows
"Unknown artist" for a bare string. The same applies to `xesam:albumArtist`.

**`mpris:trackid` is an object path, not a number, and it identifies a queue
entry rather than a track.** The key is D-Bus type `o`, so a `u64` track id is
spelled `/priel/track/<id>` - not under `/org/mpris`, which the spec reserves,
and with every character outside `[A-Za-z0-9_]` excluded, which a numeric id
gives for free. Sending a string here is the Spotify bug that segfaults
playerctl. It is minted per *queue entry*, because Plasma treats a change of
trackid as the signal to reset its position: the same track twice in the queue
needs two ids, and one play of one entry must keep one id throughout.

**`priel_core::Track` carries no cover art**, so `mpris:artUrl` is either omitted
- which costs a placeholder in the applet - or a field is added to the library and
populated from the listing. That is a `priel-core` change and therefore a separate
decision from this one.

**Both feature configurations stay green by construction.** The bus module uses
`Player`'s public API and nothing from libmpv, so it builds under
`--no-default-features`; the new `set_paused` lands in `backend_mpv.rs` and
`backend_stub.rs` together, which is the standing rule for any player API.

**Everything but the socket is testable without a bus.** Address parsing,
marshalling, unmarshalling and dispatch are pure functions over bytes. The
connection is written against `Read + Write` so an in-memory duplex replays a
whole session, the way `Client::with_base_url` and `EventSource` already work.
The state translation - `PlaybackStatus` plus the current track to the published
property set - is a pure function from a snapshot, in the shape `App::decide`
already established, so the interop rules above are a table of tests rather than
comments pleading with the reader.

**What would change this decision.** If MPRIS needs to ship in weeks rather than
months, or if `TrackList` or `Playlists` ever comes into scope - both of which
need a general implementation rather than a closed set of shapes - then rustbus
is the better trade, and its dormancy is a vendorable problem rather than an
architectural one. And if the no-async-runtime rule is ever relaxed for a reason
of its own, zbus wins outright on volume of code. Neither is true today.
