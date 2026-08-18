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
CTL=/usr/local/bin/fw-helperctl

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl disable --now fw-helperd.service 2>/dev/null || true
    rm -fv "$POLICY" "$UNIT" "$BIN" "$CTL"
    systemctl daemon-reload
    systemctl reload dbus 2>/dev/null || true
    echo "removed."
    exit 0
fi

# Validate before installing. dbus-daemon skips an unparseable file and reports it
# only to the journal; the daemon then fails with a misleading AccessDenied.
if ! python3 -c "import xml.dom.minidom,sys; xml.dom.minidom.parse(sys.argv[1])" \
        "$REPO/data/org.fwhelper.Daemon1.conf" 2>/tmp/fw-policy-parse.err; then
    echo "ERROR: D-Bus policy is not well-formed XML, refusing to install:" >&2
    cat /tmp/fw-policy-parse.err >&2
    exit 1
fi

install -m 644 -v "$REPO/data/org.fwhelper.Daemon1.conf" "$POLICY"
# The bus only reads system.d at startup or on reload.
systemctl reload dbus 2>/dev/null || echo "note: could not reload dbus; a reboot will pick it up"

# The reload succeeds even when the bus rejected our file, so check the journal.
if journalctl -u dbus.service --since "10 seconds ago" --no-pager 2>/dev/null \
        | grep -q "org.fwhelper.Daemon1.conf"; then
    echo "WARNING: dbus reported a problem with the policy file:" >&2
    journalctl -u dbus.service --since "10 seconds ago" --no-pager \
        | grep -A2 "org.fwhelper.Daemon1.conf" >&2
fi

# Symlink the CLI onto PATH so `fw-helperctl` works from any directory. It is
# unprivileged and holds no hardware access; everything goes over D-Bus.
#
# Pick whichever build is NEWEST, not release-by-preference: a stale release binary
# left over from an earlier milestone will silently shadow a fresh debug build and
# behave like an old version of the program.
newest=""
for build in release debug; do
    candidate="$REPO/target/$build/fw-helperctl"
    [[ -x "$candidate" ]] || continue
    if [[ -z "$newest" || "$candidate" -nt "$newest" ]]; then
        newest="$candidate"
    fi
done

if [[ -n "$newest" ]]; then
    ln -sfnv "$newest" "$CTL"
    other=$([[ "$newest" == *release* ]] && echo "$REPO/target/debug/fw-helperctl" \
                                         || echo "$REPO/target/release/fw-helperctl")
    if [[ -x "$other" ]]; then
        echo "note: both debug and release builds exist; linked the newer one."
        echo "      run 'cargo build --release --all' to keep them in step."
    fi
else
    echo "note: no fw-helperctl binary found; run 'cargo build --all' then re-run this"
fi

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
    echo "Policy installed. Start the daemon and query it in one go:"
    echo "    sudo sh -c '$REPO/target/debug/fw-helperd >/tmp/fw-helperd.log 2>&1 &'"
    echo "    fw-helperctl status"
    echo
    echo "Stop it with:  sudo pkill -x fw-helperd"
fi
