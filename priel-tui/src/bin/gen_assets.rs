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

//! Writes the man page and shell completions, generated from the CLI definition.
//!
//! Run via `make assets`. Kept out of the shipped binary by the `gen-assets`
//! feature so `clap_mangen` and `clap_complete` are build-time only.

#[path = "../cli.rs"]
mod cli;

use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};

fn main() -> std::io::Result<()> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/assets".to_string()),
    );
    std::fs::create_dir_all(&out)?;

    let mut cmd = cli::Cli::command();

    let mut page = Vec::new();
    clap_mangen::Man::new(cmd.clone()).render(&mut page)?;
    std::fs::write(out.join("priel.1"), page)?;

    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        generate_to(shell, &mut cmd, "priel", &out)?;
    }

    println!("wrote priel.1 and shell completions to {}", out.display());
    Ok(())
}
