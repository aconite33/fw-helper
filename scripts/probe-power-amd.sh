#!/usr/bin/env bash
# probe-power-amd.sh — is there any usable power telemetry on this board?
#
# The Intel fork reads watts from intel-rapl-mmio:0's energy_uj counter. That zone does
# not exist here. What does exist is intel-rapl:0, whose energy_uj read back <denied> to
# an unprivileged survey — which is the PLATYPUS 0400 mitigation, not evidence of absence.
# The daemon runs as root, so the question is whether the counter actually advances.
#
# Also checks BAT1's current_now x voltage_now, which is whole-system draw rather than CPU
# package power. Different quantity, world-readable, and only meaningful on battery.
#
# Read-only. Run as root or the RAPL half cannot answer.
#
#   sudo ./scripts/probe-power-amd.sh

set -uo pipefail

rd() { cat "$1" 2>/dev/null; }
have() { [[ -r "$1" ]]; }

echo "== RAPL zones present =="
for z in /sys/class/powercap/intel-rapl*; do
    [[ -d "$z" ]] || continue
    printf '  %-28s name=%-12s enabled=%s\n' \
        "$(basename "$z")" "$(rd "$z/name")" "$(rd "$z/enabled")"
done
echo

# The counter is what matters. `enabled` refers to whether a RAPL *constraint* is active;
# it says nothing about whether the energy counter advances, and those are separate
# questions that are easy to conflate.
for z in /sys/class/powercap/intel-rapl:0 /sys/class/powercap/intel-rapl:0:0 \
         /sys/class/powercap/intel-rapl:1; do
    [[ -d "$z" ]] || continue
    name=$(rd "$z/name")
    echo "== $(basename "$z") ($name) =="
    if ! have "$z/energy_uj"; then
        echo "  energy_uj not readable (are you root?)"
        echo
        continue
    fi
    a=$(rd "$z/energy_uj")
    sleep 3
    b=$(rd "$z/energy_uj")
    range=$(rd "$z/max_energy_range_uj")
    if [[ -z "$a" || -z "$b" ]]; then
        echo "  energy_uj unreadable"
    elif [[ "$a" == "$b" ]]; then
        echo "  energy_uj = $a, UNCHANGED over 3 s — counter is frozen, not usable"
    else
        # Handle the wrap the same way the daemon must.
        if (( b >= a )); then d=$(( b - a )); else d=$(( range - a + b )); fi
        printf '  energy_uj %s -> %s   delta %s uJ over 3 s = %s.%03d W\n' \
            "$a" "$b" "$d" "$(( d / 3000000 ))" "$(( (d / 3000) % 1000 ))"
        echo "  -> counter ADVANCES; usable as a power source"
    fi
    echo
done

echo "== BAT1 (whole-system draw, only meaningful on battery) =="
B=/sys/class/power_supply/BAT1
status=$(rd $B/status)
cur=$(rd $B/current_now)
vol=$(rd $B/voltage_now)
echo "  status      $status"
echo "  current_now ${cur:-<absent>} uA"
echo "  voltage_now ${vol:-<absent>} uV"
if [[ -n "$cur" && -n "$vol" && "$cur" != "0" ]]; then
    # uA * uV = pW; /1e12 for W. Done in integer arithmetic to avoid needing bc.
    mw=$(( cur / 1000 * vol / 1000000 ))
    printf '  power       %s.%03d W\n' "$(( mw / 1000 ))" "$(( mw % 1000 ))"
    [[ "$status" == "Discharging" ]] \
        || echo "  (on AC: this is charge current, NOT system draw)"
else
    echo "  power       unavailable (current_now is 0 — typical on AC)"
fi
