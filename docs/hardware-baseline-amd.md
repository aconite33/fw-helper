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
