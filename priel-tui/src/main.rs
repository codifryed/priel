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
//!   --exclusive             take that device for priel alone (never automatic)
//!   --list-devices          print the output devices and exit
//!   --log-level <level>     diagnostics detail (default: warn; `$PRIEL_LOG` too)
//!   --log-file <path>       default: `~/.local/state/priel/priel.log`

mod app;
mod bus;
mod cli;
mod logging;
mod ui;
mod worker;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    // clap handles --help and --version itself, exiting before we touch the
    // terminal - important, since setup() puts it in raw mode.
    let args = cli::Cli::parse();
    // Started before the terminal is taken, so a problem opening the log lands
    // on a stderr the user can still see. It is never fatal: priel plays music
    // with or without a log.
    //
    // Done here rather than in `App::new` for the same reason as the migration
    // below - it writes to the user's home directory, and tests build an `App`.
    // The in-memory ring is created here whether or not a file can be opened,
    // so the `M` overlay works even when the log is off or unwritable.
    let recent = logging::Recent::default();
    if let Err(e) = logging::init(args.log_level(), &args.log_path(), &recent) {
        eprintln!("priel: no diagnostic log: {e:#}");
    }
    log::info!("priel {} starting", env!("CARGO_PKG_VERSION"));
    // Installed here, before the terminal exists, so it sits underneath the
    // restoring hook that `setup` adds and covers the run that never gets that
    // far. The hook is global, so it also catches the worker, player and
    // downloader threads, whose panics otherwise leave no trace at all - and
    // release builds are `panic = "abort"`, so there is no second chance.
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        logging::record_panic(&info.to_string());
        orig(info);
    }));
    // Answered before the terminal is taken. This exists for launchers, scripts
    // and bug reports, and the alternate screen would swallow every line of it.
    let outcome = if args.list_devices {
        print_devices();
        Ok(())
    } else {
        session(args, recent)
    };
    match &outcome {
        Ok(()) => log::info!("priel stopping"),
        Err(e) => log::error!("exiting: {e:?}"),
    }
    // The writer thread is detached and its sink is buffered, so the tail of the
    // log has to be asked for.
    log::logger().flush();
    outcome
}

/// The widest identifier column `--list-devices` will pad to.
///
/// One absurd device name should not push every description off the right of
/// the terminal.
const DEVICE_NAME_COLUMN_MAX: usize = 60;

/// The listing `--list-devices` prints, one line per device.
///
/// Pure, so the shape can be asserted without an audio device: the caller does
/// the printing. The identifier leads and is unadorned, because it is what gets
/// pasted after `--device`.
fn device_lines(devices: &[priel_player::AudioDevice]) -> Vec<String> {
    let width = devices
        .iter()
        .map(|d| d.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(DEVICE_NAME_COLUMN_MAX);
    devices
        .iter()
        .map(|d| format!("{:width$}  {}", d.name, d.description))
        .collect()
}

/// Print the output devices and stop.
fn print_devices() {
    let devices = priel_player::audio_devices();
    if devices.is_empty() {
        // Not on stdout: a caller parsing the listing wants an empty listing,
        // not a sentence to filter out.
        eprintln!("priel: no output devices were reported");
        return;
    }
    for line in device_lines(&devices) {
        println!("{line}");
    }
}

/// Everything between the log being up and the log being flushed.
///
/// Split out so that no exit path can skip the record of why priel stopped: a
/// terminal that cannot be prepared used to return straight out of `main`,
/// leaving the one file that would have explained it empty.
fn session(args: cli::Cli, recent: logging::Recent) -> Result<()> {
    // Both files used to live in the config directory. Move them once rather
    // than silently logging the user out to make a spec point.
    //
    // Done here rather than in `App::new` on purpose: this writes to the user's
    // home directory, and `App::new` is constructed by tests.
    priel_core::auth::migrate_from_config("token.json");
    priel_core::auth::migrate_from_config("credentials.json");
    let mut terminal = setup().context("preparing the terminal")?;
    let player = priel_player::PlayerConfig {
        mpv_log_level: args.mpv_log_level(),
        audio_device: args.device,
        exclusive: args.exclusive,
    };
    let res = App::new(player, priel_core::Client::default_token_path(), recent)
        .and_then(|mut app| run(&mut terminal, &mut app, &mut TerminalEvents));
    restore(&mut terminal)?;
    // Reported rather than returned: the terminal is back, and printing the
    // error beats anyhow's own rendering of it after an interactive session.
    if let Err(e) = res {
        log::error!("the session ended in an error: {e:?}");
        eprintln!("priel error: {e:?}");
    }
    Ok(())
}

fn setup() -> Result<Tui> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    // Bracketed paste turns a pasted URL into one event rather than a couple of
    // hundred key presses, which is the difference between the sign-in screen
    // feeling instant and feeling broken.
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    // Restore the terminal even if we panic mid-render.
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        orig(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
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
            Some(Event::Paste(text)) => app.on_paste(&text),
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
    use super::{EventSource, run};
    use crate::app::App;
    use crate::cli::{Cli, DISCLAIMER};
    use clap::{CommandFactory, Parser};
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

    fn parse(args: &[&str]) -> Cli {
        let mut with_name = vec!["priel"];
        with_name.extend_from_slice(args);
        Cli::try_parse_from(with_name).expect("should parse")
    }

    #[test]
    fn no_arguments_means_default_sink_and_a_quiet_log() {
        // Goal: priel must be runnable with no flags at all, and the log it
        // keeps by default must be small enough that nobody has to think about
        // it - warnings and errors only.
        let cli = parse(&[]);
        assert!(
            cli.device.is_none(),
            "no --device means the system default sink"
        );
        assert_eq!(cli.log_level(), log::LevelFilter::Warn);
        assert!(cli.log_path().ends_with("/priel/priel.log"));
    }

    #[test]
    fn the_device_flag_is_read() {
        // Goal: --device is how a user reaches their DAC.
        let cli = parse(&["--device", "pipewire/dac"]);
        assert_eq!(cli.device.as_deref(), Some("pipewire/dac"));
    }

    #[test]
    fn exclusive_access_is_asked_for_separately_from_the_device() {
        // Goal: the two are deliberately orthogonal - choosing a hardware
        // device does not imply taking it from everything else on the machine,
        // and a device can be opened either way. Spelling exclusivity as a
        // variant of --device would weld them together.
        assert!(
            !parse(&[]).exclusive,
            "priel never takes a device on its own"
        );
        assert!(
            !parse(&["--device", "alsa/hw:CARD=AUDIO,DEV=0"]).exclusive,
            "a hardware device on its own is still shared"
        );
        let cli = parse(&["--device", "alsa/hw:CARD=AUDIO,DEV=0", "--exclusive"]);
        assert!(cli.exclusive);
        assert_eq!(cli.device.as_deref(), Some("alsa/hw:CARD=AUDIO,DEV=0"));
        assert!(
            parse(&["--exclusive"]).exclusive,
            "and it is answerable on its own, for whatever device is default"
        );
    }

    #[test]
    fn the_device_listing_flag_is_read_and_is_off_by_default() {
        // Goal: --list-devices is the answer to "what strings does --device
        // accept", and it must not fire on a normal start.
        assert!(!parse(&[]).list_devices);
        assert!(parse(&["--list-devices"]).list_devices);
    }

    #[test]
    fn a_listed_device_shows_its_identifier_first_and_lines_the_rest_up() {
        // Goal: the identifier is what gets pasted after --device, so it leads
        // and is printed unadorned. The descriptions line up behind it because
        // this listing is read in bug reports.
        let devices = vec![
            priel_player::AudioDevice {
                name: "auto".into(),
                description: "Autoselect device".into(),
            },
            priel_player::AudioDevice {
                name: "pipewire/some.dac".into(),
                description: "A DAC".into(),
            },
        ];
        let lines = super::device_lines(&devices);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("auto "), "{:?}", lines[0]);
        assert!(lines[0].ends_with("Autoselect device"), "{:?}", lines[0]);
        assert!(
            lines[1].starts_with("pipewire/some.dac  A DAC"),
            "the widest identifier sets the column: {:?}",
            lines[1]
        );
        let column = |l: &str| l.find("A ").or_else(|| l.find("Autoselect"));
        assert_eq!(
            column(&lines[0]),
            column(&lines[1]),
            "the descriptions should share a column"
        );
    }

    #[test]
    fn listing_devices_with_none_to_list_prints_nothing() {
        // Goal: a script reading this gets an empty listing rather than a line
        // of prose it would have to filter out. The note about it goes to
        // stderr, where the caller printing it decides.
        assert!(super::device_lines(&[]).is_empty());
    }

    #[test]
    fn the_log_flags_are_read() {
        // Goal: raising the level and redirecting the file are the two things
        // asked of a user who is reporting a bug.
        let cli = parse(&["--log-level", "debug", "--log-file", "/tmp/p.log"]);
        assert_eq!(cli.log_level(), log::LevelFilter::Debug);
        assert_eq!(cli.log_path(), "/tmp/p.log");
    }

    #[test]
    fn the_environment_sets_the_level_only_when_the_flag_does_not() {
        // Goal: $PRIEL_LOG is how a user raises the level when priel is started
        // from a launcher rather than a shell. A typo there falls back to the
        // default instead of refusing to start: it gets set once and forgotten,
        // and it must not be able to cost someone their music player.
        use crate::cli::LogLevel;
        use log::LevelFilter;
        assert_eq!(Cli::resolve_level(None, Some("debug")), LevelFilter::Debug);
        assert_eq!(Cli::resolve_level(None, Some("DEBUG")), LevelFilter::Debug);
        assert_eq!(
            Cli::resolve_level(Some(LogLevel::Error), Some("trace")),
            LevelFilter::Error,
            "the flag wins"
        );
        assert_eq!(Cli::resolve_level(None, Some("verbose")), LevelFilter::Warn);
        assert_eq!(Cli::resolve_level(None, None), LevelFilter::Warn);
    }

    #[test]
    fn mpv_is_asked_for_the_detail_priel_was_asked_for() {
        // Goal: one --log-level controls both halves of the log. Asking mpv for
        // more than priel will record is wasted work; asking for less loses the
        // half that explains a playback failure.
        assert_eq!(parse(&[]).mpv_log_level().as_deref(), Some("warn"));
        assert_eq!(
            parse(&["--log-level", "info"]).mpv_log_level().as_deref(),
            Some("info")
        );
        assert_eq!(
            parse(&["--log-level", "debug"]).mpv_log_level().as_deref(),
            Some("v"),
            "mpv's own `debug` is far noisier than priel's"
        );
        assert!(
            parse(&["--log-level", "off"]).mpv_log_level().is_none(),
            "off means mpv is not asked at all"
        );
    }

    #[test]
    fn the_token_file_flag_is_gone() {
        // Goal: the session lives at one XDG path and nowhere else. Pointing
        // priel at another application's file would rewrite it on every token
        // refresh, so there is deliberately no way to ask for that.
        let err = Cli::try_parse_from(["priel", "--token-file", "/t.json"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn help_and_version_stop_before_the_terminal_is_touched() {
        // Goal: clap exits on these itself. If it returned normally instead, the
        // help text would be printed and then hidden by the alternate screen.
        for flag in ["-h", "--help", "-V", "--version"] {
            let err = Cli::try_parse_from(["priel", flag]).unwrap_err();
            assert!(
                matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ),
                "{flag} should be a display-and-exit, got {:?}",
                err.kind()
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_an_error_rather_than_being_ignored() {
        // Goal: the hand-rolled parser silently swallowed typos, so `--devise`
        // started with the default sink and no explanation.
        let err = Cli::try_parse_from(["priel", "--devise", "x"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        // Goal: `--device` with nothing after it is a mistake worth reporting.
        let err = Cli::try_parse_from(["priel", "--device"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn the_command_definition_stays_valid() {
        // Goal: clap's own consistency check. It also guards the man page and
        // completions, which are generated from this same definition.
        Cli::command().debug_assert();
    }

    #[test]
    fn the_help_text_carries_the_unofficial_disclaimer() {
        // Goal: the disclaimer is a distribution requirement, not decoration, so
        // it must survive edits and reach both --help and the man page.
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("not affiliated"), "{help}");
        assert!(help.contains("--device"), "{help}");
        assert!(help.contains("--log-level"), "{help}");
        assert!(DISCLAIMER.contains("not affiliated"));
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
}
