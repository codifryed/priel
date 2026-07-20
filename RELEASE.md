# Releasing priel

A release is a **signed tag plus a source tarball**, and - once the crates are
ready for it - three crates.io publishes on top.

The audience is a *packager*, and what they need is a tarball that builds offline
from a committed lockfile with no patches. `make dist`, `make vendor` and the GNU
install conventions in the `Makefile` are the contract; steps 1 to 7 are the
order to exercise them in. Step 8 covers crates.io, which is a different promise
with different rules: a version published there is permanent.

## 1. Decide the version

One version covers all three crates: `[workspace.package]` in the root
`Cargo.toml`.

While the project is `0.x`, semver puts breaking changes in the **minor**
position:

| Change | Bump |
|---|---|
| A CLI flag removed or renamed, a config file moved, a public library item changed | minor (`0.1.0` -> `0.2.0`) |
| Anything else | patch (`0.1.0` -> `0.1.1`) |

Commit subjects carry this already: a `!` after the type (`feat(tui)!:`) marks a
breaking change, so `git log --oneline <last-tag>..HEAD | grep '!:'` answers the
question. The removal of `--token-file` is the worked example - a flag someone
had in a launcher stops working, so it is a minor bump even though nothing
crashed.

## 2. Bump it

Edit `version` under `[workspace.package]` in the root `Cargo.toml`. That is the
only place: the crates inherit it with `version.workspace = true`, and the
`Makefile` reads it back out with `sed`, so `make help` printing the new number
is the check that it took.

**Then refresh the lockfile**, or nothing else in this list will run:

```bash
cargo update --workspace     # rewrites only priel's own entries
make help                    # should print the new version
```

`CARGO_FLAGS` defaults to `--locked`, so every `make` target refuses to build
while `Cargo.lock` still records the old version. This is the step that catches
people out.

Commit both files together, `chore: release 0.2.0`.

## 3. Verify

```bash
make check-deps    # cargo and libmpv are present
make check         # fmt, clippy pedantic, tests, both feature configurations
make build-nolibmpv
```

Then the things `make check` does not cover:

```bash
cargo +1.88 check --workspace --all-targets --locked   # the declared MSRV
make assets && man -l target/assets/priel.1   # read it, do not skim it
```

- **The MSRV claim is a promise to packagers**, whose toolchains lag, and it is
  the easiest thing in this repo to get quietly wrong. `rust-version` in the
  root `Cargo.toml` must actually build - and note that it fails for two
  independent reasons: a dependency raising *its* MSRV, and the source using a
  language feature newer than the claim. Only compiling catches the second, so
  check it rather than reading `cargo metadata` and assuming.
- **The man page and completions are generated from the clap derive** in
  `priel-tui/src/cli.rs` and are never committed, so `make install` regenerates
  them. Reading the page is still worth a minute: a stale doc comment on a flag
  ships as documentation. Never hand-edit `target/assets/priel.1`.

## 4. Write the notes

Conventional Commits make the first draft mechanical:

```bash
git log --oneline --no-merges <last-tag>..HEAD
```

Group by type - breaking first, then `feat`, then `fix` - and say what changed
for a *user*, not for the compiler. Three things belong in every set of notes:

- **Breaking changes, with the migration.** "`--token-file` is gone; the session
  is always at `$XDG_STATE_HOME/priel/token.json`" is the whole of what a user
  needs.
- **The MSRV**, if it moved. Packagers need it before they start.
- **New runtime files**, if any. priel writes to the user's state directory, and
  a new file appearing there without warning is rude.

The trademark constraint applies to release notes exactly as it does to commit
subjects and branch names: describe the change, not the service.

## 5. Tag

```bash
git tag -s v0.2.0 -m "priel 0.2.0"
git verify-tag v0.2.0
```

Signed and annotated, `v`-prefixed, matching the signed commits. A lightweight
tag carries no author, no date and no signature, which is three things a
packager checking provenance would like to have.

## 6. Build the artefacts

```bash
make dist                        # priel-<version>.tar.gz, archived from HEAD
sha256sum priel-*.tar.gz
```

`make dist` archives **HEAD**, so it must run after the tag commit exists, and it
includes only tracked files - which is why `Cargo.lock` being committed matters.

Verify the tarball actually stands alone, somewhere else, from nothing:

```bash
tar xzf priel-<version>.tar.gz && cd priel-<version>
make check-deps && make && make DESTDIR=/tmp/stage PREFIX=/usr install
```

If that fails, the release is broken however green the working tree was. It is
the only step that tests what a packager will actually run.

For an offline build, `make vendor` prints the `[source]` block to add to
`.cargo/config.toml`, after which `make CARGO_FLAGS='--locked --offline'` works
with no network at all.

## 7. Publish

Push the branch and the tag to Codeberg, create the release, attach the tarball
and its checksum, and paste the notes.

```bash
git push origin main
git push origin v0.2.0
```

## 8. Publish to crates.io

Only when the preconditions below are met. Unlike a tag, **a published version
can never be replaced** - `cargo yank` hides it from new resolutions but the
files stay up forever - so this step is worth being slower about than the rest.

Publish in dependency order, waiting for the index between each:

```bash
cargo publish -p priel-core
cargo publish -p priel-player
cargo publish -p priel-tui
```

Dry-run each one first (`--dry-run`), which packages and verifies without
uploading. What it catches:

- **Path dependencies need a version.** `priel-core = { path = "priel-core" }`
  publishes nothing, because crates.io has no path to follow:

  ```
  all dependencies must have a version requirement specified when publishing.
  dependency `priel-core` does not specify a version
  ```

  The fix is `{ path = "priel-core", version = "0.2.0" }` in the workspace
  dependency table - both, not either. Cargo uses the path locally and strips it
  on publish, so the two must be bumped together at step 2 from then on.
- **`repository` is read by more than people.** cargo warns without it; scanners
  and packagers rely on it.

Two things that only bite once:

- **The names must be free.** `priel`, `priel-core` and `priel-player` are
  claimed by whoever publishes first, and the binary crate is `priel-tui` while
  the binary itself is `priel` - decide which name goes on crates.io before
  claiming either.
- **The MSRV becomes someone else's problem.** Once a crate is published, its
  `rust-version` is a promise to downstream builds, and raising it in a patch
  release is the kind of thing that breaks other people's CI. Bump it in a minor
  release, and say so in the notes.

## 9. Afterwards

- `make clean` removes the build output and the tarball.
- Check the runtime files a fresh user ends up with, by running the new binary
  with `HOME` pointed somewhere empty. Nothing in this repo may write to a real
  home directory except the running program, and that includes the release you
  just built.

## Preconditions this repo has not met yet

Neither blocks a tag; both block step 8, and both would be embarrassing to
discover during a release rather than before one:

- **`repository` in the root `Cargo.toml` is empty.** It is what a packager, a
  security scanner and `cargo metadata` all read to find the source, and
  `cargo publish` warns about its absence.
- **The workspace path dependencies carry no version**, so `cargo publish` on
  anything but `priel-core` fails outright. See step 8.
