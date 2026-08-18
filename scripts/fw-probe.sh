#!/usr/bin/env bash
# fw-probe.sh — capture what this Framework board actually exposes.
#
#   ./fw-probe.sh              read-only survey (safe, no root needed for most of it)
#   sudo ./fw-probe.sh --write-test   additionally answer Q1/Q2/Q4 by writing and reverting
#
# --write-test briefly changes RAPL power limits and fan duty, then restores them.
# It is safe but it is not a no-op. Read the script before running it as root.

set -uo pipefail

WRITE_TEST=0
[[ "${1:-}" == "--write-test" ]] && WRITE_TEST=1

bold() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
kv()   { printf '  %-34s %s\n' "$1" "${2:-<absent>}"; }
rd()   { cat "$1" 2>/dev/null; }

# Locate the cros_ec hwmon node by name rather than assuming an index — hwmon
# numbering is not stable across boots.
find_hwmon() {
    local want=$1 d
    for d in /sys/class/hwmon/hwmon*; do
        [[ "$(rd "$d/name")" == "$want" ]] && { echo "$d"; return 0; }
    done
    return 1
}

EC_HWMON=$(find_hwmon cros_ec) || EC_HWMON=""

bold "Machine"
for f in sys_vendor product_name board_name bios_version; do
    kv "$f" "$(rd /sys/class/dmi/id/$f)"
done
kv "cpu"     "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
kv "kernel"  "$(uname -r)"
kv "os"      "$(. /etc/os-release && echo "$PRETTY_NAME")"

bold "Embedded controller"
kv "/dev/cros_ec" "$([[ -e /dev/cros_ec ]] && stat -c '%A %U:%G' /dev/cros_ec)"
kv "cros_ec hwmon node" "${EC_HWMON:-<not found>}"
kv "loaded modules" "$(lsmod | awk '/^cros_|^leds_cros/ {printf "%s ", $1}')"

bold "Fan (hwmon)"
if [[ -n "$EC_HWMON" ]]; then
    for f in pwm1_enable pwm1 fan1_input fan1_target; do kv "$f" "$(rd "$EC_HWMON/$f")"; done
    echo
    for i in 1 2 3 4 5 6 7 8; do
        [[ -f "$EC_HWMON/temp${i}_input" ]] || continue
        printf '  temp%-2s %-22s %6.2f C   (max %s crit %s)\n' \
            "$i" "$(rd "$EC_HWMON/temp${i}_label")" \
            "$(echo "$(rd "$EC_HWMON/temp${i}_input")/1000" | bc -l 2>/dev/null || echo 0)" \
            "$(rd "$EC_HWMON/temp${i}_max")" "$(rd "$EC_HWMON/temp${i}_crit")"
    done
else
    echo "  cros_ec hwmon not present — fan control unavailable via sysfs (ADR 0004 fallback)"
fi

bold "Power / performance"
kv "platform_profile"         "$(rd /sys/firmware/acpi/platform_profile)"
kv "platform_profile_choices" "$(rd /sys/firmware/acpi/platform_profile_choices)"
kv "scaling_driver"           "$(rd /sys/devices/system/cpu/cpufreq/policy0/scaling_driver)"
kv "EPP now"                  "$(rd /sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference)"
kv "EPP choices"              "$(rd /sys/devices/system/cpu/cpufreq/policy0/energy_performance_available_preferences)"
kv "power-profiles-daemon"    "$(systemctl is-active power-profiles-daemon 2>/dev/null)"
kv "PPD active profile"       "$(powerprofilesctl get 2>/dev/null)"
kv "tlp"                      "$(systemctl is-active tlp 2>/dev/null)"

bold "RAPL"
for zone in /sys/class/powercap/intel-rapl:0 /sys/class/powercap/intel-rapl-mmio:0; do
    [[ -d "$zone" ]] || { kv "$(basename "$zone")" "<absent>"; continue; }
    printf '  %s (%s) enabled=%s\n' "$(basename "$zone")" "$(rd "$zone/name")" "$(rd "$zone/enabled")"
    for c in 0 1 2; do
        p="$zone/constraint_${c}_power_limit_uw"
        [[ -f "$p" ]] || continue
        maxp=$(rd "$zone/constraint_${c}_max_power_uw")
        # max_power_uw is the ceiling the PLATFORM declares. A limit set above it means
        # that constraint is not actually constraining anything -- flag it loudly, because
        # the raw limit value reads as a scary-large number that means the opposite.
        note=""
        if [[ -n "$maxp" && "$maxp" != "0" ]] && (( $(rd "$p") > maxp )); then
            note="  <-- ABOVE declared max (${maxp} uW): constraint inactive"
        fi
        printf '    c%s %-12s limit=%4s W  max=%4s W  window=%-10s mode=%s%s\n' \
            "$c" "$(rd "$zone/constraint_${c}_name")" \
            "$(( $(rd "$p") / 1000000 ))" \
            "$([[ -n "$maxp" && "$maxp" != "0" ]] && echo $(( maxp / 1000000 )) || echo '-')" \
            "$(rd "$zone/constraint_${c}_time_window_us")us" "$(stat -c %a "$p")" "$note"
    done
done

bold "Battery / charge control"
BAT=/sys/class/power_supply/BAT1
kv "charge_control_end_threshold" "$(rd $BAT/charge_control_end_threshold)"
kv "extensions/"                  "$(ls $BAT/extensions 2>/dev/null | tr '\n' ' ')"
kv "capacity"                     "$(rd $BAT/capacity)%"
kv "status"                       "$(rd $BAT/status)"
kv "cycle_count"                  "$(rd $BAT/cycle_count)"
echo "  dmesg (charge):"
dmesg 2>/dev/null | grep -i 'charge.control\|cros_charge' | tail -5 | sed 's/^/    /' \
    || echo "    <needs root to read dmesg>"

bold "LEDs"
ls /sys/class/leds/ | grep -i chromeos | sed 's/^/  /'

# ---------------------------------------------------------------------------
if [[ $WRITE_TEST -eq 0 ]]; then
    bold "Write tests"
    echo "  skipped — re-run as: sudo $0 --write-test"
    exit 0
fi

[[ $EUID -eq 0 ]] || { echo "ERROR: --write-test requires root" >&2; exit 1; }

# Everything below mutates hardware state. Register restoration on EVERY exit path
# (normal, error, Ctrl-C, SIGTERM) before touching anything -- this is ADR 0006's
# principle applied to the probe itself. Globals are populated as we go, so the trap
# is a no-op until there is actually something to undo.
RESTORE_PWM_ENABLE=""
RESTORE_RAPL_PATH=""
RESTORE_RAPL_VALUE=""

restore_all() {
    local rc=$?
    if [[ -n "$RESTORE_RAPL_PATH" && -n "$RESTORE_RAPL_VALUE" ]]; then
        echo "$RESTORE_RAPL_VALUE" > "$RESTORE_RAPL_PATH" 2>/dev/null
        printf '  [restore] %s -> %s W\n' "$(basename "$(dirname "$RESTORE_RAPL_PATH")")" \
            "$(( RESTORE_RAPL_VALUE / 1000000 ))"
    fi
    if [[ -n "$RESTORE_PWM_ENABLE" && -n "$EC_HWMON" ]]; then
        echo "$RESTORE_PWM_ENABLE" > "$EC_HWMON/pwm1_enable" 2>/dev/null
        printf '  [restore] pwm1_enable -> %s (EC automatic)\n' \
            "$(rd "$EC_HWMON/pwm1_enable")"
    fi
    return $rc
}
trap restore_all EXIT INT TERM

bold "Q1 — do RAPL writes stick?"
# Test constraint_0 (PL1) -- that is the knob the product actually drives, and on this
# platform the MMIO zone is the one with a real value (see baseline Q2).
for zone in /sys/class/powercap/intel-rapl-mmio:0 /sys/class/powercap/intel-rapl:0; do
    p="$zone/constraint_0_power_limit_uw"       # long_term / PL1
    [[ -f "$p" ]] || continue
    orig=$(rd "$p"); target=$(( orig - 5000000 ))   # 5 W below current
    if (( target < 10000000 )); then target=10000000; fi   # never below 10 W
    RESTORE_RAPL_PATH="$p"; RESTORE_RAPL_VALUE="$orig"
    if ! echo "$target" > "$p" 2>/dev/null; then
        RESTORE_RAPL_PATH=""; RESTORE_RAPL_VALUE=""
        printf '  %-22s WRITE REJECTED (EACCES/EIO) — locked\n' "$(basename "$zone")"; continue
    fi
    sleep 2
    now=$(rd "$p")
    if [[ "$now" == "$target" ]]; then
        printf '  %-22s STICKS  (%sW -> %sW, held 2s)\n' \
            "$(basename "$zone")" "$((orig/1000000))" "$((now/1000000))"
    else
        printf '  %-22s REVERTED (wrote %sW, read back %sW) — firmware override\n' \
            "$(basename "$zone")" "$((target/1000000))" "$((now/1000000))"
    fi
    echo "$orig" > "$p" 2>/dev/null
    printf '  %-22s restored to %sW (read back %sW)\n' "" "$((orig/1000000))" \
        "$(( $(rd "$p") / 1000000 ))"
    RESTORE_RAPL_PATH=""; RESTORE_RAPL_VALUE=""
done

bold "Q4 — does writing pwm1 move the fan?"
if [[ -z "$EC_HWMON" ]]; then
    echo "  no cros_ec hwmon — skipped"
else
    orig_en=$(rd "$EC_HWMON/pwm1_enable")
    echo "  baseline: pwm1_enable=$orig_en rpm=$(rd "$EC_HWMON/fan1_input")"
    RESTORE_PWM_ENABLE="$orig_en"
    # Spin UP, never down — a stuck-high fan is safe, a stuck-low fan is not (ADR 0006).
    echo 1   > "$EC_HWMON/pwm1_enable"
    echo 160 > "$EC_HWMON/pwm1"
    echo "  set manual duty 160/255, waiting 6s..."
    sleep 6
    echo "  under manual: rpm=$(rd "$EC_HWMON/fan1_input") target=$(rd "$EC_HWMON/fan1_target")"
    echo "$orig_en" > "$EC_HWMON/pwm1_enable"
    RESTORE_PWM_ENABLE=""
    sleep 3
    echo "  restored pwm1_enable=$(rd "$EC_HWMON/pwm1_enable") rpm=$(rd "$EC_HWMON/fan1_input")"
    echo "  -> if RPM rose then fell, manual control works and the EC reclaims cleanly."
fi
