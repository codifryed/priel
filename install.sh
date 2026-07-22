#!/bin/sh
# priel installer — build from source and install (Linux only).
#
# Quick install:
#   curl -fsSL https://codeberg.org/codifryed/priel/raw/branch/main/install.sh | sh
#
# Builds priel from source and installs it under ~/.local, so no root is needed.
# priel needs Rust and the mpv client library; if either is missing this script
# names the package for your distro and stops. Re-run it any time to update to
# the latest commit on the branch.
#
# Environment overrides:
#   PRIEL_PREFIX  install prefix (default: $HOME/.local). For a system-wide
#                 install run from a clone: sudo make install PREFIX=/usr/local
#   PRIEL_REF     git branch or tag to build (default: main)
#
# Unofficial software; not affiliated with TIDAL/Aspiro. Reads the script before
# piping it to a shell is always fair - it is short on purpose.

set -eu

REPO='https://codeberg.org/codifryed/priel'
NAME='priel'
REF="${PRIEL_REF:-main}"
PREFIX="${PRIEL_PREFIX:-$HOME/.local}"

say()  { printf '%s\n' "$*"; }
warn() { printf '%s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# Linux only, like priel itself.
[ "$(uname -s)" = Linux ] || die "priel is Linux-only (this is $(uname -s))."

# Report every missing prerequisite at once, rather than one failed run each.
missing=''
have git       || missing="$missing git"
have cargo     || missing="$missing cargo(Rust)"
have make      || missing="$missing make"
have cc || have gcc || have clang || missing="$missing cc"
have pkg-config || missing="$missing pkg-config"
# The mpv client library development package: priel's one non-Rust build dep.
if have pkg-config && ! pkg-config --exists mpv 2>/dev/null; then
    missing="$missing libmpv"
fi

if [ -n "$missing" ]; then
    warn "priel needs these first:$missing"
    warn ''
    warn '  Rust (cargo):  https://rustup.rs'
    warn '  build tools:   a C compiler, make, git and pkg-config'
    warn '  the mpv client library development package:'
    warn '    Debian/Ubuntu   sudo apt install libmpv-dev'
    warn '    Fedora          sudo dnf install mpv-devel'
    warn '    Arch            sudo pacman -S mpv'
    warn '    openSUSE        sudo zypper install mpv-devel'
    die  'install what is listed above, then run this again.'
fi

# Fetch the source into a temp dir that is removed however the script exits.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM
say "Fetching priel ($REF)…"
git clone --depth 1 --branch "$REF" "$REPO" "$tmp/src" >/dev/null 2>&1 \
    || die "could not clone $REPO at ref '$REF'."

# `make install` builds the release binary, generates the man page and shell
# completions, and places everything under PREFIX. The build output is left on
# screen on purpose: a fat-LTO release takes a few minutes, and a silent pipe
# reads as a hang.
say 'Building priel (a few minutes; optimised release)…'
if ! make -C "$tmp/src" install PREFIX="$PREFIX"; then
    warn ''
    warn 'The build failed. To see the full output, run it without the pipe:'
    warn "    git clone $REPO && cd $NAME && make install PREFIX=\"$PREFIX\""
    die  'build failed.'
fi

bin="$PREFIX/bin/$NAME"
[ -x "$bin" ] || die "the build finished but $bin is not there."

say ''
say "Installed: $bin"
"$bin" --version 2>/dev/null || true

# PATH check, the way herdr's installer does it: install where you like, but say
# so if what was just installed will not be found.
case ":${PATH}:" in
    *":$PREFIX/bin:"*) ;;
    *)
        say ''
        say "$PREFIX/bin is not on your PATH. Add this to your shell config:"
        say "    export PATH=\"$PREFIX/bin:\$PATH\""
        ;;
esac

say ''
say "Run it:     $NAME"
say 'Update:     re-run this installer.'
say "Uninstall:  git clone $REPO && make -C $NAME uninstall PREFIX=\"$PREFIX\""
