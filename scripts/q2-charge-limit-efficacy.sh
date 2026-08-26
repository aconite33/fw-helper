#!/usr/bin/env bash
# Does the charge limit actually stop charging?
#
# This is the test M2 never had. ADR 0008 shipped a charge limit that was written,
# read back, persisted and re-applied across suspend and reboot - five green checks,
# every one of them upstream of the question "does charging stop". It did not. The
# battery went to 100% with the threshold at 80.
#
# So this script watches exactly one thing: charge_now across the threshold, on AC,
# starting from below it. Read-back is not efficacy. See ADR 0012.
#
# Needs no root. Run it after setting a limit and plugging in.
set -uo pipefail

BAT=/sys/class/power_supply/BAT1
INTERVAL=${INTERVAL:-30}
# How far past the limit counts as a failure. The EC reports whole percent and may
# settle a point either side, so 1 is noise and 3 is charging straight through.
MARGIN=${MARGIN:-3}
TIMEOUT_MIN=${TIMEOUT_MIN:-120}

read_int() { cat "$1" 2>/dev/null || echo 0; }

limit=$(fw-helperctl status 2>/dev/null | sed -n 's/^ *charge limit *\([0-9]\+\)%.*/\1/p')
if [ -z "$limit" ]; then
    echo "cannot read the charge limit from fw-helperctl; is the daemon running?" >&2
    exit 2
fi

ac=$(read_int /sys/class/power_supply/ACAD/online)
cap=$(read_int "$BAT/capacity")
status=$(cat "$BAT/status" 2>/dev/null)

echo "charge limit ........ ${limit}%"
echo "battery ............. ${cap}% ($status)"
echo "AC .................. $([ "$ac" = 1 ] && echo connected || echo "NOT CONNECTED")"
echo

if [ "$ac" != 1 ]; then
    echo "FAIL: plug in the charger. Nothing can be observed on battery." >&2
    exit 2
fi
if [ "$cap" -ge "$limit" ]; then
    echo "INCONCLUSIVE: battery is already at or above the limit." >&2
    echo "  Discharge below ${limit}% first, or this measures nothing - which is" >&2
    echo "  precisely how the previous mechanism passed for weeks." >&2
    exit 2
fi

out=${1:-$HOME/fw-helper-charge-efficacy-$(date +%Y%m%d-%H%M%S).log}
echo "logging to $out"
echo "watching every ${INTERVAL}s, up to ${TIMEOUT_MIN} min. Ctrl-C to stop."
echo
printf '%-9s %-5s %-11s %-9s %s\n' time cap status charge_now note | tee "$out"

deadline=$(( $(date +%s) + TIMEOUT_MIN * 60 ))
verdict="INCONCLUSIVE: ran out of time before reaching the limit"
rc=2
peak=0

while [ "$(date +%s)" -lt "$deadline" ]; do
    cap=$(read_int "$BAT/capacity")
    now=$(read_int "$BAT/charge_now")
    status=$(cat "$BAT/status" 2>/dev/null)
    [ "$cap" -gt "$peak" ] && peak=$cap
    note=""

    if [ "$cap" -gt $(( limit + MARGIN )) ]; then
        note="OVER LIMIT"
        verdict="FAIL: reached ${cap}% with a ${limit}% limit - charging straight through it"
        rc=1
        printf '%-9s %-5s %-11s %-9s %s\n' "$(date +%H:%M:%S)" "$cap%" "$status" "$now" "$note" | tee -a "$out"
        break
    fi

    # The pass condition: at or near the limit, and no longer charging.
    if [ "$cap" -ge $(( limit - 1 )) ] && [ "$status" != "Charging" ]; then
        note="STOPPED"
        verdict="PASS: charging stopped at ${cap}% against a ${limit}% limit (status=$status)"
        rc=0
        printf '%-9s %-5s %-11s %-9s %s\n' "$(date +%H:%M:%S)" "$cap%" "$status" "$now" "$note" | tee -a "$out"
        break
    fi

    printf '%-9s %-5s %-11s %-9s %s\n' "$(date +%H:%M:%S)" "$cap%" "$status" "$now" "$note" | tee -a "$out"
    sleep "$INTERVAL"
done

echo | tee -a "$out"
echo "$verdict" | tee -a "$out"
echo "peak observed: ${peak}%" | tee -a "$out"
exit $rc
