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
# Installs the polkit actions that gate hardware writes, and with --systemd the
# fan restore binary the unit's ExecStopPost depends on.

set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "ERROR: run with sudo" >&2; exit 1; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# Layout differs by distribution, so both of these are detected rather than assumed.
#
# dbus-daemon reads /etc/dbus-1/system.d as well as its own share directory, but Debian
# ships that directory and Arch does not - so it is created rather than required. /etc is
# the right home for a local install either way; the share directory belongs to packages.
DBUS_DIR=/etc/dbus-1/system.d
POLICY=$DBUS_DIR/org.fwhelper.Daemon1.conf
UNIT=/etc/systemd/system/fw-helperd.service

# /usr/libexec does not exist on Arch, which puts helper binaries under /usr/lib. The
# unit ships with /usr/libexec paths and is rewritten at install time to match whichever
# is in use, rather than creating a directory the distribution deliberately omits.
if [[ -d /usr/libexec ]]; then
    LIBEXEC=/usr/libexec
else
    LIBEXEC=/usr/lib/fw-helper
fi
BIN=$LIBEXEC/fw-helperd
RESTORE=$LIBEXEC/fw-helper-restore-fan
CTL=/usr/local/bin/fw-helperctl
GUI=/usr/local/bin/fw-helper
POLKIT=/usr/share/polkit-1/actions/org.fwhelper.policy
MODPROBE=/etc/modprobe.d/fw-helper.conf
PROFILES=/etc/fw-helper/profiles.d

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
    rm -fv "$POLICY" "$UNIT" "$BIN" "$RESTORE" "$CTL" "$GUI" "$POLKIT" "$MODPROBE" \
        /etc/fw-helper/example-profile.conf
    # Leave $PROFILES and anything in it: those are the user's, not ours.
    # Only ever remove directories this script may have created, and only when empty.
    rmdir --ignore-fail-on-non-empty "$PROFILES" /etc/fw-helper "$DBUS_DIR" \
        /etc/dbus-1 2>/dev/null || true
    [[ "$LIBEXEC" == /usr/lib/fw-helper ]] && rmdir "$LIBEXEC" 2>/dev/null || true
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

install -D -m 644 -v "$REPO/data/org.fwhelper.Daemon1.conf" "$POLICY"
# The bus only reads system.d at startup or on reload.
systemctl reload dbus 2>/dev/null || echo "note: could not reload dbus; a reboot will pick it up"

# The reload succeeds even when the bus rejected our file, so check the journal.
if journalctl -u dbus.service --since "10 seconds ago" --no-pager 2>/dev/null \
        | grep -q "org.fwhelper.Daemon1.conf"; then
    echo "WARNING: dbus reported a problem with the policy file:" >&2
    journalctl -u dbus.service --since "10 seconds ago" --no-pager \
        | grep -A2 "org.fwhelper.Daemon1.conf" >&2
fi

# Put the CLI and the GUI on PATH via shims that resolve the newest build at RUN time.
#
# A plain symlink to target/release goes stale the moment you rebuild only debug,
# and then behaves like an older version of the program, which looks exactly like a
# bug in the daemon rather than a stale binary. Resolving per-invocation removes the
# failure mode instead of narrowing it. Packaging (M7) installs a real binary.
#
# Remove first, ALWAYS. `cat >` follows symlinks, and older versions of this script
# installed $CTL as a symlink to target/release/fw-helperctl. Writing through it
# overwrote the real binary with this shim, which then exec'd itself forever at 100%
# CPU. Worse, cargo hardlinks that path into target/release/deps, so the build
# artifact was clobbered too and cargo considered it fresh and would not rebuild.
# Measured 2026-08-21. Never write to a path in this script without unlinking it.
install_shim() {
    local name=$1 dest=$2
    rm -f "$dest"
    cat > "$dest" <<SHIM
#!/bin/sh
# fw-helper development shim, installed by scripts/install-dev.sh
R="$REPO/target/release/$name"
D="$REPO/target/debug/$name"
# Belt and braces against the failure above: if a build path is somehow this shim,
# exec'ing it would loop forever. Say so instead of spinning.
for c in "\$R" "\$D"; do
    if [ -f "\$c" ] && head -c 2 "\$c" 2>/dev/null | grep -q '^#!'; then
        echo "$name: \$c is a script, not a build. Rebuild:" >&2
        echo "  rm -f \$c && cargo build --release --all" >&2
        exit 1
    fi
done
if [ -x "\$R" ] && { [ ! -x "\$D" ] || [ "\$R" -nt "\$D" ]; }; then
    exec "\$R" "\$@"
fi
if [ -x "\$D" ]; then
    exec "\$D" "\$@"
fi
echo "$name: no build found; run 'cargo build --all' in $REPO" >&2
exit 1
SHIM
    chmod 755 "$dest"
    echo "installed shim: $dest (resolves newest build at run time)"
}

install_shim fw-helperctl "$CTL"
# The GUI too, so the Cinnamon applet's "Open fw-helper" item finds it on PATH
# without needing a build path typed into its settings.
install_shim fw-helper "$GUI"

install -m 644 -v "$REPO/data/org.fwhelper.policy" "$POLKIT"

# Where user profiles live. Created empty with the example alongside it rather than
# inside it, so a fresh install does not silently gain a profile nobody asked for.
install -d -m 755 -v "$PROFILES"
install -m 644 -v "$REPO/data/example-profile.conf" /etc/fw-helper/example-profile.conf
echo "user profiles: drop .conf files in $PROFILES (see /etc/fw-helper/example-profile.conf)"

if [[ "${1:-}" == "--systemd" ]]; then
    [[ -x "$REPO/target/release/fw-helperd" ]] || {
        echo "ERROR: build first: cargo build --release -p fw-helperd" >&2; exit 1; }
    # The unit's ExecStopPost points at this, and a unit referencing a missing
    # ExecStopPost binary fails to start. More to the point, it is the only thing
    # standing between a SIGKILLed daemon and a fan stuck at a fixed duty (ADR 0006),
    # so refuse to install a unit that cannot run it.
    [[ -x "$REPO/target/release/fw-helper-restore-fan" ]] || {
        echo "ERROR: build first: cargo build --release -p fw-helper-restore-fan" >&2; exit 1; }
    install -D -m 755 -v "$REPO/target/release/fw-helperd" "$BIN"
    install -D -m 755 -v "$REPO/target/release/fw-helper-restore-fan" "$RESTORE"
    # Point the unit at wherever the binaries actually went. rm first: `>` follows
    # symlinks, which this script has been bitten by before.
    rm -f "$UNIT"
    sed -e "s|/usr/libexec/fw-helperd|$BIN|g" \
        -e "s|/usr/libexec/fw-helper-restore-fan|$RESTORE|g" \
        "$REPO/data/fw-helperd.service" > "$UNIT"
    chmod 644 "$UNIT"
    echo "installed unit: $UNIT (ExecStart=$BIN)"
    systemctl daemon-reload
    # enable, then RESTART. `enable --now` only starts a unit that is not already
    # running, so on every install after the first it would leave the previous process
    # alive holding the old binary's inode: the files on disk are new, `systemctl
    # status` says active, and nothing you just built is running. Restarting is also
    # correct on a first install, where it is equivalent to starting.
    systemctl enable fw-helperd.service
    systemctl restart fw-helperd.service
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
