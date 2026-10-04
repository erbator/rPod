#!/bin/sh
# rPod installer.
#
#   curl -fsSL https://raw.githubusercontent.com/erbator/rPod/master/install.sh | sh
#
# Environment:
#   RPOD_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   RPOD_VERSION       release tag to install, e.g. v0.1.0 (default: latest)
set -eu

REPO="erbator/rPod"
BIN_DIR="${RPOD_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${RPOD_VERSION:-latest}"
DOC_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/doc/rpod"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() {
    if have curl; then curl -fsSL "$1" -o "$2"
    elif have wget; then wget -qO "$2" "$1"
    else die "curl or wget is required"
    fi
}

sha256_check() {
    if have sha256sum; then sha256sum -c "$1" >/dev/null 2>&1
    elif have shasum; then shasum -a 256 -c "$1" >/dev/null 2>&1
    else warn "no sha256sum/shasum found; skipping checksum"; return 0
    fi
}

[ "$(uname -s)" = Linux ] || die "rPod currently supports Linux only."

case "$(uname -m)" in
    x86_64 | amd64)  ARCH=x86_64 ;;
    aarch64 | arm64) ARCH=aarch64 ;;
    *)               ARCH="" ;;
esac

if [ "$VERSION" = latest ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
else
    BASE="https://github.com/$REPO/releases/download/$VERSION"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$BIN_DIR"

installed=""
# Releases up to v0.2.0 were static musl builds; later ones link glibc and ALSA.
for TARGET in ${ARCH:+"$ARCH-unknown-linux-gnu" "$ARCH-unknown-linux-musl"}; do
    asset="rpod-$TARGET.tar.gz"
    if fetch "$BASE/$asset" "$tmp/$asset" 2>/dev/null; then
        say "Downloaded $asset ($VERSION)"
        if fetch "$BASE/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
            (cd "$tmp" && sha256_check "$asset.sha256") || die "checksum mismatch for $asset"
        else
            warn "no checksum published for $asset; skipping verification"
        fi
        tar -xzf "$tmp/$asset" -C "$tmp"
        install -m 755 "$tmp/rpod" "$BIN_DIR/rpod"
        # License notices travel with the installed binary.
        for doc in LICENSE.md THIRD_PARTY_LICENSES.md; do
            if [ -f "$tmp/$doc" ]; then
                mkdir -p "$DOC_DIR"
                install -m 644 "$tmp/$doc" "$DOC_DIR/$doc"
            fi
        done
        installed=1
        break
    fi
done
[ -n "$installed" ] || warn "no prebuilt binary for this machine; falling back to building from source"

if [ -z "$installed" ]; then
    have cargo || die "no prebuilt binary for this machine and cargo isn't installed (get it from https://rustup.rs)"
    have pkg-config && pkg-config --exists alsa || warn "ALSA headers not found; install alsa-lib / libasound2-dev if the build fails"
    say "Building rPod from source with cargo (this takes a few minutes)"
    if [ "$VERSION" = latest ]; then
        cargo install --locked --git "https://github.com/$REPO" --root "$tmp/cargo" rpod
    else
        cargo install --locked --git "https://github.com/$REPO" --tag "$VERSION" --root "$tmp/cargo" rpod
    fi
    install -m 755 "$tmp/cargo/bin/rpod" "$BIN_DIR/rpod"
fi

version=$("$BIN_DIR/rpod" --version 2>&1) ||
    die "rpod won't start: $version
Install your distro's ALSA library (alsa-lib, or libasound2 on Debian/Ubuntu) and run it again."
say "Installed $version to $BIN_DIR/rpod"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not in your PATH. Add this to your shell config:
    export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac
have ffmpeg     || warn "ffmpeg not found: needed to convert FLAC, WAV, Ogg, Opus and other formats. Install it with your package manager."
have udisksctl  || warn "udisksctl not found: the 'e' eject key won't work (eject from your desktop instead)."

say "Done. Plug in your iPod and run: rpod"
