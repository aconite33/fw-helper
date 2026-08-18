#!/usr/bin/env bash
# q6-pl1-load-test.sh — does PL1 actually GOVERN sustained package power,
# or does firmware/Intel Dynamic Tuning arbitrate it away?
#
# Q1 proved the register accepts and holds writes. That is not the same as the CPU
# obeying them. This runs a sustained load at two different PL1 values and measures
# real package power from energy_uj deltas.
#
#   sudo ./q6-pl1-load-test.sh [seconds]     default 60s of sampling per run
#
# Restores PL1 and kills all load on every exit path.

set -uo pipefail
[[ $EUID -eq 0 ]] || { echo "ERROR: needs root (energy_uj is 0400 — PLATYPUS mitigation)" >&2; exit 1; }

SAMPLE_SECS=${1:-60}
WARMUP=20          # PL1 averaging window is ~32s; sample well past it
COOL=20
ZONE=/sys/class/powercap/intel-rapl-mmio:0
EC=$(for d in /sys/class/hwmon/hwmon*; do [[ "$(cat $d/name 2>/dev/null)" == cros_ec ]] && echo $d && break; done)

ORIG_PL1=$(cat $ZONE/constraint_0_power_limit_uw)
MAXE=$(cat $ZONE/max_energy_range_uj)
LOAD_PGID=""

cleanup() {
    local rc=$?
    [[ -n "$LOAD_PGID" ]] && kill -TERM -"$LOAD_PGID" 2>/dev/null
    echo "$ORIG_PL1" > $ZONE/constraint_0_power_limit_uw 2>/dev/null
    printf '\n[restore] PL1 -> %s W (read back %s W)\n' \
        "$((ORIG_PL1/1000000))" "$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))"
    return $rc
}
trap cleanup EXIT INT TERM

if command -v stress-ng >/dev/null; then
    LOADER="stress-ng"
else
    LOADER="shell"
    echo "NOTE: stress-ng not installed — using shell workers, which are less power-dense."
    echo "      If the unconstrained result lands under $((ORIG_PL1/1000000)) W this test cannot"
    echo "      conclude anything. 'apt install stress-ng' gives a far better signal."
    echo
fi

start_load() {
    if [[ "$LOADER" == "stress-ng" ]]; then
        setsid stress-ng --cpu "$(nproc)" --cpu-method matrixprod -t 0 >/dev/null 2>&1 &
    else
        setsid bash -c 'for i in $(seq '"$(nproc)"'); do while :; do :; done & done; wait' >/dev/null 2>&1 &
    fi
    LOAD_PGID=$!
    sleep 1
}
stop_load() { [[ -n "$LOAD_PGID" ]] && kill -TERM -"$LOAD_PGID" 2>/dev/null; LOAD_PGID=""; }

# Package power over `dur` seconds, from energy counter delta. Handles counter wrap.
measure_w() {
    local dur=$1 e1 e2 t1 t2 de dt
    e1=$(cat $ZONE/energy_uj); t1=$(date +%s%N)
    sleep "$dur"
    e2=$(cat $ZONE/energy_uj); t2=$(date +%s%N)
    de=$(( e2 - e1 )); (( de < 0 )) && de=$(( de + MAXE ))   # wrap
    dt=$(( t2 - t1 ))
    awk -v de="$de" -v dt="$dt" 'BEGIN{ printf "%.2f", de*1000/dt }'
}

sensors() {
    local t="?" r="?"
    [[ -n "$EC" ]] && {
        for i in 1 2 3 4 5; do
            [[ "$(cat $EC/temp${i}_label 2>/dev/null)" == "peci-temp" ]] && \
                t=$(awk -v v="$(cat $EC/temp${i}_input)" 'BEGIN{printf "%.1f", v/1000}')
        done
        r=$(cat $EC/fan1_input 2>/dev/null)
    }
    printf "%5s C  %5s rpm" "$t" "$r"
}

run_at() {
    local label=$1 pl1_w=$2 pl1_uw=$(( $2 * 1000000 ))
    printf '\n\033[1m== %s: PL1 = %s W ==\033[0m\n' "$label" "$pl1_w"
    echo "$pl1_uw" > $ZONE/constraint_0_power_limit_uw
    local readback=$(( $(cat $ZONE/constraint_0_power_limit_uw) / 1000000 ))
    [[ "$readback" != "$pl1_w" ]] && echo "  WARNING: wrote ${pl1_w}W, register reads ${readback}W"

    start_load
    printf '  warmup %ss (PL1 window ~32s)...\n' "$WARMUP"
    sleep "$WARMUP"

    local sum=0 n=0 w
    local half=$(( SAMPLE_SECS / 2 )) elapsed=0
    while (( elapsed < SAMPLE_SECS )); do
        w=$(measure_w 5); elapsed=$(( elapsed + 5 ))
        printf '    t+%-3ss  %6s W   %s\n' "$elapsed" "$w" "$(sensors)"
        # average only the second half — by then PL1 is fully engaged
        if (( elapsed > half )); then
            sum=$(awk -v s="$sum" -v w="$w" 'BEGIN{print s+w}'); n=$(( n + 1 ))
        fi
    done
    stop_load

    STEADY=$(awk -v s="$sum" -v n="$n" 'BEGIN{ printf "%.2f", (n>0)? s/n : 0 }')
    printf '  \033[1msteady-state (last %ss): %s W\033[0m\n' "$half" "$STEADY"
    printf '  cooling %ss...\n' "$COOL"
    sleep "$COOL"
}

printf '\033[1m== idle baseline ==\033[0m\n'
printf '  %s W   %s\n' "$(measure_w 5)" "$(sensors)"

run_at "UNCONSTRAINED" "$((ORIG_PL1/1000000))"; HIGH=$STEADY
run_at "LIMITED"        15;                     LOW=$STEADY

printf '\n\033[1m== Q6 verdict ==\033[0m\n'
printf '  PL1 %sW -> %s W sustained\n' "$((ORIG_PL1/1000000))" "$HIGH"
printf '  PL1 15W -> %s W sustained\n' "$LOW"
awk -v hi="$HIGH" -v lo="$LOW" -v cap="$((ORIG_PL1/1000000))" 'BEGIN{
    d = hi - lo
    if (hi < 15.5) {
        print "  INCONCLUSIVE: load never exceeded 15 W, so the limit was never exercised."
        print "                Install stress-ng and re-run."
    } else if (d > 3 && lo < 18) {
        printf "  GOVERNS: draw tracked the limit (%.2f W drop, settled near 15 W).\n", d
        print "           PL1 is effective. M4 ships as a real control."
    } else if (d > 1) {
        printf "  PARTIAL: %.2f W drop but did not settle near 15 W — firmware is arbitrating.\n", d
        print "           Ship M4 as advisory; document that limits are hints, not guarantees."
    } else {
        printf "  NO EFFECT: %.2f W difference. The register accepts writes but does not govern.\n", d
        print "             Cut M4 or ship read-only, per the plan."
    }
}'
