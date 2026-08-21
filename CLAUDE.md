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
| M2 — battery charge limit | write path **verified on hardware**; suspend + reboot re-apply still to confirm |
| M3–M5, M7 | planned; every mechanism pre-verified |
| M6 — GUI | read-only telemetry view landed early; controls pending M2–M5 |

Read `docs/plan.md` for milestones and `docs/hardware-baseline.md` for what the board
actually exposes. **Do not re-derive hardware facts — they are measured and recorded.**

### Resume here — M2's last check

**The write path is proven.** On 2026-08-21, `sudo fw-helperctl charge-limit 80` returned
promptly (the polkit hang is fixed), the daemon's read-back confirmed it, sysfs reads `80`,
D-Bus reports `80%`, and `/var/lib/fw-helper/state` now holds `charge_limit=80`. No UEFI
override — `NotApplied` never fired. Also settled the same day: the modprobe drop-in **does**
take effect across a cold boot, which had only ever been proven after a live
`modprobe -r`/`modprobe`.

One criterion remains, plus one open question the suspend test surfaced. Both need the
daemon running, and there is still no systemd unit installed, so start it by hand:

```bash
sudo sh -c './target/debug/fw-helperd >/tmp/fw-helperd.log 2>&1 &'
```

**Suspend/resume — passed 2026-08-21.** After `systemctl suspend` and wake, sysfs read `80`
and the daemon logged both `resumed from sleep` and `re-applied charge limit 80%`. The
logind hook (`main.rs:106`) works and the post-resume write succeeds.

Note what this did *not* settle: at the time, `reapply_charge_limit` wrote unconditionally,
so that log line appeared whether or not firmware cleared anything. **Whether firmware
resets the threshold across suspend is still unknown.**

That is now fixed — `reapply_charge_limit` reads before writing and returns a `Reapply`
outcome, so the log distinguishes the cases:

```
charge limit still 80%; nothing to re-apply          → firmware left it alone
charge limit is 100%, expected 80%; re-applying      → firmware reset it
```

So the next suspend answers the question for free, with the daemon running as normal. Just
restart it on the new build first. The answer matters past M2: it decides whether the resume
hook every future write path inherits is load-bearing or belt-and-braces.

**Reboot.** The sysfs value is expected to be lost; what is under test is the daemon
re-applying it from persisted state at startup. After rebooting, start the daemon and expect
`persisted charge limit: 80%` in the log and `80` in sysfs — checked *before* touching
`fw-helperctl`, since the point is that no one asked.

Only then does M2 move to complete. Then M3.

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
