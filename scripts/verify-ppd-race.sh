#!/usr/bin/env bash
# Verify the PPD startup fixes, and the two defects the same boot exposed.
#
# The defect, measured on FRANMGCP09 at boot on 2026-09-27: the PPD probe blocked
# 90 s, systemd killed the unit on its start timeout, and PPD activated 45 ms after
# our process died - because our own blocking probe was the thing asking systemd to
# start PPD. docs/measurements/ppd-boot-race-hx370.txt.
#
# Reproducing that does not need a reboot. Stopping PPD and starting the daemon puts
# it in the same position: PPD absent, and activatable on request.
#
# Three arms:
#   1. PPD already up      -> ready fast, and delegating
#   2. PPD down at startup -> STILL ready fast, then adopted when PPD returns
#   3. a profile applies its fan curve on a board with no power-limit interface
#
# Run as root. Read it first: it stops and starts power-profiles-daemon.
set -uo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "run as root: sudo $0" >&2
    exit 1
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Ready must be well inside this. The bounded probe is 2 s, so anything near the unit's
# TimeoutStartSec=20 means the daemon is blocking on something at startup again.
READY_BUDGET=10

pass=0
fail=0
ok() {
    echo "  PASS  $1"
    pass=$((pass + 1))
}
bad() {
    echo "  FAIL  $1"
    fail=$((fail + 1))
}

# Is the running daemon the one that was just built? The stale-binary trap has cost this
# project two debugging sessions, and an install log saying "installed" is not evidence.
echo "== binary =="
running=$(systemctl show fw-helperd -p ExecStart --value | sed -n 's/.*path=\([^ ]*\).*/\1/p')
built="$REPO/target/release/fw-helperd"
if [[ -n "$running" && -f "$running" && -f "$built" ]]; then
    a=$(md5sum "$running" | cut -d' ' -f1)
    b=$(md5sum "$built" | cut -d' ' -f1)
    echo "  installed $running"
    echo "    md5 $a"
    echo "    md5 $b  (target/release)"
    # Fatal, not a recorded failure. Every arm below reads the journal of whatever is
    # actually running, so against a stale binary they report the old behaviour as if it
    # were the new code's - which is exactly the confusion this check exists to prevent.
    if [[ "$a" == "$b" ]]; then
        ok "the installed binary is the one just built"
    else
        echo
        echo "  STOP: the running daemon is not the binary you built."
        echo "  Install it, note the flag - without --systemd only policy is installed:"
        echo "      sudo ./scripts/install-dev.sh --systemd"
        exit 1
    fi
else
    echo "  STOP: cannot compare binaries (running=$running)" >&2
    exit 1
fi

# How long `systemctl restart` takes to return. The unit is Type=dbus, so systemd holds
# the restart until the bus name is claimed: this measures readiness as a client
# experiences it, not merely process start. Sets ELAPSED and RESTART_RC.
restart_and_time() {
    local start
    start=$(date +%s.%N)
    systemctl restart fw-helperd
    RESTART_RC=$?
    ELAPSED=$(awk -v a="$start" -v b="$(date +%s.%N)" 'BEGIN { printf "%.2f", b - a }')
}

echo
echo "== arm 1: PPD already running =="
systemctl start power-profiles-daemon 2>/dev/null
sleep 1
since=$(date '+%Y-%m-%d %H:%M:%S')
restart_and_time
t=$ELAPSED
echo "  restart returned after ${t}s (rc=$RESTART_RC)"
awk -v t="$t" -v b="$READY_BUDGET" 'BEGIN { exit !(t < b) }' \
    && ok "ready in ${t}s, inside the ${READY_BUDGET}s budget" \
    || bad "took ${t}s to become ready"
[[ "$(systemctl is-active fw-helperd)" == active ]] && ok "unit is active" || bad "unit is not active"
if journalctl -u fw-helperd --since "$since" --no-pager | grep -q "delegating to PPD"; then
    ok "delegated to PPD (ADR 0005)"
else
    bad "did not delegate to PPD though it was running"
    journalctl -u fw-helperd --since "$since" --no-pager | grep -i "power profiles" || true
fi

echo
echo "== arm 2: PPD unavailable at startup, then returning =="
echo "  NOTE: this arm cannot reproduce the 90s block itself. That needed boot-time"
echo "        contention, where PPD's activation was slow; here it starts in"
echo "        milliseconds. What it does test is the fallback path and the adoption"
echo "        that follows. A reboot is the only real gate on the boot race."
# Masked, not merely stopped. Stopping is not enough: PPD is D-Bus-activatable, so the
# probe's own call brings it straight back and the arm silently tests the opposite of
# what it means to - which is how a stopped-PPD run still reported "delegating".
systemctl stop power-profiles-daemon 2>/dev/null
systemctl mask power-profiles-daemon >/dev/null 2>&1
sleep 1
if systemctl is-active --quiet power-profiles-daemon; then
    echo "  SKIP  PPD is still running despite being masked. Nothing concluded."
    systemctl unmask power-profiles-daemon >/dev/null 2>&1
else
    since=$(date '+%Y-%m-%d %H:%M:%S')
    restart_and_time
    t=$ELAPSED
    echo "  restart returned after ${t}s with no PPD on the bus"
    # The whole point: an absent PPD must cost ~2 s, not 90 s and a kill.
    awk -v t="$t" -v b="$READY_BUDGET" 'BEGIN { exit !(t < b) }' \
        && ok "ready in ${t}s with PPD unavailable" \
        || bad "took ${t}s with PPD unavailable - the probe is blocking again"
    [[ "$(systemctl is-active fw-helperd)" == active ]] \
        && ok "unit reached active with PPD absent" || bad "unit did not reach active"
    journalctl -u fw-helperd --since "$since" --no-pager | grep -q "did not answer within" \
        && ok "logged the bounded probe giving up" \
        || bad "no bounded-probe message in the log"

    # Now hand PPD back and watch it get adopted. Without this half, losing the race
    # would still mean writing platform_profile behind the desktop's back all session.
    adopt_since=$(date '+%Y-%m-%d %H:%M:%S')
    systemctl unmask power-profiles-daemon >/dev/null 2>&1
    systemctl start power-profiles-daemon
    adopted=no
    for _ in $(seq 1 20); do
        if journalctl -u fw-helperd --since "$adopt_since" --no-pager | grep -q "PPD appeared"; then
            adopted=yes
            break
        fi
        sleep 0.5
    done
    if [[ "$adopted" == yes ]]; then
        ok "adopted PPD when it appeared, without a restart"
        journalctl -u fw-helperd --since "$adopt_since" --no-pager | grep "PPD appeared" | tail -1
    else
        bad "PPD came up but was never adopted"
    fi
fi

echo
echo "== arm 3: a profile on a board with no power-limit interface =="
# This aborted at the power limit before, so the fan curve a profile carries was never
# applied on the AMD boards. The journal said: could not follow PPD to performance.
since=$(date '+%Y-%m-%d %H:%M:%S')
if out=$(fw-helperctl profile performance 2>&1); then
    ok "profile performance applied"
else
    bad "profile performance failed: $out"
fi
sleep 2
log=$(journalctl -u fw-helperd --since "$since" --no-pager)
if grep -q "could not follow PPD\|power limit: no usable RAPL" <<<"$log"; then
    bad "the power limit still aborts profile application"
    grep -E "could not follow|power limit" <<<"$log" | tail -3
else
    ok "no power-limit abort in the log"
fi
if grep -q "profile performance applied" <<<"$log"; then
    ok "daemon logged the profile as applied"
    grep "profile performance applied" <<<"$log" | tail -1
fi
echo "  capability, as a user sees it:"
fw-helperctl status | grep -iE "power limit|fan|profile" | sed 's/^/    /'

# Never leave PPD masked, whatever happened above: the desktop's power slider depends
# on it, and a masked unit survives a reboot.
systemctl unmask power-profiles-daemon >/dev/null 2>&1
systemctl start power-profiles-daemon >/dev/null 2>&1

echo
echo "== summary =="
echo "  $pass passed, $fail failed"
echo "  Still unproven here: the boot race itself. Reboot, then:"
echo "      journalctl -u fw-helperd -u power-profiles-daemon -b -o short-precise | head -20"
[[ $fail -eq 0 ]]
