#!/usr/bin/env bash
# Build fw-helper_<version>_<arch>.deb.
#
# dpkg-deb from a staging tree rather than cargo-deb: no extra tooling to install, and
# the layout is visible in one place. Packaging proper is M7; this is the whole of it.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

VERSION=$(awk -F'"' '/^version/ {print $2; exit}' Cargo.toml)
ARCH=$(dpkg --print-architecture)
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

echo "building release binaries..."
cargo build --release --all

install -d "$STAGE/DEBIAN" \
    "$STAGE/usr/bin" "$STAGE/usr/libexec" \
    "$STAGE/usr/share/applications" "$STAGE/usr/share/fw-helper" \
    "$STAGE/usr/share/doc/fw-helper" \
    "$STAGE/usr/share/polkit-1/actions" \
    "$STAGE/etc/dbus-1/system.d" "$STAGE/etc/fw-helper/profiles.d" \
    "$STAGE/lib/systemd/system"

# The daemon and the crash-path fan restore are not for users to run; the CLI and the
# window are.
install -m 755 target/release/fw-helperd            "$STAGE/usr/libexec/"
install -m 755 target/release/fw-helper-restore-fan "$STAGE/usr/libexec/"
install -m 755 target/release/fw-helperctl          "$STAGE/usr/bin/"
install -m 755 target/release/fw-helper             "$STAGE/usr/bin/"
install -m 755 data/fw-helper-enable-charge-control "$STAGE/usr/bin/"

install -m 644 data/org.fwhelper.Daemon1.conf "$STAGE/etc/dbus-1/system.d/"
install -m 644 data/org.fwhelper.policy       "$STAGE/usr/share/polkit-1/actions/"
install -m 644 data/fw-helperd.service        "$STAGE/lib/systemd/system/"
install -m 644 data/fw-helper.desktop         "$STAGE/usr/share/applications/"
install -m 644 data/example-profile.conf      "$STAGE/usr/share/fw-helper/"
# Kept out of /etc/modprobe.d: enabling it is the opt-in step, not the install.
install -m 644 data/fw-helper.modprobe.conf   "$STAGE/usr/share/fw-helper/"
install -m 644 README.md LICENSE              "$STAGE/usr/share/doc/fw-helper/"

# Work out library dependencies rather than guessing: the GUI links GTK4 and
# libadwaita and their sonames differ across releases.
#
# dpkg-shlibdeps insists on a debian/control existing relative to its working
# directory, even with -O, so it gets a throwaway one. It is deleted before the
# package is built - shipping a debian/ directory inside the .deb would be absurd.
install -d "$STAGE/debian"
cat > "$STAGE/debian/control" <<EOF
Source: fw-helper
Package: fw-helper
Architecture: $ARCH
EOF
DEPS=$(cd "$STAGE" && dpkg-shlibdeps -O --ignore-missing-info \
        usr/bin/fw-helper usr/bin/fw-helperctl usr/libexec/fw-helperd 2>/dev/null \
        | sed 's/^shlibs:Depends=//') || DEPS=""
rm -rf "$STAGE/debian"

if [ -z "$DEPS" ]; then
    # Not fatal, but the package would then declare no library dependencies at all and
    # would install onto a machine that cannot run it. Say so rather than shipping it
    # quietly.
    echo "WARNING: could not compute library dependencies; falling back to a minimum" >&2
    DEPS="libc6, libgtk-4-1, libadwaita-1-0"
fi

cat > "$STAGE/DEBIAN/control" <<EOF
Package: fw-helper
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: ${DEPS}, dbus, systemd
Recommends: power-profiles-daemon
Maintainer: Mark Hall <mark.a.hall@gmail.com>
Homepage: https://github.com/Wooloomooloo2/fw-helper
Description: Firmware control for Framework laptops
 Fan curves, sustained power limits, battery charge limit and performance
 profiles for the Framework Laptop 13 on Ubuntu, in one application.
 .
 Profiles are layered over power-profiles-daemon rather than replacing it, so
 the GNOME power slider keeps working and stays in sync.
 .
 Manual fan control is never taken without being asked, and is bounded by the
 firmware's own behaviour: the fan is never run slower than the EC would run it,
 and control is handed back on exit, on crash, on suspend, and if the daemon
 stops responding.
EOF

cat > "$STAGE/DEBIAN/conffiles" <<'EOF'
/etc/dbus-1/system.d/org.fwhelper.Daemon1.conf
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    systemctl daemon-reload || true
    # enable AND restart: `enable --now` leaves an already-running old binary in
    # place on upgrade, which is indistinguishable from the new one not working.
    systemctl enable fw-helperd.service || true
    systemctl restart fw-helperd.service || true

    # Ask whether charge control is configured to *persist*, not whether it happens to
    # work this minute. The sysfs node exists whenever the module was loaded with the
    # parameter set - including by a drop-in that has since been removed, since the
    # module keeps its parameter until reboot. Testing the node alone therefore reports
    # a machine as set up when the capability is one reboot from silently vanishing.
    if [ ! -e /etc/modprobe.d/fw-helper.conf ]; then
        echo ""
        echo "fw-helper: battery charge limiting is off by default."
        echo "  It changes which mechanism governs charging on this machine, so it"
        echo "  is opt-in:  sudo fw-helper-enable-charge-control"
        if [ -e /sys/class/power_supply/BAT1/charge_control_end_threshold ]; then
            echo ""
            echo "  Note: it is working right now, but only because the module was"
            echo "  loaded with the parameter earlier. Without the step above it will"
            echo "  stop working at the next reboot."
        fi
        echo ""
    fi
fi
EOF

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = deconfigure ]; then
    # Stopping runs the unit's ExecStopPost, which returns the fan to the EC. Do it
    # while the binary still exists: removing the package must never leave the fan
    # held at a fixed duty with nothing managing it.
    systemctl stop fw-helperd.service || true
    systemctl disable fw-helperd.service || true
    [ -x /usr/libexec/fw-helper-restore-fan ] && /usr/libexec/fw-helper-restore-fan || true
fi
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = purge ]; then
    rm -f /etc/modprobe.d/fw-helper.conf
    rm -rf /var/lib/fw-helper
    # Profiles the user wrote are theirs; remove the directory only if it is empty.
    rmdir --ignore-fail-on-non-empty /etc/fw-helper/profiles.d /etc/fw-helper 2>/dev/null || true
fi
systemctl daemon-reload || true
EOF

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

OUT="$REPO/fw-helper_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb --info "$OUT" | sed -n '1,12p'
echo
echo "contents:"
dpkg-deb --contents "$OUT" | awk '{print $6, $7, $8}' | grep -vE '/$' | sed 's/^/  /'
