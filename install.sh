#!/bin/sh
# priel installer (Linux only).
#
# Quick install / update:
#   curl -fsSL https://raw.githubusercontent.com/codifryed/priel/main/install.sh | sh
#
# Downloads the latest release binary and installs it under ~/.local, so no root
# is needed. Where there is no binary for the platform - a non-x86_64 machine, or
# before the first release is cut - it builds from source instead, which needs
# Rust and the mpv client library. Re-run it any time to update; `priel --update`
# runs exactly this.
#
# Environment overrides:
#   PRIEL_PREFIX       install prefix (default: $HOME/.local). System-wide:
#                      run as root with PRIEL_PREFIX=/usr/local.
#   PRIEL_FROM_SOURCE  set to 1 to build from source even where a binary exists.
#   PRIEL_REF          git branch or tag to build from source (default: main).
#
# Unofficial software; not affiliated with TIDAL/Aspiro. Reading the script
# before piping it to a shell is always fair - it is short on purpose.

set -eu

REPO='https://github.com/codifryed/priel'
API='https://api.github.com/repos/codifryed/priel'
NAME='priel'
REF="${PRIEL_REF:-main}"
PREFIX="${PRIEL_PREFIX:-$HOME/.local}"

say()  { printf '%s\n' "$*"; }
warn() { printf '%s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ "$(uname -s)" = Linux ] || die "priel is Linux-only (this is $(uname -s))."
have curl || die 'this installer needs curl.'

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

# ---- the two ways in ----

# Download the release binary for this platform and unpack it into PREFIX.
#
# The release tarball is a prefix tree - bin/, share/man, share/…/completions -
# so installing is unpacking it and copying that tree into PREFIX, which is what
# `make install` and a distro package do too. Returns non-zero (rather than
# exiting) on anything it cannot do, so the caller can fall back to source.
install_prebuilt() {
    arch="$(uname -m)"
    [ "$arch" = x86_64 ] || { warn "no release binary for $arch."; return 1; }
    have tar || { warn 'no tar; cannot unpack a release.'; return 1; }

    # The newest release's tag, from the one field of the public API. The pattern
    # tolerates whitespace after the colon because GitHub pretty-prints its JSON
    # ("tag_name": "v1.2.3"), unlike the compact body the previous forge returned.
    tag="$(curl -fsSL "$API/releases/latest" 2>/dev/null \
        | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 \
        | sed 's/.*"\([^"]*\)"$/\1/')"
    [ -n "${tag:-}" ] || { warn 'no release published yet.'; return 1; }

    asset="${NAME}-${tag}-x86_64-linux.tar.gz"
    url="$REPO/releases/download/${tag}/${asset}"
    say "Downloading priel $tag…"
    curl -fsSL "$url" -o "$tmp/$asset" 2>/dev/null \
        || { warn "could not download $asset."; return 1; }

    # Verify the checksum when the release carries one; a mismatch is fatal, but
    # an absent sum only skips the check rather than blocking the install.
    if curl -fsSL "${url}.sha256" -o "$tmp/$asset.sha256" 2>/dev/null && have sha256sum; then
        ( cd "$tmp" && sha256sum -c "$asset.sha256" >/dev/null 2>&1 ) \
            || die "the download did not match its checksum; not installing it."
    fi

    tar -xzf "$tmp/$asset" -C "$tmp" || { warn 'the release would not unpack.'; return 1; }
    tree="$tmp/${NAME}-${tag}-x86_64-linux"
    [ -x "$tree/bin/$NAME" ] || { warn 'the release did not contain a binary.'; return 1; }

    # Copy the prefix tree into PREFIX. `cp -a` under bin/ and share/ lands each
    # file exactly where make install would put it.
    mkdir -p "$PREFIX"
    cp -a "$tree/." "$PREFIX/" || { warn "could not write to $PREFIX."; return 1; }

    # A prebuilt binary still links libmpv at runtime - the one non-Rust
    # dependency - so say so if it is not there. Not fatal: it may be installed
    # somewhere ldconfig does not list.
    if have ldconfig && ! ldconfig -p 2>/dev/null | grep -q 'libmpv\.so\.2'; then
        warn ''
        warn 'Note: libmpv.so.2 was not found. Install the mpv client library to play:'
        warn '    Debian/Ubuntu  sudo apt install libmpv2   (or libmpv-dev)'
        warn '    Fedora         sudo dnf install mpv-libs'
        warn '    Arch           sudo pacman -S mpv'
    fi
    return 0
}

# Clone and `make install`, the from-source path. Exits (does not return) on a
# missing prerequisite, because this is the last resort - there is nowhere left
# to fall back to.
install_from_source() {
    missing=''
    have git       || missing="$missing git"
    have cargo     || missing="$missing cargo(Rust)"
    have make      || missing="$missing make"
    have cc || have gcc || have clang || missing="$missing cc"
    have pkg-config || missing="$missing pkg-config"
    if have pkg-config && ! pkg-config --exists mpv 2>/dev/null; then
        missing="$missing libmpv-dev"
    fi
    if [ -n "$missing" ]; then
        warn "building from source needs these first:$missing"
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

    say "Fetching priel ($REF)…"
    git clone --depth 1 --branch "$REF" "$REPO" "$tmp/src" >/dev/null 2>&1 \
        || die "could not clone $REPO at ref '$REF'."
    # Output left on screen: a fat-LTO release takes a few minutes, and a silent
    # pipe reads as a hang.
    say 'Building priel (a few minutes; optimised release)…'
    make -C "$tmp/src" install PREFIX="$PREFIX" || die 'the build failed.'
}

# ---- do it ----

if [ "${PRIEL_FROM_SOURCE:-0}" = 1 ] || ! install_prebuilt; then
    [ "${PRIEL_FROM_SOURCE:-0}" = 1 ] || say 'Falling back to a source build.'
    install_from_source
fi

bin="$PREFIX/bin/$NAME"
[ -x "$bin" ] || die "install finished but $bin is not there."

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
say 'Update:     priel --update, or re-run this installer.'
say "Uninstall:  git clone $REPO && make -C $NAME uninstall PREFIX=\"$PREFIX\""
