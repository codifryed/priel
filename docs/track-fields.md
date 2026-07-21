# What a track row and a playback answer actually carry

The response structs in `priel-core` declare only the fields in use, and serde
discards the rest without a word. That is cheap and it is also why nobody knows
what is on the wire without going and looking. Three times now the answer has
mattered and been different each time:

- **#6** found the listings carrying a `totalNumberOfItems` that nothing read,
  which turned a short-page guess into an authoritative end-of-list signal.
- **#16** found the opposite: there is no favourite state on a track at all, so
  priel keeps its own record and a favourited track met in search wears a hollow
  heart until its listing page loads.
- **#17** found a mix carries no track count and no duration, where a playlist
  carries both.

So "discarded" and "absent" are both real answers and they lead to opposite
work. This is the inventory, so that the next person reads it instead of
establishing it a fourth time.

## What this was established against

Reference clients' **parsing and request code**, plus real captured responses.
Prose documentation was used only to corroborate.

| Source | Language | What it is worth |
| --- | --- | --- |
| [`tamland/python-tidal`](https://github.com/tamland/python-tidal) `tidalapi/media.py`, `mix.py`, `user.py` | Python | The most actively maintained client. Distinguishes required (`json_obj["x"]`) from optional (`.get("x")`) key by key, which is a statement about what is always present. |
| [`nathom/streamrip`](https://github.com/nathom/streamrip) `metadata/track.py`, `client/tidal.py` | Python | Independently authored. Runs against album and playlist listings, so what it requires there is evidence about listing rows specifically. |
| [`yaronzz/Tidal-Media-Downloader`](https://github.com/yaronzz/Tidal-Media-Downloader) `tidal_dl/model.py` | Python | Hand-declared dataclass per object. Dormant since ~2022, useful mainly as a lower bound and for spotting what is newer than it. |
| [`jackfagner/OpenTidl`](https://github.com/jackfagner/OpenTidl) `Models/TrackModel.cs` | C# | Statically typed, so every expected key is declared with its type. Old, which is why the fields it lacks are informative. |
| [`bbye98/minim`](https://github.com/bbye98/minim) `src/minim/tidal.py` | Python | Carries a transcribed "Sample response" block per endpoint. |
| [`binimum/hifi-api`](https://github.com/binimum/hifi-api) `README.md`, `main.py` | Python | **Real captured bodies**, verbatim, for a single track, a search page, a playlist, a mix and two playback answers. The signed URLs inside them expire late 2025 / early 2026, so the captures are recent. `main.py` names the endpoint each capture came from. |

Where a capture and a client's code disagreed, the capture won.

## The finding that settles most of the rest

**Listing rows are not abbreviated.** A track inside a search page carries
character-for-character the same field set as `/v1/tracks/{id}` returns for that
track on its own - thirty-odd keys, not the six a row needs. The captured search
page in `hifi-api`'s README shows `replayGain`, `peak`, `bpm`, `key`, `keyScale`,
`isrc`, `copyright`, `popularity`, `url` and the rest sitting on an ordinary
`items[]` row, and the captured playlist page shows the same set under the
`items[].item` wrapper.

That is corroborated from the other direction by `python-tidal`, which runs one
`parse_track` over listing rows and single-track fetches alike and indexes
`explicit`, `allowStreaming`, `streamReady`, `trackNumber`, `volumeNumber` and
`popularity` with `[]` rather than `.get()`. Were those absent from listing rows
it would raise on every favourites page.

So for a track, almost nothing is genuinely absent. Nearly everything not in
`Track` was **discarded here**, and can be had by asking for it.

**One exception: a mix's rows are a shorter shape.** The captured
`/v1/pages/mix` answer - the same endpoint `Client::mix_tracks` calls - has rows
carrying `replayGain` but **no** `peak`, `isrc`, `copyright`, `bpm`, `key`,
`keyScale` or `premiumStreamingOnly`, and one key the other listings do not send
at all, `doublePopularity`. Anything read off a track has to tolerate absence on
that path.

## The track row

`/v1/users/{id}/favorites/tracks`, `/v1/playlists/{uuid}/tracks`, `/v1/search`
and `/v1/pages/mix`. Status is one of **read** (reaches `Track`), **discarded**
(on the wire, thrown away here) or **absent** (not sent).

| Wire key | Type | Status | Note |
| --- | --- | --- | --- |
| `id` | int | read | `Track::id` |
| `title` | str | read | `Track::title` |
| `duration` | int | read | `Track::duration_secs`; seconds |
| `artists[].name` | list | read | all of them, `Track::artists`; `artist` is the first |
| `artist.name` | obj | discarded | the primary, duplicated from `artists[0]` in every capture |
| `artists[].id`, `.type`, `.picture`, `.handle` | | discarded | ids for navigation, art a terminal cannot draw |
| `album.title` | str | read | `Track::album` |
| `album.id` | int | discarded | wanted the day there is an album view |
| `album.cover`, `.vibrantColor`, `.videoCover` | | discarded | art and a theme colour |
| `audioQuality` | str | read | via `quality_label` |
| `mediaMetadata.tags` | list | read | via `quality_label`; the hi-res tag wins |
| `version` | str/null | **read (new)** | `Track::version` |
| `explicit` | bool | **read (new)** | `Track::explicit` |
| `isrc` | str | **read (new)** | `Track::isrc`; absent on mix rows |
| `copyright` | str | **read (new)** | `Track::copyright`; absent on mix rows |
| `allowStreaming` | bool | **read (new)** | half of `Track::streamable` |
| `streamReady` | bool | **read (new)** | the other half |
| `trackNumber` | int | discarded | ordering within an album; no album view yet |
| `volumeNumber` | int | discarded | as above |
| `popularity` | int | discarded | 0..100 |
| `doublePopularity` | float | discarded | mix rows only |
| `replayGain` | float | discarded | see the playback answer, which carries it too |
| `peak` | float | discarded | as above |
| `bpm` | int | discarded | not on mix rows |
| `key`, `keyScale` | str | discarded | e.g. `"F"`, `"MAJOR"`; not on mix rows |
| `url` | str | discarded | a share link |
| `streamStartDate` | str | discarded | ISO 8601; when it reached the service |
| `dateAdded` | str | discarded | only on favourites and playlist rows |
| `premiumStreamingOnly` | bool | discarded | not on mix rows |
| `adSupportedStreamReady`, `djReady`, `stemReady` | bool | discarded | other clients' features |
| `payToStream`, `upload`, `spotlighted`, `editable` | bool | discarded | |
| `accessType` | str/null | discarded | `"PUBLIC"` or null |
| `audioModes` | list | discarded | `["STEREO"]`, or Atmos / 360 |
| `mixes.TRACK_MIX` | str | discarded | the id of this track's own radio mix |
| `type` | str | discarded on purpose | `Track` or `Video`; see `ItemRow`'s note - dropping video rows would desynchronise the caller's offset from the service's |
| **a favourite flag** | | **absent** | see below |

### What is genuinely absent from a track

**Nothing on a track row says whether it is in the listener's favourites.** Not
under that name or any other, in any capture or any client. `python-tidal`
offers no per-track check either: `Favorites` has `add_track` and
`remove_track` and no query, so the only way to know is to page the favourites
listing and compare ids. That is exactly what #16 found and why
`App::favorite_ids` exists, and it is confirmed here rather than re-derived.

## The playback answer

`/v1/tracks/{id}/playbackinfopostpaywall`.

| Wire key | Type | Status | Note |
| --- | --- | --- | --- |
| `manifest` | str | read | base64; the whole point of the call |
| `manifestMimeType` | str | read | selects the BTS or the DASH path |
| `audioQuality` | str | read | `ResolvedStream::quality` |
| `bitDepth` | int | read | absent on some tiers, hence `Option` |
| `sampleRate` | int | read | absent likewise; falls back to the MPD's `audioSamplingRate` |
| `trackReplayGain` | float | **read (new)** | `ResolvedStream::replay_gain_db` |
| `trackPeakAmplitude` | float | **read (new)** | `ResolvedStream::peak` |
| `albumReplayGain` | float | discarded | no album context to spend it in |
| `albumPeakAmplitude` | float | discarded | as above |
| `manifestHash` | str | discarded | a cache key for a cache priel does not keep |
| `trackId` | int | discarded | the caller passed it in |
| `assetPresentation` | str | discarded | priel always asks `FULL` |
| `audioMode` | str | discarded | `STEREO`; see the note on `audioModes` below |
| `streamingSessionId` | | absent unless sent | a request parameter, echoed only if supplied |

Inside the decoded **BTS** manifest, `codecs` and `urls` are read;
`mimeType`, `encryptionType` and `keyId` are discarded (the captures all show
`"encryptionType": "NONE"`, and streamrip reads a `restrictions` key that no
capture here contains). Inside the decoded **MPD**, `codecs`,
`audioSamplingRate` and the segment template are read; `bandwidth`,
`mediaPresentationDuration` and the `<Label>FLAC_HIRES</Label>` element are not.

## Already known here and simply not surfaced

Not library work, but it belongs on the same list so the display half is not
told to go and fetch what it already has:

- **`ResolvedStream::sample_rate` and `bit_depth`** reach `App::now_meta` on
  every resolve and from there only the badges. The figures are exact and
  per-track and nothing else shows them.
- **`Track::quality`** is derived and stored on every row in every listing, and
  only the row's own tier badge reads it.
- **`Page::total`** is now carried on every listing and is read only for paging.
  It is also the honest answer to "how long is this list", which nothing shows.
- **`Mix::subtitle`** is parsed and carried per #17 and is the nearest thing to a
  description a mix has.

## Where the sources disagreed

1. **Whether the playback answer carries `bitDepth` and `sampleRate` at all.**
   `minim`'s transcribed sample response lists neither, and
   `Tidal-Media-Downloader`'s `StreamRespond` declares neither. `python-tidal`
   reads both, with the comment *"Bit depth, Sample rate not available for
   low,hi_res quality modes"*. The captures settle it: both are present, on both
   the lossless BTS answer (`16` / `44100`) and the hi-res DASH answer (`24` /
   `44100`). Followed `python-tidal` and the captures - and note that priel's
   existing `Option<u32>` plus the fall back to the MPD's own
   `audioSamplingRate` was already the correct shape for a field that is
   sometimes missing. `Tidal-Media-Downloader` predates the fields; `minim`'s
   sample is simply incomplete.

2. **Whether `isrc` is always present on a track.** `streamrip` requires it
   (`typed(track["isrc"], str)`) while `python-tidal` reads it with `.get()` and
   only when the track is streamable. The mix capture settles it: `isrc` is
   **not** on a mix's rows. Followed `python-tidal`, and `Track::isrc` is empty
   rather than absent on that path. `streamrip` gets away with it because it
   never reads a mix.

3. **`explicit`.** Required by `python-tidal` (`bool(json_obj["explicit"])`),
   optional-with-default by `streamrip` (`track.get("explicit", False)`), and
   absent entirely from `OpenTidl`'s C# model. Present in every capture,
   including the mix rows. Treated as optional defaulting to false: the cost of
   being wrong is a missing marker, not a failed listing.

4. **`copyright`'s spelling.** `Tidal-Media-Downloader` declares `copyRight`;
   every capture and every other client says `copyright`. That model's field
   mapper is case-insensitive, which is how the typo survives unnoticed.
   Followed the captures.

## What was exposed, and what was deliberately left out

`Mix` is the precedent: three fields, because three were what existed and were
useful, and images and colours were left out because a terminal cannot draw them
and a second frontend can have them added when it has a use - which is cheaper
than un-exporting them later. The same test was applied here.

Added to `Track`: `version`, `explicit`, `isrc`, `copyright`, `artists`,
`streamable`. Each answers something a listener can read off a screen made of
characters, and `artists` closes an outright loss - a collaboration credits
several names and this crate kept one, so the rest were unrecoverable above it.

Added to `ResolvedStream`: `replay_gain_db` and `peak`, reported and never
applied. Scaling the samples is the one thing a bit-perfect path may not do, so
these are figures about the master, not instructions to the player.

Left out on purpose, all of them one line away the day something wants them:

- **`trackNumber` / `volumeNumber`** - ordering within an album, and there is no
  album view to order.
- **`popularity`** - a 0..100 the interface has nothing to say about.
- **`audioModes` / `audioMode`** - priel asks for `HI_RES_LOSSLESS` or
  `LOSSLESS`, which are stereo FLAC. An Atmos badge would advertise something
  this player does not play.
- **`bpm`, `key`, `keyScale`** - real and interesting, absent on mix rows, and a
  feature nobody asked for.
- **`albumReplayGain` / `albumPeakAmplitude`** - no album context to spend them
  in.
- **`album.id`, `artists[].id`** - navigation keys, wanted by the browse work
  and not by this.
- **art, cover ids, `vibrantColor`, share `url`s** - a terminal cannot use them.

## A trap worth keeping

`#[serde(default)]` fills in a **missing** key and rejects an explicit `null`.
The service sends `"version": null` on the majority of tracks - it is how "no
version" is spelled, not an omission - and `"accessType": null` freely besides.
A `String` field with `#[serde(default)]` therefore fails to deserialise the
commonest row in the catalogue, taking the whole listing with it. Every string
on `TrackBrief` is `Option<String>` for that reason, and
`a_null_where_a_string_was_expected_does_not_fail_the_page` is the regression
test. Do not simplify them back.
