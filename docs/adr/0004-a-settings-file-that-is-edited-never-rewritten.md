# A settings file, that is edited and never rewritten

Status: accepted

Until now priel read no configuration file at all, and said so in three places
in the interface: a choice made in the theme picker or the output picker lasted
for that session, and a flag was the only way to keep one. That was a real
property, not an oversight - it meant there was exactly one place a setting
could come from, so no start could be broken by a file, and nothing priel wrote
could ever be a file another program owned.

It is also the property that makes a listener retype
`--device pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0` on every
start, or wrap the binary in a shell alias to keep a theme. A picker that cannot
outlive the session is a picker that has to apologise for itself in its own
footer. **The property is given up deliberately, in exchange for four settings
being remembered, and this ADR is the boundary that keeps the exchange from
widening.**

## What is in the file, and why exactly these four

`$XDG_CONFIG_HOME/priel/settings.conf`, falling back to
`~/.config/priel/settings.conf`, through the same `auth::config_dir()` that
already resolves the XDG base directory to spec - a fourth hand-rolled
resolution of the same variable is a fourth place to get "unset, empty, or not
absolute" wrong.

    theme      the colour palette          --theme
    device     the audio output device     --device
    exclusive  exclusive access to it      --exclusive / --shared
    log_level  detail in the diagnostic log --log-level, $PRIEL_LOG

The test for membership is not "would someone like this remembered". It is
**"can a flag already set it, and does it mean the same thing on the next
start"**. All four are answers to a question about this machine - which DAC,
which palette, how loud the log - and none of them is derived from anything
priel fetched. That is the whole of the file, and adding to it means answering
that question again in writing.

## What stays out, and why the distinction is load-bearing

**The session (`token.json`) and the client key (`credentials.json`) stay in
`$XDG_STATE_HOME`.** They are runtime state, not preference: obtained at
runtime rather than authored, regenerable, and meaningless on another machine -
the same category as a persisted cookie. They were moved out of the config
directory on purpose and `auth::migrate_from_config` exists to relocate an old
copy once. The arrival of a config file must not read as an invitation to move
them back. A settings file a user can copy between machines, or check into a
dotfiles repository, is exactly the wrong place for a bearer token.

**No credentials, of any kind.** The user-written credentials override was
removed deliberately; `AuthConfig` requires the caller to supply an identity and
priel ships none. `client_id` in `settings.conf` would be that override back
under a new name.

**Not `--log-file`.** A redirected log is a thing done once, for a bug report,
by a person who is already typing a flag. Persisting a *path* also has a failure
mode the other four do not: a stale path in a file silently sends every
subsequent run's diagnostics somewhere the user has forgotten about, which is
worse than no log.

**Nothing that moves on its own.** Volume, the queue, the current view, the
selected row, the window size, the shuffle flag. Those are session state; a
settings file that acquires them becomes a state file with a misleading name,
and priel would then be writing on every keypress rather than on the one action
that was a choice.

## The format: `key = value`, parsed here, no new dependency

One `key = value` per line. `#` at the start of a line (after any indent) is a
comment. No sections, no nesting, no inline comments - a device identifier is an
opaque string from the sound server and is allowed to contain a `#`.

    # priel settings
    theme = gruvbox-dark
    device = pipewire/alsa_output.usb-SMSL_SMSL_USB_AUDIO-00.pro-output-0
    exclusive = false
    log_level = warn

**The values are spelled exactly as the flag spells them** - `gruvbox-dark`,
not `GruvboxDark` - because both sides go through the same clap `ValueEnum`.
What `--theme` accepts and what the file accepts cannot drift, and the man page
that lists one lists the other.

Four scalars do not justify a configuration-format crate. `serde` and
`serde_json` are already in the tree, so JSON would have been free in dependency
terms, and it is still the wrong answer: JSON has no comments, so a file a human
is invited to edit could carry no explanation of itself, and every write would
have to reformat the user's whole document. TOML would need `toml`, or
`toml_edit` for comment-preserving writes - a parser, a lexer and a span table
to keep four strings. The hand-rolled version is about forty lines, is a pure
function from `&str` to a struct, and has no supply chain.

## Precedence: the flag wins, then the file, then the default

The shape is copied from the one that already existed, `resolve_level(flag,
env)`: each setting is a pure function whose arguments are the sources, in
order, so precedence is a table of tests rather than a comment.

    --theme      >                 file > nord
    --device     >                 file > the default sink
    --exclusive  >                 file > shared
    --log-level  > $PRIEL_LOG    > file > warn

`$PRIEL_LOG` sits above the file because it is per-invocation, like a flag: it
is how a level is raised for one run started from a desktop entry. The file is
what the machine is normally set to.

**Every setting the file can hold must be answerable from the command line in
both directions**, or a file becomes a thing you cannot get out from under.
`--theme` and `--log-level` already take a value either way, and `--device auto`
is the sound server's own spelling of "the default sink". `--exclusive` was the
exception: a bare boolean flag cannot say *false*, so a file with
`exclusive = true` would have made shared output unreachable without editing the
file. **`--shared` is added for that, `overrides_with` its opposite**, and it is
the one CLI addition this change makes.

## priel writes the file, at exit, and only the lines it owns

Three options were live. Writing nothing means `--theme` is the only real
setting and every picker keeps apologising - it does not answer the issue.
Writing on every pick puts a `write(2)` on the render thread, which is the
thread that may not block; that rule is why the log has a writer thread of its
own, and a home directory on NFS would make a theme change stutter the audio.

**So: the pickers record their choice in memory, and `main` writes it once,
after the event loop has ended.** That is the same division that already puts
the credentials migration, the log file and the browser launch in `main` rather
than in `App::new` - **`App` owns no path and cannot write to a home directory,
so no test can either.** The cost is that a choice made in a session that is
`kill -9`ed is not kept. That is accepted; the alternative is I/O on the audio
UI thread.

Two rules make the write safe against a hand-edited file:

- **Only what changed this session is written.** A value that came from a flag
  is never persisted - a flag is for one run, and silently making it permanent
  is the surprise this design exists to avoid. If no picker was used, priel does
  not open the file for writing at all.
- **The write is a line edit, not a serialisation.** The existing text is read,
  the one line whose key matches is replaced in place, a key not present is
  appended, and **every other line - comments, blank lines, spacing, and keys
  priel does not recognise - is reproduced byte for byte**. A user's file comes
  back as their file. If the file cannot be *read* (permissions, a directory in
  its place), priel does not write it either: overwriting what it could not
  inspect is how a hand-written file would be lost.

A file priel creates for itself opens with a comment saying what it is and that
a flag beats it, so the first time a user opens it, it explains itself.

## A bad file must never cost the user their music player

The rule that already governed `$PRIEL_LOG` governs every line of this file: it
gets set once and forgotten, and **a typo in it must not stop priel starting.**

- Absent: normal, especially on first run. Defaults, one line in the log at
  `info`, no warning.
- Unreadable: defaults, a warning naming the path and the OS error.
- A line that is not `key = value`, an unknown key, or a value that does not
  parse: **that line is dropped and the rest of the file is still applied.**
  All-or-nothing would let one stale key throw away three good settings. Each
  dropped line is a warning that names the line number and what it said.
- A key repeated: the first wins, the later one is a warning. First-wins is also
  what the writer edits, so read and write agree about which line is live.

The diagnostics are *returned*, not logged, because the file is read before the
logger exists - it is where the log level comes from. `main` loads the settings,
starts the logger at the level they yield, and then emits the notes it was
handed. Returning them is also what makes them assertable without a logger.

## Consequences

**Three interface strings became false and are corrected here**, not in a
follow-up: the output picker's footer, the theme picker's footer, and the
notices the two pickers and the `x` toggle raise. They now say the choice is
kept and that a flag overrides it for one run. The same sentence appears in
`--help` and the man page (generated from the same clap definition), in the `?`
overlay - which is where the running program says *where* the file is - and in
the README. **"priel reads no configuration file" is superseded by this ADR
wherever it still appears**, including in the agent guidance at
`.claude/CLAUDE.md`, which is not tracked in the repository and has to be
corrected in the working copy.

**`priel never reads or writes another application's files` is unchanged, and
is now the sharper of the two rules.** priel writes `$XDG_CONFIG_HOME/priel/`
and `$XDG_STATE_HOME/priel/` and nothing else, and there is deliberately no flag
to point the settings file elsewhere - the same reasoning that leaves the
session path unflagged.

**What would change this decision.** If a setting ever needs to be written
*during* a session rather than at the end of one - anything a background action
changes, or anything that must survive a crash - then the write moves off the UI
thread onto a thread of its own, in the shape `logging.rs` already has, and this
ADR is superseded rather than stretched. And if the file ever needs structure -
per-device settings, a list, anything with a second level - the hand-rolled
parser is the wrong tool and a real format should be adopted deliberately, not
by growing this one a line at a time.
