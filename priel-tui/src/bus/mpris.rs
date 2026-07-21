// SPDX-License-Identifier: GPL-3.0-or-later
//
// priel — hi-res terminal client for TIDAL
// Copyright (C) 2026 Guy Boldon
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! What the desktop is told, and what it may ask for.
//!
//! A pure translation from a snapshot of playback to the property set the two
//! MPRIS interfaces publish, in the shape `App::decide` established: the rules
//! below were measured against real consumers rather than read off the
//! specification, and every one of them is a test rather than a comment
//! pleading with the reader.
//!
//! This module holds no state, opens nothing and decides no policy about the
//! queue. It reads a [`Snapshot`] and writes messages.
//!
//! Specifications: MPRIS D-Bus Interface Specification 2.2, D-Bus 0.43.

// The translation lands ahead of the connection that reads it and the app that
// fills it in, so that the interop rules can be pinned as a table of tests
// before anything depends on them.
#![allow(
    dead_code,
    reason = "the connection below it and the app above it are the callers, and land next"
)]

use super::wire::{Arg, Message, Value, Variant};

/// The well-known name priel asks the session bus for.
///
/// playerctl derives a player's name by splitting on the first dot, so this is
/// what `playerctl -p priel` matches - and why the per-instance fallback in
/// [`super::conn`] adds no further one.
pub(crate) const WELL_KNOWN_NAME: &str = "org.mpris.MediaPlayer2.priel";

/// The one object an MPRIS player publishes at. Spec 2.2 fixes it, and a
/// consumer looks nowhere else.
pub(crate) const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

pub(crate) const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
pub(crate) const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// The interface every consumer actually reads state through. All four major
/// ones use `GetAll` plus `PropertiesChanged` and none of them calls
/// `Introspect`.
pub(crate) const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

/// What priel calls itself on the desktop.
///
/// **Never empty.** Plasma discards a player with an empty `Identity` as
/// non-compliant, which is a silent disappearance rather than an error.
const IDENTITY: &str = "priel";

/// The desktop file, without its extension, that supplies priel's icon and the
/// name shown beside it. Without this GNOME falls back to [`IDENTITY`] and
/// shows no icon at all.
const DESKTOP_ENTRY: &str = "priel";

/// The property that moves on its own, and the one spec 2.2 forbids announcing.
///
/// Named here only so a test can say so: [`announced`] cannot emit it, because
/// it is not given the position in the first place.
pub(crate) const POSITION: &str = "Position";

/// How far a position may move between two ticks before it is a seek rather
/// than playback.
///
/// The app refreshes about ten times a second, so a whole second and a half of
/// slack costs nothing and covers a tick that was held up by a redraw.
const SEEK_JUMP_US: i64 = 1_500_000;

/// The queue entry playing now.
///
/// A *queue entry* and not a track: the same track twice in a queue is two of
/// these, because Plasma treats a change of `mpris:trackid` as the signal to
/// reset its position.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Entry {
    /// The object path this play of this entry is known by. See [`track_path`].
    pub(crate) path: String,
    pub(crate) title: String,
    /// The track's artist, or empty when the listing carried none.
    pub(crate) artist: String,
    pub(crate) album: String,
    pub(crate) length_us: i64,
}

/// Everything the desktop is told about when it changes.
///
/// **The position is deliberately not here.** It moves every tick and spec 2.2
/// forbids announcing it, so it lives beside this in [`Snapshot`] where a change
/// to it cannot reach the property diff. That is the interop rule as a type
/// rather than as a filter someone can delete.
#[derive(Debug, Clone, Default, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the capability flags spec 2.2 publishes; a consumer reads each one on its own"
)]
pub(crate) struct Now {
    /// The entry playing now, or `None` for a player that is stopped.
    pub(crate) track: Option<Entry>,
    pub(crate) paused: bool,
    pub(crate) shuffle: bool,
    /// The software volume as a fraction of unity, which is the scale MPRIS
    /// carries. priel's own is a percentage.
    pub(crate) volume: f64,
    pub(crate) can_go_next: bool,
    pub(crate) can_go_previous: bool,
    pub(crate) can_seek: bool,
}

/// What the bus thread answers a call from.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) now: Now,
    /// How far into the current track, in microseconds. Answered on demand and
    /// never announced.
    pub(crate) position_us: i64,
}

/// The object path one play of one queue entry is known by.
///
/// Spec 2.2 makes `mpris:trackid` type `o`, and sending a string there is the
/// bug that segfaults playerctl. It counts *plays* rather than tracks, because
/// a consumer resets its position when this changes: the same track twice in a
/// queue needs two, and one play of one entry must keep one throughout. Not
/// under `/org/mpris`, which the specification reserves for itself.
pub(crate) fn track_path(play: u64) -> String {
    format!("/priel/track/{play}")
}

/// The properties of `org.mpris.MediaPlayer2`.
///
/// None of them depends on what is playing, so there is nothing to announce and
/// no snapshot to read.
pub(crate) fn root_properties() -> Vec<(String, Variant)> {
    vec![
        // `q` quits priel, so this is honest. Every member here maps onto
        // something the keyboard and the mouse already reach.
        boolean("CanQuit", true),
        // priel is a window someone else owns. There is no raise action in the
        // terminal, so there is none on the bus either.
        boolean("CanRaise", false),
        boolean("HasTrackList", false),
        text("Identity", IDENTITY),
        text("DesktopEntry", DESKTOP_ENTRY),
        // priel opens nothing it is handed; `OpenUri` is not implemented and an
        // empty list is how a player says so.
        strings("SupportedUriSchemes", &[]),
        strings("SupportedMimeTypes", &[]),
    ]
}

/// The properties of `org.mpris.MediaPlayer2.Player`, including the position.
///
/// This is what `GetAll` answers with. The announced subset is [`announced`],
/// and the difference between the two is exactly [`POSITION`].
pub(crate) fn player_properties(snapshot: &Snapshot) -> Vec<(String, Variant)> {
    let mut properties = announced(&snapshot.now);
    properties.push((
        POSITION.to_owned(),
        Variant::Value(Value::Int64(snapshot.position_us)),
    ));
    properties
}

/// Everything on the player interface that may be announced.
///
/// It takes a [`Now`] and not a [`Snapshot`], which is what makes emitting the
/// position in a `PropertiesChanged` impossible rather than merely forbidden.
fn announced(now: &Now) -> Vec<(String, Variant)> {
    let playing = now.track.is_some();
    vec![
        text("PlaybackStatus", playback_status(now)),
        ("Metadata".to_owned(), metadata(now.track.as_ref())),
        // Always unity, all three. priel does not vary rate, and a `Rate` of 0.0
        // freezes a consumer's extrapolated progress bar.
        double("Rate", 1.0),
        double("MinimumRate", 1.0),
        double("MaximumRate", 1.0),
        double("Volume", now.volume),
        boolean("Shuffle", now.shuffle),
        // Read directly to enable the skip buttons, and a missing property reads
        // as false.
        boolean("CanGoNext", now.can_go_next),
        boolean("CanGoPrevious", now.can_go_previous),
        // **False or absent hides priel from GNOME Shell entirely**: its player
        // list is filtered on exactly this. It says there is something to play,
        // not that it is playing, so it does not follow the pause state.
        boolean("CanPlay", playing),
        boolean("CanPause", playing),
        boolean("CanSeek", now.can_seek),
        // priel accepts every control it publishes. Spec 2.2 does not expect
        // this one to change.
        boolean("CanControl", true),
    ]
}

/// One of the three words spec 2.2 allows.
fn playback_status(now: &Now) -> &'static str {
    if now.track.is_none() {
        "Stopped"
    } else if now.paused {
        "Paused"
    } else {
        "Playing"
    }
}

/// `Metadata`, which is `a{sv}` and the one container that goes inside a
/// variant.
///
/// **No `mpris:artUrl`.** `priel_core::Track` carries no cover art, and an
/// invented one is worse than the placeholder an applet draws without it.
fn metadata(track: Option<&Entry>) -> Variant {
    let Some(track) = track else {
        // A stopped player describes no track. An entry with empty fields would
        // put a blank title on the lock screen instead of nothing at all.
        return Variant::Dict(Vec::new());
    };
    let mut fields = vec![
        ("mpris:trackid".to_owned(), Value::Path(track.path.clone())),
        (
            "mpris:length".to_owned(),
            Value::Int64(track.length_us.max(0)),
        ),
        ("xesam:title".to_owned(), Value::Str(track.title.clone())),
    ];
    // **`as`, never `s`.** GNOME type-checks this one, logs a fault and shows
    // "Unknown artist" for a bare string. Left out entirely when the listing
    // carried none, which is the same thing said honestly.
    if !track.artist.is_empty() {
        fields.push((
            "xesam:artist".to_owned(),
            Value::Strings(vec![track.artist.clone()]),
        ));
    }
    if !track.album.is_empty() {
        fields.push(("xesam:album".to_owned(), Value::Str(track.album.clone())));
    }
    Variant::Dict(fields)
}

/// What this tick has to announce, or nothing.
///
/// A gapless transition changes `Metadata` while `PlaybackStatus` stays
/// `Playing`, and both belong in the *one* signal this returns: two signals let
/// a consumer render the old title against the new position.
pub(crate) fn changed(previous: &Now, current: &Now) -> Vec<(String, Variant)> {
    // The common tick. Nothing moved but the position, which is not announced,
    // so nothing is built either.
    if previous == current {
        return Vec::new();
    }
    let before = announced(previous);
    announced(current)
        .into_iter()
        .filter(|(name, value)| {
            before
                .iter()
                .find(|(known, _)| known == name)
                .is_none_or(|(_, was)| was != value)
        })
        .collect()
}

/// The position moved further than playing could account for.
///
/// A change of track is not a seek: it announces itself through `Metadata`, and
/// reporting it as one would make a consumer scrub a bar it is about to reset.
/// So a jump is only ever read within one play of one entry.
pub(crate) fn seeked(previous: &Snapshot, current: &Snapshot) -> Option<i64> {
    let (before, after) = (previous.now.track.as_ref()?, current.now.track.as_ref()?);
    if before.path != after.path {
        return None;
    }
    let jump = current
        .position_us
        .saturating_sub(previous.position_us)
        .saturating_abs();
    (jump >= SEEK_JUMP_US).then_some(current.position_us)
}

/// The signal that carries a property change.
///
/// The values go in `changed_properties`; `invalidated_properties` is always
/// empty, because Plasma treats a non-empty one as a defect and re-syncs the
/// whole player.
pub(crate) fn properties_changed(interface: &str, changed: Vec<(String, Variant)>) -> Message {
    Message::signal(0, OBJECT_PATH, PROPERTIES_INTERFACE, "PropertiesChanged").with_body(vec![
        Arg::Value(Value::Str(interface.to_owned())),
        Arg::Dict(changed),
        Arg::Value(Value::Strings(Vec::new())),
    ])
}

/// What a jump announces, since the position itself may not be.
pub(crate) fn seeked_signal(position_us: i64) -> Message {
    Message::signal(0, OBJECT_PATH, PLAYER_INTERFACE, "Seeked")
        .with_body(vec![Arg::Value(Value::Int64(position_us))])
}

fn boolean(name: &str, value: bool) -> (String, Variant) {
    (name.to_owned(), Variant::Value(Value::Bool(value)))
}

fn text(name: &str, value: &str) -> (String, Variant) {
    (
        name.to_owned(),
        Variant::Value(Value::Str(value.to_owned())),
    )
}

fn double(name: &str, value: f64) -> (String, Variant) {
    (name.to_owned(), Variant::Value(Value::Double(value)))
}

fn strings(name: &str, values: &[&str]) -> (String, Variant) {
    (
        name.to_owned(),
        Variant::Value(Value::Strings(
            values.iter().map(|value| (*value).to_owned()).collect(),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One property out of a set, by name.
    fn get<'a>(properties: &'a [(String, Variant)], name: &str) -> Option<&'a Variant> {
        properties
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value)
    }

    fn names(properties: &[(String, Variant)]) -> Vec<&str> {
        properties.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn boolean_at(properties: &[(String, Variant)], name: &str) -> Option<bool> {
        match get(properties, name) {
            Some(Variant::Value(Value::Bool(value))) => Some(*value),
            _ => None,
        }
    }

    fn entry(play: u64, title: &str) -> Entry {
        Entry {
            path: track_path(play),
            title: title.to_owned(),
            artist: "An Artist".to_owned(),
            album: "An Album".to_owned(),
            length_us: 300_000_000,
        }
    }

    /// A player mid-track, which tests override a field of.
    fn playing() -> Snapshot {
        Snapshot {
            now: Now {
                track: Some(entry(1, "A Title")),
                paused: false,
                shuffle: false,
                volume: 1.0,
                can_go_next: true,
                can_go_previous: true,
                can_seek: true,
            },
            position_us: 42_000_000,
        }
    }

    /// Goal: an empty `Identity` makes Plasma discard the player as
    /// non-compliant, and a missing `DesktopEntry` costs the icon and the app
    /// name in GNOME. Both are silent failures. Method: read them back.
    #[test]
    fn the_identity_is_never_empty_and_the_icon_is_named() {
        let root = root_properties();
        assert_eq!(
            get(&root, "Identity"),
            Some(&Variant::Value(Value::Str("priel".into())))
        );
        assert_eq!(
            get(&root, "DesktopEntry"),
            Some(&Variant::Value(Value::Str("priel".into())))
        );
    }

    /// Goal: the root interface promises only what the terminal can already do.
    /// `q` quits, and nothing raises a window priel does not own. Method: the
    /// three capability flags, plus the two empty lists that say `OpenUri` is
    /// not implemented.
    #[test]
    fn the_root_promises_only_what_a_key_already_does() {
        let root = root_properties();
        assert_eq!(boolean_at(&root, "CanQuit"), Some(true));
        assert_eq!(boolean_at(&root, "CanRaise"), Some(false));
        assert_eq!(
            boolean_at(&root, "HasTrackList"),
            Some(false),
            "TrackList is permanently out of scope"
        );
        assert_eq!(
            get(&root, "SupportedUriSchemes"),
            Some(&Variant::Value(Value::Strings(Vec::new())))
        );
        assert_eq!(
            get(&root, "SupportedMimeTypes"),
            Some(&Variant::Value(Value::Strings(Vec::new())))
        );
    }

    /// Goal: **`CanPlay` false or absent hides priel from GNOME Shell
    /// entirely** - its player list is filtered on exactly that property, so
    /// getting this wrong is a disappearance rather than a broken button. It
    /// says there is something to play, not that it is playing. Method: a
    /// playing player, a paused one, and a stopped one.
    #[test]
    fn a_player_with_a_track_says_it_can_play_however_it_is_playing_it() {
        let mut snapshot = playing();
        assert_eq!(
            boolean_at(&player_properties(&snapshot), "CanPlay"),
            Some(true)
        );

        snapshot.now.paused = true;
        assert_eq!(
            boolean_at(&player_properties(&snapshot), "CanPlay"),
            Some(true),
            "a paused track is still a track"
        );

        snapshot.now.track = None;
        assert_eq!(
            boolean_at(&player_properties(&snapshot), "CanPlay"),
            Some(false)
        );
    }

    /// Goal: the skip buttons are enabled straight off `CanGoNext` and
    /// `CanGoPrevious`, and a missing property reads as false. Method: both
    /// states of each, read back as published.
    #[test]
    fn the_skip_buttons_are_enabled_from_what_the_queue_can_do() {
        let mut snapshot = playing();
        snapshot.now.can_go_next = false;
        snapshot.now.can_go_previous = true;
        let properties = player_properties(&snapshot);
        assert_eq!(boolean_at(&properties, "CanGoNext"), Some(false));
        assert_eq!(boolean_at(&properties, "CanGoPrevious"), Some(true));

        snapshot.now.can_go_next = true;
        snapshot.now.can_go_previous = false;
        let properties = player_properties(&snapshot);
        assert_eq!(boolean_at(&properties, "CanGoNext"), Some(true));
        assert_eq!(boolean_at(&properties, "CanGoPrevious"), Some(false));
    }

    /// Goal: a seek bar is offered only where seeking works. mpv reports no
    /// duration until a track is decoding, and a bar over an unknown length is
    /// a control that does nothing. Method: both states.
    #[test]
    fn a_track_of_unknown_length_says_it_cannot_be_seeked() {
        let mut snapshot = playing();
        assert_eq!(
            boolean_at(&player_properties(&snapshot), "CanSeek"),
            Some(true)
        );
        snapshot.now.can_seek = false;
        assert_eq!(
            boolean_at(&player_properties(&snapshot), "CanSeek"),
            Some(false)
        );
    }

    /// Goal: all three rates are 1.0, always. A `Rate` of 0.0 freezes a
    /// consumer's extrapolated progress bar, which reads as a hung player.
    /// Method: read all three off a player that is not playing either, since
    /// they do not depend on it.
    #[test]
    fn every_rate_is_unity_whatever_is_playing() {
        for snapshot in [playing(), Snapshot::default()] {
            let properties = player_properties(&snapshot);
            for name in ["Rate", "MinimumRate", "MaximumRate"] {
                assert_eq!(
                    get(&properties, name),
                    Some(&Variant::Value(Value::Double(1.0))),
                    "{name} must never be 0.0"
                );
            }
        }
    }

    /// Goal: `xesam:artist` is `as` and not `s`. GNOME type-checks it, logs a
    /// fault and shows "Unknown artist" for a bare string. Method: read the
    /// metadata dict out of the variant it is nested in.
    #[test]
    fn the_artists_are_a_list_and_never_a_bare_string() {
        let properties = player_properties(&playing());
        let Some(Variant::Dict(fields)) = get(&properties, "Metadata") else {
            panic!("Metadata is a dict inside a variant");
        };
        let artist = fields
            .iter()
            .find(|(key, _)| key == "xesam:artist")
            .map(|(_, value)| value);
        assert_eq!(
            artist,
            Some(&Value::Strings(vec!["An Artist".into()])),
            "a bare string here is the Spotify bug"
        );
    }

    /// Goal: `mpris:trackid` is an object path, and it counts plays rather than
    /// tracks - a consumer resets its position when it changes, so the same
    /// track twice in a queue must not collide. Method: two plays, and the
    /// shape of the path itself.
    #[test]
    fn a_track_id_is_an_object_path_that_counts_plays() {
        let mut snapshot = playing();
        let read = |snapshot: &Snapshot| {
            let properties = player_properties(snapshot);
            let Some(Variant::Dict(fields)) = get(&properties, "Metadata") else {
                panic!("Metadata is a dict");
            };
            fields
                .iter()
                .find(|(key, _)| key == "mpris:trackid")
                .map(|(_, value)| value.clone())
        };
        assert_eq!(read(&snapshot), Some(Value::Path("/priel/track/1".into())));

        // The same track, played a second time. The path must differ, or Plasma
        // keeps the position it had from the first.
        snapshot.now.track = Some(entry(2, "A Title"));
        assert_eq!(read(&snapshot), Some(Value::Path("/priel/track/2".into())));
        assert!(
            !track_path(7).starts_with("/org/mpris"),
            "spec 2.2 reserves that subtree"
        );
    }

    /// Goal: `priel_core::Track` carries no cover art, so no `mpris:artUrl` may
    /// be invented - an applet draws a placeholder, which is honest, where a
    /// made-up URL is a broken image. Method: the whole key set.
    #[test]
    fn no_cover_art_is_invented_and_nothing_empty_is_published() {
        let mut snapshot = playing();
        snapshot.now.track = Some(Entry {
            artist: String::new(),
            album: String::new(),
            ..entry(1, "A Title")
        });
        let properties = player_properties(&snapshot);
        let Some(Variant::Dict(fields)) = get(&properties, "Metadata") else {
            panic!("Metadata is a dict");
        };
        let keys: Vec<&str> = fields.iter().map(|(key, _)| key.as_str()).collect();
        assert!(!keys.contains(&"mpris:artUrl"), "{keys:?}");
        assert!(
            !keys.contains(&"xesam:artist"),
            "a listing with no artist says nothing rather than an empty list"
        );
        assert!(!keys.contains(&"xesam:album"), "{keys:?}");
        assert!(keys.contains(&"xesam:title"));
    }

    /// Goal: a stopped player describes no track at all. An entry of empty
    /// fields would put a blank title on the lock screen. Method: no track.
    #[test]
    fn nothing_playing_carries_no_metadata() {
        let properties = player_properties(&Snapshot::default());
        assert_eq!(
            get(&properties, "Metadata"),
            Some(&Variant::Dict(Vec::new()))
        );
        assert_eq!(
            get(&properties, "PlaybackStatus"),
            Some(&Variant::Value(Value::Str("Stopped".into())))
        );
    }

    /// Goal: the three words spec 2.2 allows, and no fourth. Method: each state
    /// priel can be in.
    #[test]
    fn the_playback_status_is_one_of_the_three_the_specification_names() {
        let mut snapshot = playing();
        assert_eq!(playback_status(&snapshot.now), "Playing");
        snapshot.now.paused = true;
        assert_eq!(playback_status(&snapshot.now), "Paused");
        snapshot.now.track = None;
        assert_eq!(
            playback_status(&snapshot.now),
            "Stopped",
            "no track outranks the pause flag"
        );
    }

    /// Goal: **the position answers a plain `Get` but is never announced.**
    /// Plasma's seek bar and playerctl read it that way, and emitting it at the
    /// player's tick rate would make priel a bus traffic hog - the spec forbids
    /// it outright. Method: the full set carries it; a tick where only it moved
    /// announces nothing.
    #[test]
    fn the_position_answers_a_read_but_is_never_announced() {
        let before = playing();
        let after = Snapshot {
            position_us: before.position_us + 100_000,
            ..before.clone()
        };
        assert_eq!(
            get(&player_properties(&after), POSITION),
            Some(&Variant::Value(Value::Int64(42_100_000))),
            "a plain Get has to be answerable"
        );
        assert!(
            changed(&before.now, &after.now).is_empty(),
            "a tick that only moved the position announces nothing"
        );
    }

    /// Goal: nothing announced may be a property the full set does not carry -
    /// a consumer that took a change for a property `GetAll` never mentioned
    /// would hold a value it can never re-read. Method: change everything at
    /// once and check every name against the published set.
    #[test]
    fn every_property_that_can_change_is_one_the_full_set_carries() {
        let before = Snapshot::default();
        let after = playing();
        let changed = changed(&before.now, &after.now);
        assert!(!changed.is_empty());
        let all = player_properties(&after);
        let published = names(&all);
        for (name, _) in &changed {
            assert!(
                published.contains(&name.as_str()),
                "{name} is not published"
            );
            assert_ne!(name, POSITION, "the position is never announced");
        }
    }

    /// Goal: a gapless transition changes `Metadata` while `PlaybackStatus`
    /// stays `Playing`, and both belong in **one** signal - two let a consumer
    /// render the old title against the new position. Method: advance the track
    /// without stopping and read the whole announcement.
    #[test]
    fn a_gapless_change_of_track_is_one_announcement() {
        let before = playing();
        let after = Snapshot {
            now: Now {
                track: Some(entry(2, "The Next One")),
                ..before.now.clone()
            },
            position_us: 0,
        };
        let changed = changed(&before.now, &after.now);
        assert_eq!(
            names(&changed),
            vec!["Metadata"],
            "the status did not change, so only the metadata is announced"
        );
        assert_eq!(
            get(&player_properties(&after), "PlaybackStatus"),
            Some(&Variant::Value(Value::Str("Playing".into()))),
            "playback never stopped"
        );
    }

    /// Goal: a pause that lands on the same tick as a new track is still one
    /// signal carrying both, because the three answers are independent rather
    /// than a priority chain. Method: change the track and the pause state at
    /// once.
    #[test]
    fn two_things_changing_at_once_are_still_one_announcement() {
        let before = playing();
        let after = Now {
            track: Some(entry(2, "The Next One")),
            paused: true,
            ..before.now.clone()
        };
        let changed = changed(&before.now, &after);
        assert_eq!(names(&changed), vec!["PlaybackStatus", "Metadata"]);
    }

    /// Goal: an unchanged tick costs nothing at all. priel refreshes ten times
    /// a second for hours, and announcing an identical property set at that
    /// rate is the traffic-hog bug in its other form. Method: the same state
    /// twice.
    #[test]
    fn an_unchanged_tick_announces_nothing() {
        let snapshot = playing();
        assert!(changed(&snapshot.now, &snapshot.now).is_empty());
    }

    /// Goal: an announcement carries its values and invalidates nothing -
    /// Plasma treats a non-empty `invalidated_properties` as a defect and
    /// re-syncs the whole player. Method: the signal's own shape.
    #[test]
    fn an_announcement_carries_its_values_and_invalidates_nothing() {
        let signal = properties_changed(PLAYER_INTERFACE, vec![boolean("Shuffle", true)]);
        assert_eq!(signal.path(), Some(OBJECT_PATH));
        assert_eq!(signal.interface(), Some(PROPERTIES_INTERFACE));
        assert_eq!(signal.member(), Some("PropertiesChanged"));
        assert_eq!(signal.signature(), "sa{sv}as");
        assert_eq!(
            signal.body,
            vec![
                Arg::Value(Value::Str(PLAYER_INTERFACE.into())),
                Arg::Dict(vec![boolean("Shuffle", true)]),
                Arg::Value(Value::Strings(Vec::new())),
            ]
        );
    }

    /// Goal: a jump within one track is what `Seeked` announces, and a change
    /// of track is not one - it announces itself through `Metadata`, and
    /// reporting it as a seek makes a consumer scrub a bar it is about to
    /// reset. Method: a jump, ordinary playback, and a transition.
    #[test]
    fn a_jump_within_one_track_is_a_seek_and_a_change_of_track_is_not() {
        let before = playing();
        let jumped = Snapshot {
            position_us: 90_000_000,
            ..before.clone()
        };
        assert_eq!(seeked(&before, &jumped), Some(90_000_000));

        let backwards = Snapshot {
            position_us: 0,
            ..before.clone()
        };
        assert_eq!(seeked(&before, &backwards), Some(0), "a rewind is a seek");

        let ticked = Snapshot {
            position_us: before.position_us + 100_000,
            ..before.clone()
        };
        assert_eq!(seeked(&before, &ticked), None, "playing is not seeking");

        let advanced = Snapshot {
            now: Now {
                track: Some(entry(2, "The Next One")),
                ..before.now.clone()
            },
            position_us: 0,
        };
        assert_eq!(seeked(&before, &advanced), None);
        assert_eq!(seeked(&Snapshot::default(), &before), None);
        assert_eq!(seeked(&before, &Snapshot::default()), None);
    }

    /// Goal: `Seeked` is a signal on the player interface carrying
    /// microseconds, which is the unit every position in MPRIS is in. Method:
    /// its shape.
    #[test]
    fn a_seek_is_announced_in_microseconds() {
        let signal = seeked_signal(90_000_000);
        assert_eq!(signal.path(), Some(OBJECT_PATH));
        assert_eq!(signal.interface(), Some(PLAYER_INTERFACE));
        assert_eq!(signal.member(), Some("Seeked"));
        assert_eq!(signal.signature(), "x");
        assert_eq!(signal.body, vec![Arg::Value(Value::Int64(90_000_000))]);
    }

    /// Goal: the volume is a fraction of unity on the bus, where priel's own is
    /// a percentage, and a length is microseconds where a listing gives
    /// seconds. Getting either scale wrong is silent. Method: read both back.
    #[test]
    fn the_scales_are_the_ones_the_specification_uses() {
        let properties = player_properties(&playing());
        assert_eq!(
            get(&properties, "Volume"),
            Some(&Variant::Value(Value::Double(1.0))),
            "unity is 1.0 and not 100.0"
        );
        let Some(Variant::Dict(fields)) = get(&properties, "Metadata") else {
            panic!("Metadata is a dict");
        };
        assert_eq!(
            fields
                .iter()
                .find(|(key, _)| key == "mpris:length")
                .map(|(_, value)| value.clone()),
            Some(Value::Int64(300_000_000)),
            "five minutes in microseconds"
        );
    }
}
