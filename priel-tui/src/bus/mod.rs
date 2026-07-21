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

//! The session bus, spoken directly.
//!
//! priel implements the D-Bus wire protocol itself rather than linking a bus
//! library: every one of them either brings an async executor or brings a
//! shared object, and priel wants neither. See
//! `docs/adr/0003-the-session-bus-is-spoken-directly-or-not-at-all.md` for the
//! measurements behind that.
//!
//! This module owns nothing but bytes and sockets. It holds no player state and
//! decides no policy: what to publish, and which actions the bus may reach, is
//! decided above it.

pub(crate) mod conn;
pub(crate) mod wire;
