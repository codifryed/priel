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

//! priel — hi-res TUI client for TIDAL. Mouse-first + VIM-first
//! (herdr/ncspot-inspired). Unofficial; not affiliated with or endorsed by TIDAL.
//!
//!   --device <mpv-device>   e.g. pipewire/alsa_output.usb-SMSL...pro-output-0
//!   --token-file <path>     hiresTI token (default: ~/.`config/hiresti/hiresti_token.json`)

mod app;
mod ui;
mod worker;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;

type Tui = Terminal<CrosstermBackend<Stdout>>;

const HELP: &str = concat!(
    "priel ",
    env!("CARGO_PKG_VERSION"),
    " — hi-res terminal client for TIDAL (mouse + VIM)

USAGE:
    priel [OPTIONS]

OPTIONS:
    --device <mpv-device>  output device, e.g. pipewire/alsa_output.usb-...
                           (default: the system default sink)
    --token-file <path>    hiresTI PKCE token file
                           (default: ~/.config/hiresti/hiresti_token.json)
    -h, --help             print this help and exit
    -V, --version          print version and exit

priel is unofficial software. It is not affiliated with, endorsed by, or
sponsored by TIDAL or Aspiro AB. TIDAL is a trademark of its respective owner.
A TIDAL subscription is required; priel neither circumvents access controls nor
exports content for offline use."
);

fn main() -> Result<()> {
    let Some((device, token)) = parse_args() else {
        return Ok(()); // printed help/version
    };
    let mut terminal = setup()?;
    let res = App::new(device, token).and_then(|mut app| run(&mut terminal, &mut app));
    restore(&mut terminal)?;
    if let Err(e) = res {
        eprintln!("priel error: {e:?}");
    }
    Ok(())
}

/// `None` means we printed help/version and the caller should exit quietly.
fn parse_args() -> Option<(Option<String>, String)> {
    let mut device = None;
    let mut token = priel_core::Client::default_token_path();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--device" => device = it.next(),
            "--token-file" => {
                if let Some(t) = it.next() {
                    token = t;
                }
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return None;
            }
            "-V" | "--version" => {
                println!("priel {}", env!("CARGO_PKG_VERSION"));
                return None;
            }
            _ => {}
        }
    }
    Some((device, token))
}

fn setup() -> Result<Tui> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;

    // Restore the terminal even if we panic mid-render.
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        orig(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn run(terminal: &mut Tui, app: &mut App) -> Result<()> {
    app.start();
    // Draw only when something actually changed. priel runs for hours, and an
    // unconditional redraw every tick spends CPU re-rendering an identical
    // screen while paused or idle. `App` owns the decision; see `take_dirty`.
    let mut needs_draw = true;
    loop {
        if needs_draw {
            terminal.draw(|f| ui::render(f, app))?;
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(k) => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(_, _) => app.mark_dirty(),
                _ => {}
            }
        }
        app.drain_worker();
        app.refresh();
        needs_draw = app.take_dirty();
        if app.should_quit {
            break;
        }
    }
    Ok(())
}
