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
    let res = App::new(device, token)
        .and_then(|mut app| run(&mut terminal, &mut app, &mut TerminalEvents));
    restore(&mut terminal)?;
    if let Err(e) = res {
        eprintln!("priel error: {e:?}");
    }
    Ok(())
}

/// `None` means we printed help/version and the caller should exit quietly.
fn parse_args() -> Option<(Option<String>, String)> {
    parse_args_from(std::env::args().skip(1))
}

/// The parsing itself, over any argument sequence, so it can be tested without
/// the process environment.
fn parse_args_from(args: impl Iterator<Item = String>) -> Option<(Option<String>, String)> {
    let mut device = None;
    let mut token = priel_core::Client::default_token_path();
    let mut it = args;
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

/// Where input comes from.
///
/// Abstracted so the loop can be driven without a terminal: `event::poll` reads
/// the process's real stdin, which a test has no way to feed.
trait EventSource {
    /// Wait up to `timeout` for the next event, or `None` if none arrived.
    fn next(&mut self, timeout: Duration) -> Result<Option<Event>>;
}

/// The real one: crossterm on stdin.
struct TerminalEvents;

impl EventSource for TerminalEvents {
    fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

fn run<B: ratatui::backend::Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    events: &mut E,
) -> Result<()> {
    app.start();
    // Draw only when something actually changed. priel runs for hours, and an
    // unconditional redraw every tick spends CPU re-rendering an identical
    // screen while paused or idle. `App` owns the decision; see `take_dirty`.
    let mut needs_draw = true;
    loop {
        if needs_draw {
            terminal.draw(|f| ui::render(f, app))?;
        }

        match events.next(Duration::from_millis(100))? {
            Some(Event::Key(k)) => app.on_key(k),
            Some(Event::Mouse(m)) => app.on_mouse(m),
            Some(Event::Resize(_, _)) => app.mark_dirty(),
            Some(_) | None => {}
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

#[cfg(test)]
mod tests {
    use super::{EventSource, HELP, parse_args_from, run};
    use crate::app::App;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    /// Replays a fixed script. A `None` entry is a tick where the poll simply
    /// timed out, which is the common case in a real session.
    struct Scripted(std::vec::IntoIter<Option<Event>>);

    impl EventSource for Scripted {
        fn next(&mut self, _timeout: Duration) -> anyhow::Result<Option<Event>> {
            Ok(self.0.next().flatten())
        }
    }

    fn press(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    /// Drive the real loop over a script that ends in `q`, and hand back the app
    /// so the test can assert on what the events did to it.
    fn drive(script: Vec<Option<Event>>) -> App {
        let (mut app, _to, _from) = App::rigged();
        let mut term = Terminal::new(TestBackend::new(80, 12)).expect("backend");
        let mut events = Scripted(script.into_iter());
        run(&mut term, &mut app, &mut events).expect("loop");
        app
    }

    fn parse(args: &[&str]) -> Option<(Option<String>, String)> {
        parse_args_from(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn no_arguments_means_default_sink_and_default_token() {
        // Goal: priel must be runnable with no flags at all.
        let (device, token) = parse(&[]).expect("should run");
        assert!(
            device.is_none(),
            "no --device means the system default sink"
        );
        assert!(token.ends_with("hiresti_token.json"));
    }

    #[test]
    fn the_device_and_token_flags_are_read() {
        // Goal: --device is how a user reaches their DAC and --token-file is how
        // they point at a non-standard login.
        let (device, token) =
            parse(&["--device", "pipewire/dac", "--token-file", "/t.json"]).expect("should run");
        assert_eq!(device.as_deref(), Some("pipewire/dac"));
        assert_eq!(token, "/t.json");
    }

    #[test]
    fn help_and_version_ask_the_caller_to_exit() {
        // Goal: `None` is the signal to exit quietly rather than open a TUI over
        // the text just printed.
        for flag in ["-h", "--help", "-V", "--version"] {
            assert!(parse(&[flag]).is_none(), "{flag} should stop startup");
        }
    }

    #[test]
    fn unknown_and_valueless_flags_do_not_derail_startup() {
        // Goal: a stray argument should not stop the app launching, and a
        // trailing flag with no value must not panic on the missing argument.
        let (device, _) = parse(&["--nonsense", "--device"]).expect("should still run");
        assert!(device.is_none());
    }

    #[test]
    fn the_loop_runs_until_quit_and_applies_the_events_it_sees() {
        // Goal: the whole loop end to end - events reach the app, the app is
        // refreshed, and `should_quit` is what ends it. A loop that ignored
        // should_quit would hang the process on `q`.
        let app = drive(vec![Some(press('2')), Some(press('q'))]);
        assert!(app.should_quit);
        assert_eq!(app.view, crate::app::View::Playlists, "the key was applied");
    }

    #[test]
    fn a_quiet_loop_still_ends_on_quit() {
        // Goal: `None` from the event source is the common case (the poll timed
        // out), and an ignored event kind must be equally harmless. Neither may
        // be mistaken for input.
        let app = drive(vec![None, Some(Event::FocusGained), None, Some(press('q'))]);
        assert!(app.should_quit);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn a_resize_is_handled_without_ending_the_loop() {
        // Goal: a resize only forces a redraw; treating it as input would move
        // the selection or worse.
        let app = drive(vec![Some(Event::Resize(40, 10)), Some(press('q'))]);
        assert!(app.should_quit);
        assert_eq!(app.selected, 0, "a resize is not a movement key");
    }

    #[test]
    fn a_mouse_event_reaches_the_app_through_the_loop() {
        // Goal: mouse events take a different branch from keys, and dropping
        // them here would disable the entire mouse interface.
        let app = drive(vec![
            Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })),
            Some(press('q')),
        ]);
        assert!(app.should_quit);
    }

    #[test]
    fn the_help_text_carries_the_unofficial_disclaimer() {
        // Goal: the disclaimer is a distribution requirement, not decoration, so
        // it must survive edits to the help text.
        assert!(HELP.contains("not affiliated"));
        assert!(HELP.contains("--device"));
        assert!(HELP.contains("--token-file"));
    }
}
