#!/bin/sh
# Install sriv for the current user: the binary, the desktop entry, and the
# icon referenced by the desktop entry (Icon=sriv).
#
# Usage: ./install.sh [prefix]   (default prefix: ~/.local)
set -e

prefix="${1:-$HOME/.local}"

cargo build --release

install -Dm755 target/release/sriv "$prefix/bin/sriv"
install -Dm644 sriv.desktop "$prefix/share/applications/sriv.desktop"
# The icon is not square (460x307), so the legacy pixmaps dir is simpler than
# hicolor, which expects square sizes.
install -Dm644 ui/icon.png "$prefix/share/pixmaps/sriv.png"

echo "Installed to $prefix — make sure $prefix/bin is on your PATH."
