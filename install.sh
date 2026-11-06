#!/bin/sh
# Installs rsedit for the current user (no root needed):
#   binary        -> ~/.cargo/bin/rsedit           (via `cargo install`)
#   launcher      -> ~/.local/share/applications/rsedit.desktop
#   icon          -> ~/.local/share/icons/hicolor/scalable/apps/rsedit.svg
#
# Usage: ./install.sh            install or update
#        ./install.sh --uninstall remove everything again
set -eu

cd "$(dirname "$0")"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN="${CARGO_HOME:-$HOME/.cargo}/bin/rsedit"
DESKTOP="$DATA/applications/rsedit.desktop"
ICON="$DATA/icons/hicolor/scalable/apps/rsedit.svg"

refresh_menus() {
    # Let the desktop notice the change right away (each is optional).
    update-desktop-database "$DATA/applications" 2>/dev/null || true
    gtk-update-icon-cache -q "$DATA/icons/hicolor" 2>/dev/null || true
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || kbuildsycoca5 >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
    cargo uninstall rsedit 2>/dev/null || rm -f "$BIN"
    rm -f "$DESKTOP" "$ICON"
    refresh_menus
    echo "rsedit uninstalled. (Your session file ~/.config/rsedit/ was kept.)"
    exit 0
fi

echo "Building and installing the binary (release build, takes a few minutes)..."
cargo install --path . --locked --force

install -Dm644 packaging/rsedit.svg "$ICON"
mkdir -p "$(dirname "$DESKTOP")"
# Absolute path: launchers don't always have ~/.cargo/bin on their PATH.
sed "s|@BIN@|$BIN|" packaging/rsedit.desktop.in > "$DESKTOP.tmp"
chmod 644 "$DESKTOP.tmp"
mv "$DESKTOP.tmp" "$DESKTOP"
refresh_menus

echo "Installed: $BIN"
echo "rsedit is now in your application launcher (Development)."
