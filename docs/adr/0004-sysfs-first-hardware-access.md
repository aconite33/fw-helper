# 0004 — Kernel sysfs first, raw EC commands only as fallback

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

There are two ways to reach the Framework EC:

1. **Kernel drivers exposing standard sysfs** — `cros_ec_hwmon`, `cros_charge_control`,
   `cros_kbd_led_backlight`, `platform_profile`, `intel_pstate`, `powercap`
2. **Raw host commands** over `/dev/cros_ec` — what `ectool` and `framework_tool` do

Existing tools in this space (notably `fw-fanctrl`) were built when option 1 did not exist,
and shell out to `ectool`. Probing the target machine shows that assumption is now outdated:

```
/sys/class/hwmon/hwmon11/          # name = cros_ec
  pwm1  pwm1_enable  fan1_input  fan1_target
  temp1..temp5_input  + _label _max _crit _emergency
```

`pwm1_enable` is `2` (EC automatic) and drops to `1` for manual duty control. This is the
whole fan-control surface, through the standard hwmon ABI, with no external binary.

## Decision

Use kernel sysfs interfaces wherever they exist. Reach for raw `/dev/cros_ec` host commands
**only** where sysfs demonstrably does not work, and isolate that code behind a trait so the
fallback is swappable and testable.

Never shell out to `ectool` or `framework_tool`. If a host command is needed, issue it
directly via `ioctl` (encoding cribbed from `framework_system`).

## Consequences

**Positive**

- **No runtime dependency on `ectool`/`framework_tool`** — neither is packaged in Ubuntu, so
  this removes a build-from-source step from the install instructions.
- The hwmon ABI is stable and documented; `ectool`'s host command numbering is versioned
  against EC firmware and drifts.
- Standard tooling (`sensors`, `fancontrol`, monitoring agents) sees the same values we do.
- Testable: sysfs paths can be faked with a temp directory root, so the fan curve engine
  gets unit tests without hardware.

**Negative**

- Kernel-version-dependent. The hwmon interface needs a reasonably recent kernel; the
  target runs 7.0.0-29, but a broader user base will not. The daemon must degrade
  gracefully and report reduced `Capabilities` ([0003](0003-privileged-daemon-split.md))
  rather than failing to start.
- Some functionality may only ever exist via host commands.

## Known fallback: charge limit

`cros_charge_control` is loaded but `/sys/class/power_supply/BAT1/extensions/` is empty and
no `charge_control_end_threshold` exists — see open question Q3 in the hardware baseline.

Resolution order:

1. Diagnose (`dmesg | grep -i charge`); if it is a driver/firmware bug, prefer fixing or
   reporting it upstream — that helps every Framework user, not just us
2. If unfixable on this board, implement `EC_CMD_CHARGE_CONTROL` directly over `/dev/cros_ec`
3. Prefer sysfs at runtime whenever it *is* present, so the fallback dies naturally as
   kernels and firmware improve

## Alternatives considered

- **Shell out to `ectool` for everything.** Rejected: adds an unpackaged build-from-source
  dependency, costs a process spawn per poll, gives poor error reporting, and re-implements
  what the kernel already exposes.
- **Link `framework_system` and use host commands throughout.** Rejected as the default: it
  bypasses kernel drivers that may be concurrently managing the same hardware, risking
  conflicting writes. Kept as the fallback implementation, which is exactly what it is good at.
