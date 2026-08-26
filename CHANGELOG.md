# Changelog

## 0.1.0 — 2026-08-26

First public preview. Fan curves, sustained power limits, a battery charge limit and
performance profiles for the Framework Laptop 13 (Intel Core Ultra), on Ubuntu.

Everything below was measured on the target machine — board `FRANMJCP07`, BIOS 03.02,
EC `sakura-3.0.2`, Ubuntu 24.04, kernel 7.0. Where something is implemented but not
demonstrated, it says so.

### The finding most worth passing upstream

**On Framework hardware the standard CrOS EC charge-control interface is silently
inert, and forcing it to bind makes that worse rather than better.**

The kernel's `cros_charge-control` driver deliberately refuses to bind on Framework
laptops, because Framework's EC implements a custom charge-control command alongside the
standard one and the custom one can override it. It offers
`probe_with_fwk_charge_control=1` as an escape hatch, conditioned on the user "not going
to use the custom command".

That condition is not satisfiable. The custom command is not a user choice — it is what
the EC firmware runs, whether or not anything configures it. Forcing the binding produces
a `charge_control_end_threshold` attribute that accepts a value, reads it back, survives
suspend and reboot, and **governs nothing**:

| | |
|---|---|
| `charge_control_end_threshold` | 80 |
| Custom EC command reports | `max=100` |
| Result | charged 88% → 93% → 100%, `status=Charging` throughout |

Two independent limits exist and sysfs is wired to the losing one. The driver's refusal
to bind is a correct verdict about this hardware.

fw-helper now drives `EC_CMD_CHARGE_LIMIT_CONTROL` (`0x3E03`) over `/dev/cros_ec`
instead. Verified: charged from below the limit on AC and stopped at exactly 80% —
`status=Not charging`, `current_now=0`, `charge_now` 3 859 000 of `charge_full` 4 821 000.

See `docs/adr/0012-charge-limit-via-custom-ec-command.md`, and
`docs/adr/0008-charge-limit-via-module-parameter.md` for the approach it replaces and why
it failed.

### Works, verified on hardware

- **Fan control** — manual duty or a temperature curve, with a graphical editor. Never
  runs the fan slower than firmware would at the same temperature on its ascending
  branch, and hands control back to the EC on exit, crash, signal, suspend and watchdog
  timeout.
- **Battery charge limit** — verified to actually stop charging (above).
- **Power limits (PL1)** — a 15 W setpoint held 15.02 W sustained, +0.1%.
- **Performance profiles** — layered over power-profiles-daemon rather than replacing it,
  so the GNOME power slider keeps working and stays in sync. Custom profiles in
  `/etc/fw-helper/profiles.d/`.
- **Live telemetry** — temperatures, fan RPM, package and whole-machine power, battery.
- **Capability detection** — every knob reports available, or a reason it is not.
- **Packaging** — `.deb`, systemd unit, D-Bus and polkit policy. `apt remove` returns the
  fan to the EC.

### Known issues

- **The power-limit slider stops at 25 W, and 25 W is not a hardware limit.** It comes
  from `constraint_0_max_power_uw`, which does not bind firmware: with PL1 set to 35 W the
  package drew **31.94 W sustained**. The practical ceiling appears to be ~31 W, reached
  at 87 °C with the EC's fan at its own maximum. Raising the clamp needs an ADR and is not
  in this release.
- **A manually set power limit is discarded when a profile re-applies.** AC/battery
  transitions re-apply a profile, and a profile carries its own PL1, so a hand-set value
  silently reverts with no notification.
- **`fw-helperd` can lose a boot race with power-profiles-daemon.** PPD is D-Bus
  activatable, so our own probe triggers its activation; on a busy boot that took 26.9 s
  against a ~25 s timeout, and the daemon then wrote `platform_profile` directly — the
  path ADR 0005 forbids — for the whole session. Restarting the service fixes it until the
  next boot.
- **Fan floor observations only ever rise**, so a single anomalous sample is sticky and
  nothing undoes it. Errs loud rather than quiet.
- **Whether PL1 governs below 15 W is untested.** 15 W is verified; lower setpoints are
  accepted but have not been shown to hold under load.
- **PL2 is untouched** — its `max_power_uw` reads 0, which is "unset", not "no power".
- **The curve's control sensor is not configurable** — it is always `peci-temp`.
- **The panic path is implemented and unit-tested but has never fired live.**
- **Undervolting is impossible on this hardware** and deliberately absent — the
  Plundervolt mitigation locks the MSR. See ADR 0007.

### Notes for anyone testing this

- The charge limit needs no opt-in step any more. If you set up an earlier build,
  `/etc/modprobe.d/fw-helper.conf` is now inert and can be removed.
- `scripts/q2-charge-limit-efficacy.sh` will tell you whether a charge limit actually
  works on *your* machine. It refuses to run on battery, or when the battery is already
  above the limit — both conditions under which a broken limit looks healthy.
- Read `docs/hardware-baseline.md` before assuming anything about this board. Four of six
  starting assumptions in this project turned out to be wrong.
