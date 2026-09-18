#!/usr/bin/env bash
# probe-fan-curve.sh — record firmware's own fan curve, heating and cooling, against
# every temperature sensor at once.
#
# Two questions this answers for a board, and both have to be measured:
#
#   1. Which sensor does firmware's fan decision actually follow? The floor must track
#      the same one, or "never quieter than firmware" compares against the wrong thing.
#      On the Intel board it is peci-temp. The AMD boards have no peci-temp, and the
#      daemon's fallback picks whichever label containing "cpu" comes first - which is a
#      board thermistor, not the die.
#   2. What firmware does on each branch. Its curve is hysteretic, so a heating run and a
#      cooling run describe different things, and only the heating branch says what a
#      temperature needs (ADR 0011).
#
# Needs fan1_target to be live, which it is on EC firmware lilac-4.0.2. Unprivileged:
# every value read here is world-readable. The load is plain bash under `timeout`, so it
# cannot outlive this script even if the script is killed.
#
#   ./scripts/probe-fan-curve.sh [heat_seconds] [cool_seconds]

set -uo pipefail

HEAT=${1:-100}
COOL=${2:-120}
STEP=2

EC=""; CPU=""
for d in /sys/class/hwmon/hwmon*; do
    case "$(cat "$d/name" 2>/dev/null)" in
        cros_ec) EC=$d ;;
        k10temp|coretemp) CPU=$d ;;
    esac
done
[[ -n "$EC" ]] || { echo "no cros_ec hwmon" >&2; exit 1; }

milli() { local v; v=$(cat "$1" 2>/dev/null) || { printf -- '-'; return; }; awk -v m="$v" 'BEGIN{printf "%.1f", m/1000}'; }

# Header from the sensors actually present, labelled, so a board with different ones
# produces a readable table rather than guessed column names.
header="phase    t_s"
cols=()
for t in "$EC"/temp*_input; do
    lbl=$(cat "${t%_input}_label" 2>/dev/null || basename "${t%_input}")
    header+=$(printf '  %16s' "$lbl"); cols+=("$t")
done
if [[ -n "$CPU" ]]; then
    for t in "$CPU"/temp*_input; do
        lbl=$(cat "${t%_input}_label" 2>/dev/null || echo cpu)
        header+=$(printf '  %16s' "$(cat "$CPU/name"):$lbl"); cols+=("$t")
    done
fi
header+=$(printf '  %8s  %8s' fan_in target)

row() {
    local phase=$1 t=$2 line
    line=$(printf '%-7s %5d' "$phase" "$t")
    for c in "${cols[@]}"; do line+=$(printf '  %16s' "$(milli "$c")"); done
    line+=$(printf '  %8s  %8s' "$(cat "$EC/fan1_input")" "$(cat "$EC/fan1_target")")
    echo "$line"
}

echo "# firmware fan curve, $(cat /sys/class/dmi/id/board_name 2>/dev/null)," \
     "EC $(head -1 /sys/class/chromeos/cros_ec/version 2>/dev/null | awk '{print $3}')," \
     "$(date -Iseconds)"
echo "# heat ${HEAT}s on $(nproc) threads, then cool ${COOL}s; ${STEP}s per row"
echo "$header"

row start 0
for i in $(seq 1 "$(nproc)"); do
    timeout "$HEAT" bash -c 'while :; do :; done' &
done
for ((t = STEP; t <= HEAT; t += STEP)); do sleep "$STEP"; row heat "$t"; done
wait 2>/dev/null
for ((t = STEP; t <= COOL; t += STEP)); do sleep "$STEP"; row cool "$t"; done
