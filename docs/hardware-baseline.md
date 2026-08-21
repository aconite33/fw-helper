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

> **The index moved.** On 2026-08-21 the same node came up as `hwmon9`, not `hwmon11`.
> This was the predicted instability, now observed rather than assumed — always resolve
> `cros_ec` by its `name` file, never by index.

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

#### How manual control actually behaves

Measured 2026-08-21 driving `FanControl` (M3) as root, spinning up and releasing. Three of
these were assumptions before this run, and two of them were wrong.

- **`pwm1` is not writable while the EC owns the fan.** Writing it with `pwm1_enable=2`
  fails with `EOPNOTSUPP` (errno 95) — it is not silently ignored, it is refused. So the
  duty *cannot* be pre-loaded before taking control, and the window between
  `pwm1_enable=1` and the first duty write cannot be closed from userspace. Keep that
  window to adjacent statements.
- **Duty round-trips through whole percent.** The EC stores a percentage, so an 8-bit
  count comes back up to one count away. Verification must use a tolerance, not equality
  — unlike the charge limit, where exact read-back is correct:

  | Requested | 77 | 90 | 100 | 128 | 150 | 180 | 200 | 230 | 255 |
  |---|---|---|---|---|---|---|---|---|---|
  | Observed | 77 | 89 | 99 | 128 | 150 | 181 | 199 | 230 | 255 |

  Every point matches `round(round(d / 2.55) × 2.55)`. Max observed error ±1 count.
- **Under EC control, `pwm1` reports firmware's own duty.** Corrected 2026-08-21: an
  earlier note here said `pwm1` "goes to 0" a few seconds after firmware reclaims the
  fan. It goes to *firmware's current duty*, which is 0 only because the machine was
  idle. Measured under load at 68.8 °C with `pwm1_enable=2`: `pwm1` read **64** and the
  fan turned at 2302 rpm — against a table where duty 65 gives 2296 rpm. It does still
  take a few seconds to stop reflecting the duty *we* last wrote, so it is not a signal
  of *who owns* the fan; read `pwm1_enable` for that.

  **This is worth acting on.** `FirmwareFloor` currently reconstructs firmware's duty by
  inverting an RPM table, composing two measured tables and inheriting the interpolation
  error of both. If `pwm1` can simply be read while the EC owns the fan, the floor can be
  *measured* rather than modelled, and the knee gap closes without needing the learned
  observation mechanism. Confirm across the temperature range before changing working
  safety code.
- **The EC reclaims cleanly and promptly**, confirming Q4 on a second occasion: fan went
  to 0 rpm within 4 s of release, at 41 °C.

#### The EC's own curve: measured, and hysteretic

Read directly from `pwm1` while `pwm1_enable=2` (see above). Measured 2026-08-21.

| Temp | EC duty, **heating** | EC duty, **cooling** |
|---|---|---|
| 44.9 °C | — | 51 |
| 48.9 °C | — | 66 |
| 54.9 °C | **0** | **82** |
| 58.9 °C | **0** | **87** |
| 61.9 °C | **0** | **92** |
| 64.8 °C | **0** | — |
| 66.8 °C | 59 | — |

**The hysteresis is enormous.** At 61.9 °C firmware runs the fan off or at duty 92
depending only on which way the temperature is going, and it does not stop the fan until
below 44.9 °C. Climbing from cold it kept the fan entirely off past 64.8 °C.

The fan-start point is **not a fixed temperature**: 66.8 °C on a 16-core step, 72.8 °C on
a gentler 2-core climb. Note the direction — the *slower* ramp started *later*, which is
the opposite of what response lag predicts, so firmware is likely triggering on something
other than instantaneous `peci-temp`.

Consequences, both acted on in [ADR 0011](adr/0011-quiet-is-a-legitimate-choice.md):

- The `EC_CURVE` points recorded earlier in this document (2020 rpm at 53.9 °C, 2925 at
  64.8 °C) are **descending-branch** measurements, taken under sustained load. They are
  not what firmware does at those temperatures while heating, which is nothing at all.
- "Never quieter than firmware" needs a branch named, or it means nothing.

#### Thermal limits, and what protects what

| Sensor | crit | Self-protecting? |
|---|---|---|
| `coretemp` package + every core | **100.0 °C** (Tjmax) | **Yes** — the CPU throttles |
| `peci-temp` | 119.8 °C | Reports *above* Tjmax; not a usable limit |
| `local_f75397@4c`, `cpu_f75303@4d` | 87.8 °C | No |
| `ddr_f75303@4d` | 86.8 °C | No |
| `battery_temp@b` | **49.9 °C** | **No** — and it is the lowest on the board |

**Battery temperature barely moves under CPU load.** Measured across five minutes of
16-core load with firmware driving the fan, on battery power: `battery_temp` went
**31.9 → 33.9 °C** while `peci-temp` went 40.9 → 78.8 °C. That is 16 °C of headroom below
its 49.9 °C crit. It lags heavily and was still rising during the cooldown. During that
cooldown, with the fan still running hard, it fell to **26.9 °C — below its idle
baseline** — so airflow does cool it. What is *not* measured is a long run with the fan
held low, which is what a user-authored curve will allow.

**The CPU protects itself at 100 °C.** Constraining the fan costs performance, not
hardware. The components with no protection of their own are the battery above all, then
the board and DDR sensors — nothing currently watches any of them.

**Peak temperature in ordinary use is higher than M0 suggested.** M0's PL1 test recorded
76.8 °C under sustained full load. Measured under ordinary multi-core load with firmware
driving the fan, `peci-temp` reached **92.8 °C**, firmware choosing duty 94/255. Any
threshold set from the 76.8 °C figure is set below normal operation.

#### Duty → RPM, measured

Full sweep 2026-08-21 at ~39 °C, descending from 180 so the fan never had to start
from rest at a duty below stiction, 8 s settling per point. This is the table the
firmware-floor clamp inverts.

| Duty | 0 | 20 | 30 | 40 | 50 | 65 | 77 | 90 | 100 | 120 | 150 | 180 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| RPM | 0 | **0** | 1107 | 1512 | 1879 | 2296 | 2693 | 3052 | 3355 | 3840 | 4551 | 5201 |

Two things matter here and neither was guessable:

- **Stiction sits between 20 and 30.** Duty 20 leaves the fan stopped; duty 30 turns it
  at 1107 rpm. Any duty in 1–29 is a stopped fan wearing a costume, which is why
  `STICTION_DUTY` exists and why a "quiet" setting of 25 must be refused rather than
  accepted and ignored.
- **The curve is concave, not affine.** A linear fit through the three high points
  measured earlier (120/160/181) predicts 1343 rpm at duty 0 and puts 2925 rpm at duty
  77. The real answer is ~85. Extrapolating that fit would have set the floor *below*
  firmware — the one direction that matters. Interpolate within the table; do not fit
  a line to it.

The table is also slightly **optimistic under load**: at 65.8 °C, duty 84 produced
2808 rpm where the table predicts ~2886. Hence the small margin added to every
non-zero floor duty.

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
