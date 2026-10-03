#!/bin/sh
# rPod uninstaller.
#
#   curl -fsSL https://raw.githubusercontent.com/erbator/rPod/master/uninstall.sh | sh
#
# Add --purge to also delete settings and iPod database backups:
#   curl -fsSL https://raw.githubusercontent.com/erbator/rPod/master/uninstall.sh | sh -s -- --purge
set -eu

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }

PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        *) warn "unknown option: $arg" ;;
    esac
done

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/rpod"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/rpod"

removed=0
for dir in "${RPOD_INSTALL_DIR:-$HOME/.local/bin}" "$HOME/.cargo/bin" /usr/local/bin; do
    [ -f "$dir/rpod" ] || continue
    if [ -w "$dir" ]; then
        rm -f "$dir/rpod"
        say "Removed $dir/rpod"
        removed=1
    else
        warn "$dir/rpod isn't writable by you; remove it with: sudo rm $dir/rpod"
    fi
done
[ "$removed" = 1 ] || warn "no rpod binary found"

DOC_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/doc/rpod"
if [ -d "$DOC_DIR" ]; then
    rm -rf "$DOC_DIR"
    say "Removed $DOC_DIR"
fi

if [ "$PURGE" = 1 ]; then
    for d in "$CONFIG_DIR" "$DATA_DIR"; do
        if [ -e "$d" ]; then
            rm -rf "$d"
            say "Removed $d"
        fi
    done
else
    [ -e "$CONFIG_DIR" ] && say "Kept settings:          $CONFIG_DIR"
    [ -e "$DATA_DIR" ]   && say "Kept iPod DB backups:   $DATA_DIR/backups  (your only way to undo past imports)"
    if [ -e "$CONFIG_DIR" ] || [ -e "$DATA_DIR" ]; then
        say "Run with --purge to delete those too."
    fi
fi

say "rPod uninstalled."
