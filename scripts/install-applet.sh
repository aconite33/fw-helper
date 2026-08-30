#!/usr/bin/env bash
# install-applet.sh — install the Cinnamon panel applet for the current user.
#
# Deliberately unprivileged: the applet reads only world-readable sysfs, so it needs no
# root, no daemon and no D-Bus policy. Installing it into the user's own applet
# directory keeps it that way.
#
#   ./scripts/install-applet.sh              copy into place
#   ./scripts/install-applet.sh --link       symlink instead, for development
#   ./scripts/install-applet.sh --uninstall

set -euo pipefail

UUID="fw-helper@fwhelper.org"
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SRC="$REPO/cinnamon-applet/$UUID"
DEST_DIR="$HOME/.local/share/cinnamon/applets"
DEST="$DEST_DIR/$UUID"

[[ $EUID -ne 0 ]] || { echo "ERROR: run as your normal user, not root" >&2; exit 1; }

case "${1:-}" in
    --uninstall)
        rm -rfv "$DEST"
        echo
        echo "Removed. Also remove it from the panel via Cinnamon's applet settings"
        echo "if it is still there."
        exit 0
        ;;
esac

[[ -d "$SRC" ]] || { echo "ERROR: $SRC not found" >&2; exit 1; }

mkdir -p "$DEST_DIR"
# rm first: the destination may be a symlink from a previous --link run, and copying
# onto a symlink writes through it into the repo.
rm -rf "$DEST"

if [[ "${1:-}" == "--link" ]]; then
    ln -s "$SRC" "$DEST"
    echo "Linked $DEST -> $SRC"
else
    cp -r "$SRC" "$DEST"
    echo "Installed to $DEST"
fi

echo
echo "Next:"
echo "  1. Right-click the panel -> Applets  (or: cinnamon-settings applets)"
echo "  2. Find \"Framework Monitor\" and add it"
echo
echo "If it does not appear, restart Cinnamon: Alt+F2, type r, Enter."
echo "Errors, if any, land in ~/.xsession-errors or: journalctl -f -o cat /usr/bin/cinnamon"
