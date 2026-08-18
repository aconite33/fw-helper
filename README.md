# fw-helper

Firmware control for the **Framework Laptop 13** on Ubuntu — fan curves, power limits,
battery charge limit, and performance profiles in one tray application.

Inspired by [G-Helper](https://github.com/seerge/g-helper), which does this for ASUS laptops
on Windows. This shares no code with it — see
[ADR 0001](docs/adr/0001-separate-repository.md).

> **Status: planning.** No code yet. The architecture is decided and recorded; the hardware
> is surveyed. See [docs/plan.md](docs/plan.md).

## Why

The pieces already exist on Linux — `powercap`, `cros_ec` hwmon, power-profiles-daemon,
`fw-fanctrl` — but they are scattered across sysfs paths, CLIs, and daemons that do not know
about each other. fw-helper is the unifying layer: named profiles that set all of them at
once, switchable from a tray icon or a hotkey.

## What works, honestly

| Feature | Status |
|---|---|
| Fan curves | Planned — verified working (0 → 4681 rpm, EC reclaims cleanly) |
| Performance profiles | Planned — layered over power-profiles-daemon |
| Power limits (PL1/PL2) | Planned — verified regulating to ±2% of setpoint |
| Battery charge limit | Planned — needs a module parameter ([ADR 0008](docs/adr/0008-charge-limit-via-module-parameter.md)) |
| Live telemetry | Planned |

Every mechanism above has been exercised on real hardware before any application code was
written — see [docs/hardware-baseline.md](docs/hardware-baseline.md).
| **Undervolting** | **Not possible.** See below |

### Undervolting is not available

Intel locked the undervolting MSR as the mitigation for
[Plundervolt / CVE-2019-11157](https://nvd.nist.gov/vuln/detail/CVE-2019-11157). It is
unreachable on Core Ultra parts and Framework's BIOS does not expose an unlock. This is a
firmware/silicon limitation, not something a Linux application can work around.

Power limits (PL1/PL2) deliver most of what people actually want from undervolting on a
laptop — lower sustained temperatures, quieter fan, longer battery. Measured on this machine:
dropping the sustained limit from 25 W to 15 W took the CPU from **76.8 °C to 64.8 °C** under
full load. See [ADR 0007](docs/adr/0007-no-undervolting.md).

## Hardware support

Developed against:

- Framework Laptop 13 Pro (Intel Core Ultra Series 3), board `FRANMJCP07`, BIOS 03.02
- Ubuntu 24.04 LTS, kernel 7.0

Other Framework 13 Intel boards will likely work — the daemon probes capabilities at startup
and disables what it cannot drive. AMD boards and the Framework 16 are **not** targeted for
v1; AMD in particular needs a different power-limiting mechanism (`ryzenadj` rather than RAPL).

## Requirements

- A kernel new enough for `cros_ec_hwmon` (fan control)
- power-profiles-daemon (optional but recommended — we integrate with it rather than
  replacing it, so the GNOME power slider keeps working)
- **No `ectool` or `framework_tool` needed** — we use kernel interfaces directly
  ([ADR 0004](docs/adr/0004-sysfs-first-hardware-access.md))

## Safety

Manual fan control is genuinely risky: once userspace takes over, the EC stops managing the
fan and holds the last duty written — so a crashed daemon can leave the fan stuck low under
load. [ADR 0006](docs/adr/0006-fail-safe-fan-control.md) specifies the mitigations
(restore-on-exit, crash-path binary, watchdog, a floor that is never quieter than firmware,
and a critical-temperature override that hands control back unconditionally).

`kill -9` recovery is a release gate, not a nice-to-have.

## Poking at your own machine

```bash
./scripts/fw-probe.sh                    # read-only survey
sudo ./scripts/fw-probe.sh --write-test  # also tests whether writes are honoured
```

`--write-test` briefly changes power limits and fan duty, then restores them. It only ever
spins the fan *up*. Read it before running it.

## Prior art

- [framework_system](https://github.com/FrameworkComputer/framework-system) — Framework's own tooling; our reference for EC host commands
- [fw-fanctrl](https://github.com/TamtamHero/fw-fanctrl) — established fan curve daemon
- [LACT](https://github.com/ilya-zlobintsev/LACT) — GPU control, for Framework 16
- [inputmodule-rs](https://github.com/FrameworkComputer/inputmodule-rs) — FW16 LED matrix

## Licence

GPL-3.0. See [ADR 0001](docs/adr/0001-separate-repository.md#licensing-note).
