#!/usr/bin/env bash
# verify-fan-recovery.sh — the ADR 0006 release gate: does the fan come back to the EC
# when the daemon is killed without warning?
#
# Takes manual control at a high duty through the daemon, SIGKILLs the daemon so none
# of its own release paths can run, and watches the fan. Only ExecStopPost
# (fw-helper-restore-fan) and the restarted daemon's startup reclaim are left to save
# it, which is exactly what this is meant to prove.
#
# Works on boards without a mode register (ADR 0013): the evidence is the fan itself.
# The machine should be idle and cool, so that firmware wants the fan OFF - then a fan
# spinning down from a high manual duty can only mean the EC took it back.
#
#   sudo ./scripts/verify-fan-recovery.sh | tee docs/measurements/fan-recovery.txt

set -uo pipefail
[[ $EUID -eq 0 ]] || { echo "needs root: it kills the daemon" >&2; exit 1; }

DUTY=${DUTY:-180}   # ~70%, loud and unmistakable
EC=""
for d in /sys/class/hwmon/hwmon*; do [[ "$(cat "$d/name")" == cros_ec ]] && EC=$d; done
[[ -n "$EC" ]] || { echo "no cros_ec hwmon" >&2; exit 1; }

rpm()    { cat "$EC/fan1_input"; }
target() { cat "$EC/fan1_target"; }
now()    { date +%s.%N; }
ctl()    { /usr/local/bin/fw-helperctl "$@"; }

# If anything below fails, the fan must not be left held. Restore on every exit path.
cleanup() { /usr/lib/fw-helper/fw-helper-restore-fan >/dev/null 2>&1 \
            || /usr/libexec/fw-helper-restore-fan >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

echo "== baseline =="
echo "  board        $(cat /sys/class/dmi/id/board_name)"
echo "  fan          $(rpm) rpm   firmware target $(target) rpm"
ctl status 2>&1 | grep -E 'fan control' | sed 's/^/  /'
if (( $(target) > 0 )); then
    echo "  WARNING: firmware already wants the fan on; let the machine idle and rerun,"
    echo "           or the spin-down below cannot be told apart from firmware's own."
fi

echo; echo "== take manual control at duty $DUTY =="
ctl fan "$DUTY" 2>&1 | sed 's/^/  /'
for i in 1 2 3 4 5 6; do sleep 1; printf '  t+%ds  fan %s rpm\n' "$i" "$(rpm)"; done
HELD=$(rpm)
(( HELD > 3000 )) || { echo "  the fan did not spin up ($HELD rpm); stopping here"; exit 1; }

PID=$(systemctl show -p MainPID --value fw-helperd)
echo; echo "== SIGKILL fw-helperd (pid $PID) - none of its own release paths can run =="
T0=$(now)
kill -9 "$PID"

FIRST_DROP=""
for i in $(seq 1 60); do
    sleep 0.25
    r=$(rpm); t=$(awk -v a="$(now)" -v b="$T0" 'BEGIN{printf "%.2f", a-b}')
    printf '  +%5ss  fan %5s rpm   target %s\n' "$t" "$r" "$(target)"
    if [[ -z "$FIRST_DROP" ]] && (( r < HELD - 400 )); then FIRST_DROP=$t; fi
done

echo; echo "== what brought it back =="
journalctl -u fw-helperd --since "@${T0%.*}" --no-pager -o cat 2>/dev/null \
    | grep -E 'restore-fan|reclaim|no mode register|starting|board|fan control' | head -12 | sed 's/^/  /'

echo; echo "== verdict =="
FINAL=$(rpm)
if [[ -n "$FIRST_DROP" ]] && (( FINAL < HELD / 2 )); then
    echo "  PASS: fan began spinning down ${FIRST_DROP}s after SIGKILL, now $FINAL rpm"
else
    echo "  FAIL: fan still at $FINAL rpm after SIGKILL - it may be held with nothing managing it"
    exit 1
fi
