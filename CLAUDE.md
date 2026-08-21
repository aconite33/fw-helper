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
| M0 — baseline & architecture | complete: 10 ADRs, all 6 hardware questions answered empirically |
| M1a — hardware layer | complete, verified on hardware |
| M1b — daemon + D-Bus | complete, verified unprivileged against a root daemon |
| M2 — battery charge limit | complete: write path, suspend/resume and reboot re-apply all verified on hardware |
| M3 — fan control | in progress: lease mechanism + restore binary land and are hardware-verified; daemon side not started |
| M4–M5, M7 | planned; every mechanism pre-verified |
| M6 — GUI | read-only telemetry view landed early; controls pending M2–M5 |

Read `docs/plan.md` for milestones and `docs/hardware-baseline.md` for what the board
actually exposes. **Do not re-derive hardware facts — they are measured and recorded.**

### Resume here — M3

**M2 is complete**, all three criteria measured on hardware on 2026-08-21.

The write path: `sudo fw-helperctl charge-limit 80` returned promptly (the polkit hang is
fixed), the daemon's read-back confirmed it, sysfs read `80`, D-Bus reported `80%`, and
`/var/lib/fw-helper/state` holds `charge_limit=80`. No UEFI override — `NotApplied` never
fired, so that path stays implemented but unexercised. The modprobe drop-in also survives a
cold boot, which had only ever been proven after a live `modprobe -r`/`modprobe`.

**Reboot re-apply: passed.** Across a journal-verified reboot the daemon was left down for
27 minutes with sysfs reading `100` — the value really is lost at boot. Starting it logged,
before any client command was issued:

```
persisted charge limit: 80%
charge limit is 100%, expected 80%; re-applying
re-applied charge limit 80%
```

**Suspend/resume: passed, and firmware does *not* reset the threshold.** The instrumented
build's first suspend logged `charge limit still 80%; nothing to re-apply`. So the resume
hook (`main.rs:106`) is **insurance rather than a requirement** for the charge limit. Two
caveats: that is one ~28 s s2idle cycle on battery, and it says nothing about the other
knobs — the EC has far more reason to reset a fan or power limit than a charge threshold.
**Each of M3–M5 earns this verdict separately.** Every resume adds a data point for free, so
a contradicting line in the log would be conspicuous.

Reading before writing is what made both results legible in one line each. **Keep that
pattern for every knob M3–M5 adds** — an unconditional write hides exactly the question you
are trying to answer.

**Known gap, not blocking M3:** there is still no systemd unit installed, so the daemon runs
by hand. `data/` carries the unit; `install-dev.sh` does not install it yet.

```bash
sudo sh -c './target/debug/fw-helperd >/tmp/fw-helperd.log 2>&1 &'
```

### M3 — in progress

Two of ADR 0006's safety layers are built and hardware-verified: the lease itself
(`fw-helper-core/src/fan.rs`) and `fw-helper-restore-fan`, the crash-path binary. A root
run on 2026-08-21 took control at 180/255 (fan 0 → 5041 rpm), ramped to 120/255 (3795 rpm),
released, and the EC was back to 0 rpm within 4 s. It also corrected two assumptions — see
the two new fan rows in the traps table, and `docs/hardware-baseline.md` for the round-trip
measurements.

**Nothing drives the fan yet.** No control loop, no D-Bus method, no CLI verb. Next, in
this order: restore on exit/signal/panic in the daemon, `ExecStopPost` wiring, then the
watchdog — all of it before the curve becomes user-editable. ADR 0006 is not negotiable.

## Layout

```
crates/
  fw-helper-core/     hardware logic. ZERO dependencies, enforced in CI (ADR 0010)
  fw-helper-client/   D-Bus proxy + decoded Snapshot, shared by CLI and GUI
  fw-helperd/         root daemon, owns all hardware access
  fw-helperctl/       CLI; prefers D-Bus, falls back to direct sysfs
  fw-helper-gui/      libadwaita window (binary: `fw-helper`), unprivileged
data/                 D-Bus policy, polkit policy, systemd unit, modprobe drop-in
scripts/              fw-probe.sh, q6-pl1-load-test.sh, install-dev.sh
```

## Commands

```bash
cargo test --all                          # no hardware, no root, no network
cargo clippy --all-targets -- -D warnings # CI gate
cargo fmt --all                           # CI gate
cargo build --release --all               # ALWAYS build release too, see traps

sudo ./scripts/install-dev.sh             # D-Bus + polkit policy, CLI shim on PATH
sudo ./scripts/install-dev.sh --enable-charge-control   # opt-in, ADR 0008
sudo ./scripts/install-dev.sh --uninstall

sudo sh -c './target/debug/fw-helperd >/tmp/fw-helperd.log 2>&1 &'
sudo pkill -x fw-helperd
fw-helperctl status | watch [secs] | charge-limit N
./target/debug/fw-helper                  # the GUI

./scripts/fw-probe.sh                     # read-only hardware survey
sudo ./scripts/fw-probe.sh --write-test   # writes and restores; read it first
sudo ./scripts/q6-pl1-load-test.sh        # PL1 efficacy; also M4's regression test
```

`FW_HELPERD_SESSION_BUS=1` runs daemon and clients on the session bus — development only,
avoids needing root and an installed policy.

## Hard rules

**Fan control can damage the machine.** Once `pwm1_enable=1`, the EC stops managing the fan
and holds the last duty forever. Stuck-high is merely loud; **stuck-low under load is
dangerous, and looks identical from outside.** Every path taking manual control must restore
`pwm1_enable=2` on exit, signal, panic, and suspend. See ADR 0006 — non-negotiable, and
`kill -9` recovery is a release gate.

**Every hardware write follows the same pattern**, established in M2:
1. polkit check first, per action, failing **closed**
2. validate range before checking support, so a typo reports as a typo
3. write, then **read back and verify** — a silent override is the expected failure here
4. persist and re-apply on resume, because firmware resets things
5. errors name the fix, not the symptom

**Write methods take `&self`, never `&mut self`.** With `&mut self` zbus holds the interface
write lock for the whole call, so one pending polkit prompt stalls telemetry for every
client. Put mutable state behind a mutex and never hold it across an await.

**`fw-helper-core` stays dependency-free.** std only. External crates belong in the daemon,
client, and GUI. CI fails the build if core gains a dependency.

**Never write hardware paths directly.** Everything goes through `Sysfs`, which carries a
filesystem root so fixtures replace hardware (ADR 0004).

**Capabilities must explain themselves.** `Cap::Yes` or `Cap::No(reason)` where the reason
tells the user how to fix it. Never a dead control with no explanation.

## Traps

All of these cost real time once. Do not rediscover them.

| Trap | Reality |
|---|---|
| `intel-rapl:0` `long_term` = **200 W** | Meaningless; its own `max_power_uw` is 25 W. Use **`intel-rapl-mmio:0`**. Clamp any UI to `max_power_uw` |
| `peak_power` = 175 W | PL4, a microsecond current ceiling. Not a thermal budget |
| `temp*_max` = **-273150** | Unset (0 K). Only `temp*_crit` is usable, validated to 0–150 °C first |
| `fan1_target` stays `0` under manual control | Read `fan1_input` for actual RPM |
| hwmon indices | Not stable across boots. Resolve by `name` (`cros_ec`) |
| `energy_uj` is `0400` | PLATYPUS/CVE-2020-8694. Root only; republishing is rate-limited and quantized (ADR 0009) |
| Energy counter wraps | Every ~2.9 h at 25 W. Single wrap is correctable; multi-wrap and suspend are not — discard, never interpolate |
| PL1 averages over ~32 s | Any power measurement must span longer or it reads turbo as steady state |
| Charge control absent by default | Driver refuses to bind on Framework by design. Needs `probe_with_fwk_charge_control=1` (ADR 0008) |
| Undervolting | **Impossible.** Plundervolt mitigation locks the MSR. Do not add a disabled control (ADR 0007) |
| **polkit `AllowUserInteraction` hangs forever** | When no authentication agent can service the caller — any process without `XDG_SESSION_ID`. Check without interaction first, then bound the interactive call |
| `pwm1` write while `pwm1_enable=2` | **`EOPNOTSUPP`**, not silently ignored. The duty cannot be pre-loaded before taking control, so the takeover window is real — keep the mode switch and first duty write adjacent |
| Fan duty read-back ≠ what you wrote | The EC stores whole percent: write 180, read 181. Verify with `DUTY_TOLERANCE`, not equality. `pwm1` is also zeroed a few seconds *after* release, so it never tells you who owns the fan — read `pwm1_enable` |
| **Stale binary on PATH** | Bit us twice, both times looking like a broken daemon. `install-dev.sh` now installs a shim resolving the newest build per invocation. Still: build release *and* debug |
| **XML comments forbid `--`** | Used as an em dash it broke the D-Bus policy; dbus-daemon skipped the file silently and surfaced it as `AccessDenied` much later. Validated in CI now |
| MSRV silently picks stale deps | At `rust-version = "1.74"` the resolver chose zbus 3 while 5 existed. **Check what resolved, not just that it resolved** |

## Coexistence

power-profiles-daemon is active and owns `platform_profile` + EPP. **Delegate to it over
D-Bus; never write those paths directly** — last-writer-wins against the GNOME power slider
is the worst bug class here (ADR 0005).

## Conventions

- **ADRs are append-only.** Superseding means a new ADR plus a status change on the old one.
  Index in `docs/adr/README.md`.
- **Verify before designing.** M0's value was that four of six starting assumptions were
  wrong. When hardware behaviour is uncertain, write a probe script and measure.
- **State what is verified vs assumed.** The docs distinguish these deliberately, including
  when something is implemented but not yet demonstrated.
- Commit messages: what changed and why, with the measurement if there was one.

## Reference numbers

Measured on the target machine, not estimated:

- Idle: 1.77 W, 43.9 °C, fan 0 rpm
- PL1 25 W → 24.67 W sustained, 76.8 °C, ~3100 rpm
- PL1 15 W → 14.68 W sustained, 64.8 °C, ~2925 rpm
- **10 W of power limit buys ~12 °C.** Why ADR 0007 can drop undervolting
- Fan curve: 0 rpm at 44.9 °C · ~2020 rpm at 53.9 °C · ~2925 rpm at 64.8 °C · ~3100 rpm at
  76.8 °C. Off to two thirds of loaded speed in ~9 degrees, then nearly flat for twenty
  more. **That flat top is where a custom curve wins.**
