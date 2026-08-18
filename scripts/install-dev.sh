#!/usr/bin/env bash
# install-dev.sh — install just enough to run fw-helperd on the system bus.
#
# Development helper, not packaging (that is M7). Installs the D-Bus policy so the
# daemon may claim its name, and optionally the systemd unit.
#
#   sudo ./scripts/install-dev.sh            # policy only, run the daemon by hand
#   sudo ./scripts/install-dev.sh --systemd  # also install + start the unit
#   sudo ./scripts/install-dev.sh --uninstall
#
# Installs nothing that writes to hardware: M1b is read-only.

set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "ERROR: run with sudo" >&2; exit 1; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
POLICY=/etc/dbus-1/system.d/org.fwhelper.Daemon1.conf
UNIT=/etc/systemd/system/fw-helperd.service
BIN=/usr/libexec/fw-helperd

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl disable --now fw-helperd.service 2>/dev/null || true
    rm -fv "$POLICY" "$UNIT" "$BIN"
    systemctl daemon-reload
    systemctl reload dbus 2>/dev/null || true
    echo "removed."
    exit 0
fi

install -m 644 -v "$REPO/data/org.fwhelper.Daemon1.conf" "$POLICY"
# The bus only reads system.d at startup or on reload.
systemctl reload dbus 2>/dev/null || echo "note: could not reload dbus; a reboot will pick it up"

if [[ "${1:-}" == "--systemd" ]]; then
    [[ -x "$REPO/target/release/fw-helperd" ]] || {
        echo "ERROR: build first: cargo build --release -p fw-helperd" >&2; exit 1; }
    install -m 755 -v "$REPO/target/release/fw-helperd" "$BIN"
    install -m 644 -v "$REPO/data/fw-helperd.service" "$UNIT"
    systemctl daemon-reload
    systemctl enable --now fw-helperd.service
    systemctl --no-pager status fw-helperd.service | head -12
else
    echo
    echo "Policy installed. Run the daemon by hand:"
    echo "    sudo $REPO/target/debug/fw-helperd"
    echo "Then, in another shell:"
    echo "    fw-helperctl status"
fi
