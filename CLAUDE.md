# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

`fw-helper` — firmware control for the **Framework Laptop 13** on Ubuntu: fan curves, power
limits, battery charge limit, performance profiles. Same product idea as
[G-Helper](https://github.com/seerge/g-helper) (ASUS/Windows), sharing **no code** with it.

Target machine: Framework Laptop 13 Pro, Intel Core Ultra X7 358H, board `FRANMJCP07`,
BIOS 03.02, EC `sakura-3.0.2`, Ubuntu 24.04, kernel 7.0.

## Current state

| Milestone | Status |
|---|---|
| M0 — baseline & architecture | complete: 9 ADRs, all hardware questions answered empirically |
| M1a — hardware layer (`fw-helper-core`, `fw-helperctl`) | complete, hardware-verified |
| M1b — `fw-helperd` + D-Bus (zbus) | next |
| M2–M7 | planned; every mechanism pre-verified on hardware |

Read `docs/plan.md` for milestones and `docs/hardware-baseline.md` for what the board
actually exposes. **Do not re-derive hardware facts — they are measured and recorded.**

## Commands

```bash
cargo test --all                          # 15 tests, no hardware or root needed
cargo clippy --all-targets -- -D warnings # CI gate
cargo fmt --all                           # CI gate
cargo run -p fw-helperctl -- status       # capabilities + one sample
sudo ./target/release/fw-helperctl watch  # package power needs root

./scripts/fw-probe.sh                     # read-only hardware survey
sudo ./scripts/fw-probe.sh --write-test   # writes and restores; read it first
sudo ./scripts/q6-pl1-load-test.sh        # PL1 efficacy; also M4's regression test
```

## Hard rules

**Fan control can damage the machine.** Once `pwm1_enable=1`, the EC stops managing the fan
and holds the last duty forever. Stuck-high is merely loud; **stuck-low under load is
dangerous, and looks identical from outside.** Every code path that takes manual control must
restore `pwm1_enable=2` on exit, signal, panic, and suspend. See ADR 0006 — it is
non-negotiable, and `kill -9` recovery is a release gate.

**`fw-helper-core` stays dependency-free.** std only. It builds and tests with no network, no
hardware, and no root. External crates belong in the daemon and GUI. This is deliberate, not
incidental.

**Never write hardware paths directly.** Everything goes through `Sysfs`, which carries a
filesystem root so fixtures can replace hardware. See ADR 0004.

**Capabilities must explain themselves.** A knob is `Cap::Yes` or `Cap::No(reason)` where the
reason tells the user how to fix it. Never offer a control that silently does nothing.

## Hardware traps

These have all cost time once. Do not rediscover them.

| Trap | Reality |
|---|---|
| `intel-rapl:0` reports `long_term` = **200 W** | Meaningless. Its own `max_power_uw` is 25 W. Use **`intel-rapl-mmio:0`** — the real PL1. Clamp any UI to `max_power_uw` |
| `peak_power` = 175 W | PL4, a microsecond current ceiling. Not a thermal budget |
| `temp*_max` = **-273150** | Unset (0 K). Only `temp*_crit` is usable, and validate it is 0–150 °C first |
| `fan1_target` stays `0` under manual control | Read `fan1_input` for actual RPM |
| hwmon indices | Not stable across boots. Always resolve by `name` (`cros_ec`) |
| `energy_uj` is `0400` | PLATYPUS/CVE-2020-8694 mitigation. Root only, and republishing it is rate-limited and quantized — ADR 0009 |
| Energy counter wraps | Every ~2.9 h at 25 W. Single wrap is correctable; multi-wrap and suspend are not — discard, never interpolate |
| No `charge_control_end_threshold` | Driver refuses to load on Framework by design. Needs `probe_with_fwk_charge_control=1` — ADR 0008 |
| Undervolting | **Impossible.** Plundervolt mitigation locks the MSR. Do not add a disabled control — ADR 0007 |
| PL1 averages over ~32 s | Any power measurement must span longer, or it reads turbo as steady state |

## Coexistence

power-profiles-daemon is active and owns `platform_profile` + EPP. **Delegate to it over
D-Bus; never write those paths directly** — last-writer-wins against the GNOME power slider is
the worst bug class here. ADR 0005.

## Conventions

- **ADRs are append-only.** Superseding a decision means a new ADR and a status change on the
  old one — never edit history. Index in `docs/adr/README.md`.
- **Verify before designing.** M0's value was that four of six starting assumptions were
  wrong. When hardware behaviour is uncertain, write a probe script and measure it.
- **State what is verified vs assumed.** The docs distinguish these deliberately.
- Commit messages: what changed and why, with the measurement if there was one.

## Reference numbers

Measured on the target machine, not estimated:

- Idle: 1.77 W, 43.9 °C, fan 0 rpm
- PL1 25 W → 24.67 W sustained, 76.8 °C, ~3100 rpm
- PL1 15 W → 14.68 W sustained, 64.8 °C, ~2925 rpm
- **10 W of power limit buys ~12 °C.** This is why ADR 0007 can drop undervolting
- Fan-start knee is above 45 °C; ramp to ~2900 rpm is compressed below 65 °C
