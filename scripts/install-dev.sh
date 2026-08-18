#!/usr/bin/env bash
# install-dev.sh — install just enough to run fw-helperd on the system bus.
#
# Development helper, not packaging (that is M7). Installs the D-Bus policy so the
# daemon may claim its name, and optionally the systemd unit.
#
#   sudo ./scripts/install-dev.sh            # policy only, run the daemon by hand
#   sudo ./scripts/install-dev.sh --systemd  # also install + start the unit
#   sudo ./scripts/install-dev.sh --enable-charge-control
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
POLKIT=/usr/share/polkit-1/actions/org.fwhelper.policy
MODPROBE=/etc/modprobe.d/fw-helper.conf

# Opt-in, never a side effect of installing: this changes which mechanism governs
# battery charging on the machine (ADR 0008).
if [[ "${1:-}" == "--enable-charge-control" ]]; then
    echo "This makes fw-helper the battery charge-limit authority."
    echo "Leave the battery limit in UEFI setup at its default, or the two will fight."
    echo
    install -m 644 -v "$REPO/data/fw-helper.modprobe.conf" "$MODPROBE"
    echo "Reloading cros_charge_control..."
    modprobe -r cros_charge_control 2>/dev/null || true
    modprobe cros_charge_control 2>/dev/null || true
    sleep 1
    if [[ -e /sys/class/power_supply/BAT1/charge_control_end_threshold ]]; then
        echo "OK: charge_control_end_threshold now present"
        echo "    current limit: $(cat /sys/class/power_supply/BAT1/charge_control_end_threshold)%"
    else
        echo "Not yet present. A reboot may be needed; check: dmesg | grep -i charge" >&2
    fi
    exit 0
fi

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl disable --now fw-helperd.service 2>/dev/null || true
    rm -fv "$POLICY" "$UNIT" "$BIN" "$CTL" "$POLKIT" "$MODPROBE"
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

# Put the CLI on PATH via a shim that resolves the newest build at RUN time.
#
# A plain symlink to target/release goes stale the moment you rebuild only debug,
# and then behaves like an older version of the program, which looks exactly like a
# bug in the daemon rather than a stale binary. Resolving per-invocation removes the
# failure mode instead of narrowing it. Packaging (M7) installs a real binary.
cat > "$CTL" <<SHIM
#!/bin/sh
# fw-helper development shim, installed by scripts/install-dev.sh
R="$REPO/target/release/fw-helperctl"
D="$REPO/target/debug/fw-helperctl"
if [ -x "\$R" ] && { [ ! -x "\$D" ] || [ "\$R" -nt "\$D" ]; }; then
    exec "\$R" "\$@"
fi
if [ -x "\$D" ]; then
    exec "\$D" "\$@"
fi
echo "fw-helperctl: no build found; run 'cargo build --all' in $REPO" >&2
exit 1
SHIM
chmod 755 "$CTL"
echo "installed shim: $CTL (resolves newest build at run time)"

install -m 644 -v "$REPO/data/org.fwhelper.policy" "$POLKIT"

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
    echo
    echo "Charge limiting needs one more opt-in step (see ADR 0008):"
    echo "    sudo $0 --enable-charge-control"
fi
