# Hardware baseline

Captured 2026-08-18 from the target machine. Re-generate with `scripts/fw-probe.sh`.

## Machine

| | |
|---|---|
| Vendor / product | Framework — `Laptop 13 Pro (Intel Core Ultra Series 3)` |
| Board | `FRANMJCP07` |
| BIOS | `03.02` |
| CPU | Intel Core Ultra X7 358H, 16 logical CPUs |
| OS | Ubuntu 24.04.4 LTS |
| Kernel | 7.0.0-29-generic |

## Confirmed present

### Embedded controller
`/dev/cros_ec` exists (`crw------- root:root`).
EC firmware **`sakura-3.0.2-cf48815`** (built 2026-05-26), board version 12,
Nuvoton `npcx9m3f`. Loaded modules:
`cros_ec`, `cros_ec_lpcs`, `cros_ec_proto`, `cros_ec_dev`, `cros_ec_chardev`,
`cros_ec_sysfs`, `cros_ec_debugfs`, `cros_ec_hwmon`, `cros_charge_control`,
`cros_kbd_led_backlight`, `leds_cros_ec`, `gpio_cros_ec`.

### Fan + EC thermal — `hwmon11` (`cros_ec`)
Standard hwmon interface, **no `ectool` required**:

| Attribute | Value at capture | Notes |
|---|---|---|
| `pwm1_enable` | `2` | 2 = EC automatic, 1 = manual |
| `pwm1` | `0` | 0–255 duty when manual |
| `fan1_input` | `0` | RPM, fan idle |
| `fan1_target` | `0` | |

EC temperature sensors:

| Sensor | Label | Reading |
|---|---|---|
| `temp1` | `local_f75397@4c` | 36.85 °C |
| `temp2` | `cpu_f75303@4d` | 36.85 °C |
| `temp3` | `battery_temp@b` | 30.85 °C |
| `temp4` | `ddr_f75303@4d` | 36.85 °C |
| `temp5` | `peci-temp` | 43.85 °C — CPU package, primary curve input |

Also `hwmon1` (`acpi_fan`) exposes `fan1_input` / `fan1_target` / `power1_input` (read-only view).
`hwmon10` is `coretemp` (per-core die temps).

### Power / performance
- `/sys/firmware/acpi/platform_profile` → `balanced`; choices `low-power balanced performance`
- `scaling_driver` → `intel_pstate`
- EPP choices → `default performance balance_performance balance_power power`
- **power-profiles-daemon is active**, driving *both* `intel_pstate` and `platform_profile`

### RAPL — `/sys/class/powercap/`
`intel-rapl:0` (`package-0`), `enabled=1`, all constraints mode `644` (root-writable):

| Constraint | `intel-rapl:0` limit | `intel-rapl-mmio:0` limit | `max_power_uw` | Window |
|---|---|---|---|---|
| 0 `long_term` (PL1) | **200 W** | **25 W** | **25 W** | 31.98 s |
| 1 `short_term` (PL2) | 60 W | 60 W | 0 | 976 us |
| 2 `peak_power` (PL4) | 175 W | 175 W | 0 | — |

**Read this table carefully — the raw numbers mislead.**

`max_power_uw` is the ceiling *the platform declares* for the constraint, and it is **25 W on
both zones**. That is the authoritative sustained figure. The MSR zone's 200 W `long_term`
is a limit set *above* the declared maximum, which is the signature of a constraint that is
not constraining anything — firmware parks an unconstrained value there because real
governance happens through MMIO and the EC. **The machine's sustained power budget is 25 W,
not 200 W.**

Three separate misreadings to avoid:

1. **A RAPL limit is a ceiling, not a draw or a target.** It caps a rolling average; it says
   nothing about actual consumption.
2. **200 W = "PL1 effectively disabled"**, not "PL1 is 200 W". Confirmed by `max_power_uw`.
3. **`peak_power` (PL4) is not a thermal number.** It is an instantaneous current-spike
   ceiling protecting the VRM and battery on a microsecond scale, and never becomes sustained
   heat. Note that its time window is empty while PL1's is ~32 s and PL2's is ~1 ms — the
   timescales are what give these numbers their meaning.

Effective envelope: **25 W sustained / 60 W burst**, consistent with a 13" chassis and with
the part's 1.9 GHz base / 4.7 GHz max frequency.

Also present: `intel-rapl:1` (`psys`).

### Telemetry needs root
`energy_uj` is mode `0400` root-only on both zones — this is the mitigation for
**PLATYPUS / CVE-2020-8694**, where RAPL energy readings were used as a side channel to
recover AES keys. Package-power telemetry therefore *cannot* be read by an unprivileged GUI.
This is independent reinforcement for the daemon split in
[ADR 0003](adr/0003-privileged-daemon-split.md): the GUI gets power readings over D-Bus or
not at all.

`max_energy_range_uj` = 262143328850 — the counter wraps, so deltas must handle rollover.

### Sensor caveat
Every `temp*_max` reads `-273150` (0 K — i.e. unset). **Only `temp*_crit` is usable.**
Observed: cpu 87.85 °C, ddr 86.85 °C, local 87.85 °C, battery 49.85 °C, peci 119.85 °C.

### LEDs
`chromeos::kbd_backlight`, `chromeos:multicolor:charging`, `chromeos:white:power`.

## Confirmed absent

- **No `charge_control_end_threshold` on `BAT1`.** `cros_charge_control` is loaded but
  `/sys/class/power_supply/BAT1/extensions/` is **empty** — the driver has not registered
  against the battery. See [open question Q3](#open-questions).
- No `ectool`, `framework_tool`, `ryzenadj`, or `tlp` installed. `powerprofilesctl` is present.
- No discrete GPU (FW13).

## Open questions

These are unresolved by read-only probing and gate specific milestones.

**Q1 — Do RAPL writes actually stick?** *(answered — YES)*
Both zones accept writes and hold them:

```
intel-rapl-mmio:0   STICKS  (25W -> 20W, held 2s)   restored to 25W
intel-rapl:0        STICKS  (200W -> 195W, held 2s) restored to 200W
```

**No lock bit. M4 is viable.** Caveat: this proves writes are not *rejected*; it does not yet
prove the limit *governs* power draw. Effectiveness still needs a load test — see Q6.

**Q2 — `intel-rapl` (MSR) vs `intel-rapl-mmio` — which governs?** *(answered)*
Both declare `max_power_uw` = 25 W, but only the MMIO zone has `long_term` actually *set* to
25 W; the MSR zone parks 200 W above its own declared maximum. **Target `intel-rapl-mmio:0`
for PL1.** Remaining work is confirmation under load, not identification.
*Gates M4.*

**Q3 — Why is charge control not registered?** *(answered)*
None of the original hypotheses. The driver **deliberately refuses to load**:

```
[2.860075] cros-charge-control cros-charge-control.6.auto:
           Framework charge control detected, preventing load
```

Framework's EC implements a *custom* charge control command alongside the standard CrOS EC
one. Both work, but the custom one can override the standard one, and the UEFI setup screen's
battery limit uses the custom one — so upstream declines to load rather than race the
firmware. The supported override is a module parameter, present on this kernel:

```
/sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control   # bool, 0644, = N
```

Resolved in [ADR 0008](adr/0008-charge-limit-via-module-parameter.md): enable the parameter,
use standard sysfs, and treat the UEFI battery limit as off-limits.

**Q4 — Does writing `pwm1` actually move the fan?** *(answered — YES)*

```
baseline:     pwm1_enable=2  rpm=0
manual 160:   rpm=4681  (duty 63%)
restored:     pwm1_enable=2  rpm=0
```

Manual control works **and the EC reclaims cleanly** — the latter is what all of
[ADR 0006](adr/0006-fail-safe-fan-control.md)'s safety machinery depends on. **M3 is viable.**

Implementation note: `fan1_target` stayed `0` while under manual control. Read `fan1_input`
for actual RPM; do not trust `fan1_target` as a feedback signal.
Scale reference: 63% duty ≈ 4681 RPM, useful for curve design.

**Q5 — What is the true sustained limit?** *(answered)*
**25 W PL1 / 60 W PL2**, confirmed by `max_power_uw` = 25 W on both zones. The MSR zone's
200 W is not a real limit and the 175 W `peak_power` is a microsecond-scale current ceiling,
not a thermal budget. Remaining work is only to pick sensible per-profile values below 25 W
(e.g. 15 W quiet / 25 W balanced) and verify them under load via `energy_uj` deltas.
*Informs M4; no longer blocking.*

**Q6 — Does PL1 actually govern sustained draw?** *(answered — YES, tightly)*
Measured with `scripts/q6-pl1-load-test.sh` (`stress-ng --cpu 16 --cpu-method matrixprod`,
steady state = mean of the second 30 s of each 60 s sampling run):

| PL1 setpoint | Sustained draw | Error | CPU (peci) | Fan |
|---|---|---|---|---|
| 25 W (stock) | **24.67 W** | −1.3% | 76.8 °C | ~3100 rpm |
| 15 W | **14.68 W** | −2.1% | 64.8 °C | ~2925 rpm |
| idle | 1.77 W | — | 43.9 °C | 0 rpm |

**PL1 is a real control, regulated to within ~2% of setpoint.** Intel Dynamic Tuning is not
arbitrating it away. M4 ships as genuine functionality.

Two secondary findings worth carrying into design:

**10 W buys 12 °C.** This is the empirical basis for the profile values in the plan, and it
substantiates the claim in [ADR 0007](adr/0007-no-undervolting.md) that power limiting
delivers the thermal and acoustic outcome people actually want from undervolting.

**The stock EC fan curve has real headroom.** Dropping 12 °C moved the fan only ~7%
(3100 → 2925 rpm), and the machine was already at 2900 rpm at 64.8 °C while sitting at
0 rpm at 43.9 °C. Later observation during M1b narrows the knee further: **0 rpm at 44.9 °C
but ~2020 rpm at 53.9 °C**, so the fan starts somewhere in 45–54 °C and is already at two
thirds of its loaded speed by 54 °C. So the EC curve ramps steeply somewhere between ~45 °C and ~65 °C and then
flattens. A custom curve that is less aggressive in the 55–70 °C band is where M3's audible
benefit lives — see M3 notes.

*Answered. M4 unblocked.*

**Measurement note:** the first sample of the unconstrained run read 29.03 W — above the
25 W limit — because at t+25 s into load the ~32 s PL1 averaging window had not yet closed.
This is why the verdict averages only the second half of each run. Any future power
measurement must respect that window or it will read turbo as steady state.
