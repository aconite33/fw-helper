#!/usr/bin/env bash
# probe-rapl-domains.sh — is the RAPL "core" domain the whole CPU, or only one core?
#
# The package domain on this board reads sensibly (1.111 W at idle, measured). The core
# domain read 0.001 W at the same moment, which is implausible for twelve cores and
# suggests the driver is reporting AMD's per-core energy MSR for CPU0 alone. That
# distinction decides whether the core domain can be used to attribute power to
# processes, so it is measured rather than assumed.
#
# Three phases: idle, all cores loaded, then CPU0 alone. If `core` tracks `package`
# across all three it is the whole CPU. If it only moves when CPU0 is busy, it is one
# core and useless as a total.
#
# Read-only apart from the CPU load it creates. Needs root: energy_uj is 0400, the
# PLATYPUS mitigation.
#
#   sudo ./scripts/probe-rapl-domains.sh

set -uo pipefail
[[ $EUID -eq 0 ]] || { echo "ERROR: needs root to read energy_uj" >&2; exit 1; }

PKG=/sys/class/powercap/intel-rapl:0
CORE=/sys/class/powercap/intel-rapl:0:0
SECONDS_PER_PHASE=${SECONDS_PER_PHASE:-6}
NCPU=$(nproc)

for z in "$PKG" "$CORE"; do
    [[ -r "$z/energy_uj" ]] || { echo "ERROR: cannot read $z/energy_uj" >&2; exit 1; }
done

# uJ delta over the window, handling the counter wrap the same way the daemon must.
sample() {
    local zone=$1 secs=$2 a b range
    a=$(cat "$zone/energy_uj")
    sleep "$secs"
    b=$(cat "$zone/energy_uj")
    range=$(cat "$zone/max_energy_range_uj")
    if (( b >= a )); then echo $(( b - a )); else echo $(( range - a + b )); fi
}

# Both domains must be sampled over the SAME window or the comparison is meaningless.
measure() {
    local label=$1 secs=$2
    local pa pb ca cb prange crange pd cd
    pa=$(cat "$PKG/energy_uj");  ca=$(cat "$CORE/energy_uj")
    sleep "$secs"
    pb=$(cat "$PKG/energy_uj");  cb=$(cat "$CORE/energy_uj")
    prange=$(cat "$PKG/max_energy_range_uj"); crange=$(cat "$CORE/max_energy_range_uj")
    if (( pb >= pa )); then pd=$(( pb - pa )); else pd=$(( prange - pa + pb )); fi
    if (( cb >= ca )); then cd=$(( cb - ca )); else cd=$(( crange - ca + cb )); fi
    awk -v l="$label" -v p="$pd" -v c="$cd" -v s="$secs" 'BEGIN{
        pw = p/(s*1000000); cw = c/(s*1000000);
        printf "  %-16s package %7.3f W   core %7.3f W   core/package %5.1f%%\n",
               l, pw, cw, (pw > 0 ? cw*100/pw : 0);
    }'
}

# Busy loop in pure bash: no stress-ng on this machine, and this needs no package.
burn() {
    local mask=$1 count=$2 i
    PIDS=()
    for ((i = 0; i < count; i++)); do
        if [[ -n "$mask" ]]; then
            taskset -c "$mask" bash -c 'while :; do :; done' &
        else
            bash -c 'while :; do :; done' &
        fi
        PIDS+=($!)
    done
}
stop_burn() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
    wait 2>/dev/null
    PIDS=()
    sleep 2   # let the package settle before the next phase
}
trap stop_burn EXIT

echo "== RAPL domains, ${SECONDS_PER_PHASE}s per phase, ${NCPU} CPUs =="
echo
measure "idle" "$SECONDS_PER_PHASE"

burn "" "$NCPU"
measure "all ${NCPU} cores" "$SECONDS_PER_PHASE"
stop_burn

burn 0 1
measure "cpu0 only" "$SECONDS_PER_PHASE"
stop_burn

burn 1 1
measure "cpu1 only" "$SECONDS_PER_PHASE"
stop_burn

cat <<'NOTE'

How to read this:
  - core rises with package on ALL phases     -> whole-CPU domain, usable as a total
  - core rises only on "cpu0 only"            -> per-core MSR for CPU0; NOT a total
  - core never rises                          -> domain is not wired up; ignore it
NOTE
