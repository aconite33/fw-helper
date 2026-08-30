# Hardware baseline — Framework Laptop 13, AMD Ryzen AI 300

Captured 2026-08-29. This is the `amd-fw13` fork's baseline; the Intel Pro figures live in
`hardware-baseline.md` and **do not carry over**. Where a number here differs from that
document, this one is what was measured on this board.

Re-generate the survey with `scripts/fw-probe.sh`, the EC answers with
`scripts/probe-ec-amd.c`, and the fan behaviour with `scripts/probe-fan-amd.c`.

## Machine

| | |
|---|---|
| Vendor / product | Framework — `Laptop 13 (AMD Ryzen AI 300 Series)` |
| Board | `FRANMGCP05` |
| BIOS | `03.05` |
| CPU | AMD Ryzen AI 5 340 w/ Radeon 840M |
| EC firmware | **`lilac-3.0.5-9010bdf`** (2025-10-30), Nuvoton `npcx9m3f`, board version 10 |
| OS | Arch Linux |
| Kernel | 7.1.11-arch1-1 |

The EC chip is the same part as the Intel board (`npcx9m3f`); the firmware is a different
build line (`lilac` vs `sakura`). That is why the Framework-custom EC commands port and the
kernel-driver surface does not.

## The headline difference: no fan PWM in sysfs

`cros_ec` hwmon exposes **`fan1_input`, `fan1_target`, `fan1_fault` and four temperatures —
and no `pwm1` or `pwm1_enable` at all.** Every fan write path in the Intel fork targets
attributes that do not exist on this board.

Fan control is still available, but only through **raw EC commands** over `/dev/cros_ec`.
`EC_FEATURE_PWM_FAN` is advertised, and the commands work (measured below). This inverts
ADR 0004's ordering for the fan specifically: sysfs first is not an option here.

## Confirmed present

### EC feature flags

Measured `flags[0]=0x0207E6AE flags[1]=0x00000207`:

| Feature | Bit | Present |
|---|---|---|
| `EC_FEATURE_LIMITED` | 0 | **No** — full command set, not a cut-down EC |
| `EC_FEATURE_PWM_FAN` | 2 | **Yes** |
| `EC_FEATURE_THERMAL` | 10 | **Yes** |

### Fan control over raw EC — verified working

Measured 2026-08-29 with `probe-fan-amd`. Opcodes and parameter structs verified against
`torvalds/linux` `include/linux/platform_data/cros_ec_commands.h`, **not from memory**:

| Command | Id | v0 parameters |
|---|---|---|
| `EC_CMD_PWM_GET_FAN_TARGET_RPM` | `0x0020` | none → `uint32_t rpm` |
| `EC_CMD_PWM_SET_FAN_DUTY` | `0x0024` | `{ uint32_t percent; }` |
| `EC_CMD_THERMAL_AUTO_FAN_CTRL` | `0x0052` | none |

```
baseline (EC owns fan)   rpm 0
manual 40% duty          rpm 3034 -> 4012   (settles ~4000)
manual 70% duty          rpm 5682 -> 6105   (settles ~6200)
released to EC auto      rpm 5313 -> 0 within 2 s
```

**Both halves pass**, which is what ADR 0006 requires before any daemon may take the fan:
duty writes move the fan, *and* the EC reclaims it cleanly and promptly.

Four things worth carrying into the design:

- **Duty is a percentage (0–100) on this interface, not an 8-bit count.** The Intel board's
  0–255 `pwm1` scale and its `DUTY_TOLERANCE` round-trip quantization do not apply. There is
  no read-back path for duty at all here, which is a *bigger* change than the scale: the
  Intel fork verifies its writes by reading `pwm1` back, and that verification is unavailable.
  RPM is the only feedback.
- **`fan1_input` does track reality under manual control.** It reads 0 at idle, so this was
  not a given. The existing telemetry source is sound.
- **`EC_CMD_PWM_GET_FAN_TARGET_RPM` returned 0 throughout, under manual control and after
  release.** This is the Intel board's `fan1_target` trap arriving through a different
  interface. It is not a usable feedback signal — read `fan1_input`.
- **This fan is roughly 20% faster than the Pro's at the same duty**: 70% → 6221 rpm here
  against 5201 rpm at the Intel board's equivalent duty 180/255 (71%). Every fan constant in
  the codebase is now known-wrong for this board, not merely presumed wrong.

### Temperature sensors — `cros_ec` hwmon

Four, not the Intel board's five:

| Sensor | Label | Idle | crit |
|---|---|---|---|
| `temp1` | `local_f75303@4d` | 41.85 °C | 89.85 °C |
| `temp2` | `cpu_f75303@4d` | 42.85 °C | 97.85 °C |
| `temp3` | `ddr_f75303@4d` | 42.85 °C | **79.85 °C** |
| `temp4` | `cpu@4c` | 43.85 °C | 114.85 °C |

Two absences that matter:

- **No `peci-temp`.** `Telemetry::control_temp()` looks for it first and falls back to any
  label containing `cpu`, which lands on `cpu_f75303@4d`. That fallback works but was never
  a deliberate choice for this board — the control sensor needs picking on evidence.
- **No `battery_temp`.** ADR 0011 sized the battery guard around `battery_temp@b` and its
  49.9 °C crit, the lowest limit on the Intel board. **That sensor does not exist here**, so
  the guard as written has nothing to watch. Battery temperature is available from
  `/sys/class/power_supply/BAT1` via `hwmon3` — different path, needs re-plumbing.

`ddr_f75303@4d` crit is **79.85 °C**, seven degrees lower than the Intel board's 86.8 °C, so
this board has *less* thermal headroom on DDR, not more.

Elsewhere: `k10temp` (`Tctl`) is the AMD package sensor, `amdgpu` `edge` the iGPU, plus two
`spd5118` DIMM sensors at 85 °C crit and `nvme` at 84.85 °C.

### Battery charge limit — the one thing that ports unchanged

Framework's custom command answers on this firmware:

```
CHARGE_LIMIT_CONTROL (0x3E03) get -> max=100% min=0%
```

That is the *identical* reading the Intel board gave before its limit was set, from the same
command with the same wire format. ADR 0012 should port with no change beyond the
`battery_temp` plumbing. **Not yet verified to actually stop charging on this board** — the
Intel fork's hardest-won lesson is that read-back is not efficacy, and
`scripts/q2-charge-limit-efficacy.sh` is the check that counts.

`cros_charge_control` is loaded but `BAT1/extensions/` is empty and there is no
`charge_control_end_threshold` — same posture as the Intel board, and the same reason to
ignore sysfs for this.

## Confirmed absent

### No usable RAPL — M4 cannot port

- `intel-rapl:0` exists but reads **`enabled=0`**, and `energy_uj` is permission-denied.
- **There is no `intel-rapl-mmio:0` zone at all** — the zone the Intel fork drives for PL1.
- No `constraint_*` files: the survey found only `enabled`, `energy_uj`,
  `max_energy_range_uj`, `name`.

Framework's EC command set has **no PPT or SOC power command** either (checked against
`framework-system`'s `EcCommands` enum: the nearest entries are `ChargeCurrentLimit 0x00A1`,
which is charger *input* current, and `GetApThrottleStatus 0x3E22`, read-only). So there is
no EC route to package power on this board.

On AMD the power limits are moved by **`amd-pmf`**, which owns `platform_profile` and applies
STAPM/SPPT/FPPT through its static-slider control. **M4 and M5 therefore collapse into one
mechanism here**: power control is available through profiles, not as a watts setpoint.
`ryzenadj`-style SMU mailbox access is the only route to a continuous limit, and is
undocumented and model-specific — deliberately not attempted.

### No power-profiles-daemon

Not installed; `powerprofilesctl` absent, the unit does not exist. `platform_profile` is owned
by `amd-pmf` (`/sys/class/platform-profile/platform-profile-0`, `name=amd-pmf`), offering
`low-power balanced performance`.

ADR 0005 exists to avoid last-writer-wins against the GNOME power slider *via PPD*. With no
PPD running there is no such race, so this fork writes `platform_profile` directly — while
detecting PPD at runtime and deferring to it if it ever appears. The Intel fork's PPD boot-race
defect does not apply here.

`scaling_driver` is `amd-pstate-epp`; EPP choices are
`default performance balance_performance balance_power power custom`.

### Other

No `ectool`, `framework_tool`, or `ryzenadj` installed. No discrete GPU.

### Duty → RPM, measured

Full sweep 2026-08-29, descending from 100% so the fan never had to start from rest below
stiction, 7 s settling per point, machine idle. Temperature drifted 47.9 → 44.9 °C across the
run — falling, because the fan was cooling the machine.

| Duty % | 0 | 4 | 6 | 8 | 10 | 12 | 14 | 16 | 18 | 20 | 25 | 30 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| RPM | 0 | **0** | **0** | **0** | 967 | 1221 | 1456 | 1689 | 1920 | 2155 | 2671 | 3160 |

| Duty % | 35 | 40 | 45 | 50 | 60 | 70 | 80 | 90 | 100 |
|---|---|---|---|---|---|---|---|---|---|
| RPM | 3614 | 4045 | 4468 | 4890 | 5585 | 6261 | 6779 | 7336 | **7864** |

**The fan stalls between duty 8% and 10%.** Duty 10 turns it at 967 rpm; duty 8 leaves it
stopped. Any duty in 1–9 is a stopped fan wearing a costume and must be refused, not accepted
and silently ignored. The Intel board stalled between its 8-bit duty 20 and 30 — 7.8% and
11.8% — so both boards stall in the same 8–12% band despite the different fan.

**This measures the STALL point, not the START point** — the sweep descends, so the fan was
already turning at every point. The two differ, and the gap was measured separately below.

### Break-away: the fan will not start at the duty it will sustain

Measured 2026-08-29 at 44.9 °C with `probe-fan-amd --breakaway`, from a confirmed standstill:

| Duty % | 8 | 9 | 10 | 11 |
|---|---|---|---|---|
| From rest | still at rest | still at rest | **still at rest** | **1098 rpm — starts** |

**Duty 10 sustains rotation at 967 rpm but cannot begin it.** That two-point gap between
stall and break-away is the dangerous band this fork must not put a curve into: a curve
idling at 10 would run correctly all the way down a cooldown, then silently fail to spin up
from cold. A fan that never starts is indistinguishable from a working quiet curve until
something overheats — the exact failure ADR 0006 exists to prevent, arriving through
arithmetic rather than through a crash.

So the minimum usable non-zero duty on this board is **11%, not 10%**, and the constant that
encodes it must carry the break-away number.

`STICTION_DUTY` should be set **above** the measured 11, not equal to it. This is a single
observation, taken at one temperature, with the fan already warm; bearing drag rises when
cold and as dust accumulates, and the failure direction is silent. **13% is the proposed
value** — 11 measured plus two points of margin, costing roughly 235 rpm of minimum speed.
Worth re-measuring from cold before that is treated as settled.

> Note for anyone reading the Intel baseline alongside this: that board's `STICTION_DUTY`
> (8-bit 30) was derived from a **descending** sweep too — duty 20 → 0 rpm, duty 30 → 1107
> rpm. If break-away exceeds stall there as it does here, that constant may sit below the
> Intel board's true starting duty. Unverified, and not this fork's problem to fix, but it
> is the same latent bug.

**The curve is concave**, and steeply so. Slope in rpm per duty point:

| Duty band | 10–20 | 20–30 | 30–50 | 50–70 | 70–100 |
|---|---|---|---|---|---|
| rpm per point | ~117 | ~100 | ~87 | ~69 | ~53 |

The bottom of the range is **more than twice as responsive** as the top. This is the Intel
board's lesson reproduced on different hardware, and it bites in the same direction: a line
fitted through the high points (80% and 100%) predicts **2982 rpm at duty 10**, against a
measured **967** — a threefold overestimate. Inverting that fit to ask "what duty matches
firmware's RPM?" would return a duty roughly three times too low, i.e. a floor *below*
firmware, which is the one direction that is not safe. **Interpolate the table. Do not fit
a line to it.**

For scale against the Intel board: 7864 rpm at full duty here, and 6261 rpm at 70% against
its 5201 rpm at duty 180/255 (71%) — the ~20% margin holds across the range.

## Not yet measured

Everything below was measured on the Intel board and is **unverified here**. None of it should
be assumed:

- **Duty → RPM table and the stiction point.** The Intel board stalled below 8-bit duty 30
  (≈12%); this fan is faster and its stiction point is unknown. `probe-fan-amd --sweep`.
- **The EC's own fan curve, and whether it is hysteretic.** The Intel board ran duty 0 heating
  and 92 cooling at the same 61.9 °C. Unknown here, and it decides where a custom curve wins.
  Harder to measure on this board: with no `pwm1`, firmware's own duty cannot be read while
  the EC owns the fan, so the curve can only be observed in **RPM**, and the firmware floor
  must be reconstructed by inverting the duty→RPM table — the interpolation error the Intel
  fork had escaped.
- **Peak temperature in ordinary use**, and therefore any threshold derived from it.
- **Whether the charge limit actually stops charging.**
- **Whether profiles measurably move power**, which is now the only power control.
