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

//! What sits between priel and the output device, read from `pw-dump`.
//!
//! [`hw`](crate::hw) answers whether the chain is clean by reading the device's
//! own live parameters. It cannot say *what* made it that way, because a
//! `/proc/asound` readout has no idea what is upstream of it. This module reads
//! the other end: the `PipeWire` graph, node by node, from the stream priel
//! feeds to the device that plays it.
//!
//! Which stream is priel's is settled by process id rather than by name. The
//! node `PipeWire` shows is libmpv's and is called `mpv`; matching on that would
//! also match a separate mpv the user happens to be running, and would break the
//! day libmpv renames it. The client object carries
//! `application.process.id`, so the answer is exact.
//!
//! **This slice only reports.** The parsed path is deliberately shaped to be
//! built on: [`GraphNode::id`] is kept so a later pass can go back to the dump
//! for a node's other properties, and [`NodeRole`] separates the hops that sit
//! between the stream and the device - which is where a resampler would be
//! named, where `allowed-rates` would be explained, and where a second
//! application holding the device would show up.
//!
//! Linux-only by nature, like the rest of the audio plumbing. Everywhere else
//! `pw-dump` is simply not installed, which is one of the answers.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

use crate::run::{self, RunError};

/// Where a node sits on the path from priel to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// The stream priel itself feeds into the graph.
    Stream,
    /// A hop between the stream and the device: a loopback, a filter chain, a
    /// virtual sink.
    Intermediate,
    /// The end of the chain - the device node, unless the links stop short of
    /// one.
    Device,
}

/// One node on the path, with the format it negotiated.
///
/// Every field is optional except the identity, because a node that has not
/// negotiated a format yet is a normal thing to find and is worth showing with
/// the format left blank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    /// The `PipeWire` object id. Kept so a later pass can look the node up
    /// again in the same dump rather than re-deriving everything here.
    pub id: u32,
    /// `node.name`, the stable identifier, e.g. `alsa_output.usb-...`.
    pub name: String,
    /// `node.description` if it has one, else the name again.
    pub description: String,
    /// `media.class` verbatim, e.g. `Audio/Sink` or `Stream/Output/Audio`.
    /// Kept because [`role`](Self::role) is positional and a later pass that has
    /// to be exact about what a node *is* needs the node's own claim.
    pub media_class: String,
    pub role: NodeRole,
    /// Negotiated rate in Hz. A rate change between two hops is a resample.
    pub rate_hz: Option<u32>,
    /// Negotiated sample format, e.g. `S32LE`.
    pub format: Option<String>,
    pub channels: Option<u32>,
}

/// The chain from priel's stream to the device that plays it, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioGraph {
    /// The stream first, the device last. Never empty when this is `Ok`.
    pub path: Vec<GraphNode>,
}

/// Why the graph could not be read.
///
/// The caller only ever displays these, but they stay distinct rather than
/// collapsing into one string: "the tools are not installed" and "nothing is
/// playing" are different situations and want different sentences, and a later
/// slice may want to act on the difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// `pw-dump` is not on `PATH`. Usually means no `PipeWire` at all.
    NotInstalled,
    /// It is there but could not be run, or it ran and failed.
    Unavailable(String),
    /// It did not answer inside the deadline and was killed.
    TimedOut,
    /// It answered with something that is not the object list it documents.
    Unreadable,
    /// The dump parsed, but priel has no stream in the graph.
    NoStream,
}

impl GraphError {
    /// What to do about it, where there is anything to do.
    ///
    /// Separate from [`Display`](fmt::Display) rather than a second sentence
    /// inside it, because the overlay draws one row per line and cannot rewrap
    /// a paragraph without its scroll bounds and its output disagreeing about
    /// how many lines there are.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NotInstalled => Some("It ships with PipeWire's command-line tools."),
            Self::Unavailable(_) => None,
            Self::TimedOut => Some("The graph was busy. Try again."),
            Self::Unreadable => Some("Worth reporting, with the log from [M]."),
            Self::NoStream => Some("It appears once playback starts."),
        }
    }
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("pw-dump was not found."),
            Self::Unavailable(why) => write!(f, "pw-dump could not be run: {why}."),
            Self::TimedOut => f.write_str("pw-dump did not answer within two seconds."),
            Self::Unreadable => f.write_str("pw-dump answered with something priel cannot parse."),
            Self::NoStream => f.write_str("priel has no stream in the graph."),
        }
    }
}

impl std::error::Error for GraphError {}

/// How long `pw-dump` gets. It normally answers in tens of milliseconds; this
/// is long enough for a loaded machine and short enough that a wedged one does
/// not hold the worker thread for a noticeable time.
const DUMP_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest chain worth following.
///
/// A stream normally reaches the device in one hop, and two or three with a
/// loopback or a filter chain in the way. The bound is what makes a graph that
/// links back on itself terminate.
const MAX_HOPS: usize = 8;

/// Read the graph for this process.
///
/// **Runs a subprocess and waits for it.** Call it from the worker thread, never
/// from the render loop.
///
/// # Errors
///
/// Returns the reason rather than an empty result: every failure here is
/// something the user is shown a sentence about.
pub fn probe() -> Result<AudioGraph, GraphError> {
    let out = run::capture("pw-dump", &[], DUMP_TIMEOUT).map_err(|e| match e {
        RunError::NotFound => GraphError::NotInstalled,
        RunError::TimedOut => GraphError::TimedOut,
        RunError::Spawn(why) => GraphError::Unavailable(why),
        RunError::Failed(Some(code)) => {
            GraphError::Unavailable(format!("it exited with status {code}"))
        }
        RunError::Failed(None) => GraphError::Unavailable("it was killed by a signal".into()),
        RunError::Unreadable => GraphError::Unavailable("its output could not be read".into()),
    })?;
    let text = String::from_utf8(out).map_err(|_| GraphError::Unreadable)?;
    parse(&text, std::process::id())
}

/// Pull the path belonging to `pid` out of a `pw-dump` object list.
///
/// Separate from [`probe`] so the parsing is testable against a recorded dump
/// with no `PipeWire` session anywhere near the machine running the tests.
///
/// # Errors
///
/// [`GraphError::Unreadable`] if the text is not the object list `pw-dump`
/// documents, [`GraphError::NoStream`] if it is but holds no stream for `pid`.
pub fn parse(dump: &str, pid: u32) -> Result<AudioGraph, GraphError> {
    let objects: Vec<Value> = serde_json::from_str(dump).map_err(|_| GraphError::Unreadable)?;
    let start = our_stream(&objects, pid).ok_or(GraphError::NoStream)?;

    // Walk the links downstream, bounded twice over: by `MAX_HOPS`, and by
    // refusing to visit a node already on the path. A loopback makes the link
    // graph a cycle, and either guard alone is enough to stop it - both are
    // here because the second also keeps the list honest, and the first also
    // covers a chain that is merely absurd rather than circular.
    let mut ids = Vec::new();
    let mut current = start;
    for _ in 0..MAX_HOPS {
        ids.push(current);
        let Some(next) = downstream(&objects, current) else {
            break;
        };
        if ids.contains(&next) {
            break;
        }
        current = next;
    }

    let last = ids.len().saturating_sub(1);
    let path: Vec<GraphNode> = ids
        .iter()
        .enumerate()
        .filter_map(|(hop, id)| node_at(&objects, *id, role_of(hop, last)))
        .collect();
    if path.is_empty() {
        return Err(GraphError::NoStream);
    }
    Ok(AudioGraph { path })
}

/// Where a hop sits, from its position rather than from its `media.class`.
///
/// Position is what the reader is being shown - the chain ends where the links
/// end, whatever the last node calls itself. `GraphNode::media_class` carries
/// the node's own claim for anything that needs to be exact about it.
fn role_of(hop: usize, last: usize) -> NodeRole {
    if hop == 0 {
        NodeRole::Stream
    } else if hop == last {
        NodeRole::Device
    } else {
        NodeRole::Intermediate
    }
}

/// The id of the output stream this process owns.
///
/// Prefers one that is running: libmpv can leave a suspended node behind after
/// a device change, and the running one is the chain actually in use.
fn our_stream(objects: &[Value], pid: u32) -> Option<u32> {
    let clients: Vec<u32> = objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Client"))
        .filter(|o| prop_u32(o, "application.process.id") == Some(pid))
        .filter_map(id_of)
        .collect();

    let mut suspended = None;
    for o in objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Node"))
    {
        let Some(client) = prop_u32(o, "client.id") else {
            continue;
        };
        if !clients.contains(&client) {
            continue;
        }
        if !prop_str(o, "media.class").is_some_and(|c| c.starts_with("Stream/Output")) {
            continue;
        }
        let Some(id) = id_of(o) else {
            continue;
        };
        if o.pointer("/info/state").and_then(Value::as_str) == Some("running") {
            return Some(id);
        }
        if suspended.is_none() {
            suspended = Some(id);
        }
    }
    suspended
}

/// The node the given one feeds into, if anything.
///
/// A stereo pair is two links between the same two nodes, so the first match is
/// the answer and the rest say the same thing again.
fn downstream(objects: &[Value], from: u32) -> Option<u32> {
    objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Link"))
        .find(|o| field_u32(o, "output-node-id") == Some(from))
        .and_then(|o| field_u32(o, "input-node-id"))
}

/// Build the reportable node for an id, or nothing if the dump has no such node.
fn node_at(objects: &[Value], id: u32, role: NodeRole) -> Option<GraphNode> {
    let o = objects
        .iter()
        .filter(|o| is_type(o, "PipeWire:Interface:Node"))
        .find(|o| id_of(o) == Some(id))?;
    let name = prop_str(o, "node.name").unwrap_or_default().to_string();
    let description = prop_str(o, "node.description")
        .filter(|d| !d.is_empty())
        .map_or_else(|| name.clone(), ToString::to_string);
    let format = o
        .pointer("/info/params/Format")
        .and_then(Value::as_array)
        .and_then(|f| f.first());
    Some(GraphNode {
        id,
        name,
        description,
        media_class: prop_str(o, "media.class").unwrap_or_default().to_string(),
        role,
        rate_hz: format.and_then(|f| f.get("rate")).and_then(as_u32),
        format: format
            .and_then(|f| f.get("format"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        channels: format.and_then(|f| f.get("channels")).and_then(as_u32),
    })
}

fn is_type(o: &Value, name: &str) -> bool {
    o.get("type").and_then(Value::as_str) == Some(name)
}

fn id_of(o: &Value) -> Option<u32> {
    o.get("id").and_then(as_u32)
}

fn field_u32(o: &Value, key: &str) -> Option<u32> {
    o.pointer("/info").and_then(|i| i.get(key)).and_then(as_u32)
}

fn prop_u32(o: &Value, key: &str) -> Option<u32> {
    o.pointer("/info/props")
        .and_then(|p| p.get(key))
        .and_then(as_u32)
}

fn prop_str<'a>(o: &'a Value, key: &str) -> Option<&'a str> {
    o.pointer("/info/props")
        .and_then(|p| p.get(key))
        .and_then(Value::as_str)
}

/// A `PipeWire` number, however this build of it chose to write one.
///
/// Properties are strings on the wire and numbers in the JSON dump depending on
/// where they came from, and a negotiated rate is sometimes a range object with
/// the agreed value under `default`. Accepting all three here keeps that out of
/// every call site.
fn as_u32(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    v.get("default").and_then(Value::as_u64)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::{AudioGraph, GraphError, NodeRole, parse};

    /// A real `pw-dump`, taken while priel was playing a 44.1 kHz track into a
    /// USB DAC. Trimmed to the objects on the path plus one unrelated sink, and
    /// with the host and user names replaced; nothing else was touched.
    const DUMP: &str = include_str!("../tests/fixtures/pw-dump-usb-dac.json");

    /// The pid priel was running under when the fixture was captured.
    const PID: u32 = 3_124_085;

    fn path_of(dump: &str, pid: u32) -> AudioGraph {
        match parse(dump, pid) {
            Ok(g) => g,
            Err(e) => panic!("the fixture should parse: {e}"),
        }
    }

    #[test]
    fn the_chain_to_the_device_is_listed_in_order_with_what_each_node_negotiated() {
        // Goal: this is the whole point of the overlay - which nodes are in the
        // way, and what each of them is doing to the audio. Order matters: the
        // reader follows it top to bottom from priel to the hardware.
        let g = path_of(DUMP, PID);
        assert_eq!(g.path.len(), 2, "stream then device, nothing in between");

        let stream = &g.path[0];
        assert_eq!(stream.role, NodeRole::Stream);
        assert_eq!(stream.rate_hz, Some(44_100));
        assert_eq!(stream.format.as_deref(), Some("S16LE"));
        assert_eq!(stream.channels, Some(2));

        let device = &g.path[1];
        assert_eq!(device.role, NodeRole::Device);
        assert_eq!(
            device.name,
            "alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0"
        );
        assert_eq!(device.description, "SMSL USB AUDIO Pro");
        assert_eq!(device.rate_hz, Some(44_100), "the device follows the rate");
        assert_eq!(device.format.as_deref(), Some("S32LE"));
        assert_eq!(device.media_class, "Audio/Sink");
    }

    #[test]
    fn a_sink_nothing_is_linked_to_stays_off_the_path() {
        // Goal: a machine has several sinks and the overlay must show the one in
        // use, not a device list. The fixture carries a second, idle sink.
        let g = path_of(DUMP, PID);
        assert!(
            !g.path.iter().any(|n| n.name.contains("HiFi__Speaker")),
            "an unconnected sink is not part of this chain"
        );
    }

    #[test]
    fn another_processs_stream_is_not_mistaken_for_ours() {
        // Goal: the node is libmpv's and is called `mpv`, so a name match would
        // happily report a completely separate mpv. Identity is the pid.
        assert_eq!(parse(DUMP, PID + 1), Err(GraphError::NoStream));
    }

    #[test]
    fn output_that_is_not_a_dump_is_reported_rather_than_panicking() {
        // Goal: pw-dump on a machine mid-upgrade, or a wrapper script printing a
        // warning first. None of these may take the overlay down with them.
        assert_eq!(parse("", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("not json at all", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("{\"id\": 1}", PID), Err(GraphError::Unreadable));
        assert_eq!(parse("[]", PID), Err(GraphError::NoStream));
        // A dump cut off mid-object, which is what a killed pw-dump leaves.
        let truncated = &DUMP[..DUMP.len() / 2];
        assert_eq!(parse(truncated, PID), Err(GraphError::Unreadable));
    }

    #[test]
    fn a_stream_linked_to_nothing_is_still_worth_showing() {
        // Goal: between tracks the stream can sit unlinked. Reporting only the
        // stream is a truthful answer; reporting nothing hides that priel is
        // there at all.
        let unlinked: String = DUMP.replace(
            "\"PipeWire:Interface:Link\"",
            "\"PipeWire:Interface:Other\"",
        );
        let g = path_of(&unlinked, PID);
        assert_eq!(g.path.len(), 1);
        assert_eq!(g.path[0].role, NodeRole::Stream);
    }

    #[test]
    fn a_graph_that_links_back_on_itself_terminates() {
        // Goal: a loopback can make the link graph a cycle. Following it must
        // stop rather than build a list until memory runs out.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "b"}}},
          {"id": 4, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}},
          {"id": 5, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 3, "input-node-id": 2}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(g.path.len(), 2, "each node once, then stop");
    }

    #[test]
    fn a_node_that_has_not_negotiated_a_format_reports_the_gap_rather_than_a_zero() {
        // Goal: a suspended device has no Format param. Showing 0 Hz would read
        // as a fact; showing nothing reads as what it is.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}}
        ]"#;
        let g = path_of(dump, 42);
        assert_eq!(g.path[0].rate_hz, None);
        assert_eq!(g.path[0].format, None);
        assert_eq!(g.path[0].description, "a", "the name stands in for it");
    }

    #[test]
    fn a_hop_between_the_stream_and_the_device_is_marked_as_one() {
        // Goal: the roles are what a later slice reads to say which node
        // resamples, so a loopback in the middle must not be filed as either
        // end of the chain.
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.process.id": 42}}},
          {"id": 2, "type": "PipeWire:Interface:Node",
           "info": {"props": {"client.id": 1, "media.class": "Stream/Output/Audio",
                              "node.name": "a"}}},
          {"id": 3, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink/Virtual", "node.name": "loop"}}},
          {"id": 4, "type": "PipeWire:Interface:Node",
           "info": {"props": {"media.class": "Audio/Sink", "node.name": "dac"}}},
          {"id": 5, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 2, "input-node-id": 3}},
          {"id": 6, "type": "PipeWire:Interface:Link",
           "info": {"output-node-id": 3, "input-node-id": 4}}
        ]"#;
        let g = path_of(dump, 42);
        let roles: Vec<NodeRole> = g.path.iter().map(|n| n.role).collect();
        assert_eq!(
            roles,
            vec![NodeRole::Stream, NodeRole::Intermediate, NodeRole::Device]
        );
    }

    #[test]
    fn every_failure_says_which_one_it_was_in_a_line_that_fits() {
        // Goal: the overlay draws one row per line and does not rewrap, so a
        // sentence longer than the box silently loses its tail - and the tail
        // is where the advice is.
        let all = [
            GraphError::NotInstalled,
            GraphError::Unavailable("it exited with status 1".into()),
            GraphError::TimedOut,
            GraphError::Unreadable,
            GraphError::NoStream,
        ];
        for e in all {
            let line = e.to_string();
            assert!(line.chars().count() <= 60, "too long to draw: {line}");
            if let Some(hint) = e.hint() {
                assert!(hint.chars().count() <= 60, "too long to draw: {hint}");
            }
        }
    }

    #[test]
    #[ignore = "runs pw-dump against whatever PipeWire session the machine has"]
    fn probing_a_live_session_never_panics() {
        let _ = super::probe();
    }
}
