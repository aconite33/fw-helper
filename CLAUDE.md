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

The daemon side now holds and releases the lease. `FanLease` is **lock-free on purpose**
— the panic hook calls into it, and a hook that blocks on a mutex held by the panicking
thread leaves the process alive with the fan stuck. Releases are unconditional for the
same reason: our own bookkeeping is what is least trustworthy after a crash.

Verified on hardware 2026-08-21: `fw-helperctl fan 180` pinned 181/255 at 5093 rpm,
`fan auto` gave it back, and **SIGTERM while holding it** put `pwm1_enable` back to `2`
with `released manual fan control` in the log. The below-floor refusal fired without
touching hardware.

**Still not demonstrated:** the panic path (implemented, unit-tested, never triggered
live) and `ExecStopPost` (wired into the unit and installer, but needs the unit installed
plus a `kill -9` — that is the M3 exit gate).

**The watchdog is done and hardware-verified** (`fw-helperd/src/watchdog.rs`). A real OS
thread, not a tokio task — the failure it guards against includes the runtime not
scheduling anything. It reads `pwm1_enable` rather than trusting our own flag, so a failed
release retries. Heartbeat is the telemetry poll loop. Test it with
`FW_HELPERD_DEBUG_WEDGE_AFTER=<secs>`, which blocks every tokio worker; the fan came back
6.0 s after the heartbeat stopped, with the process still alive.

That test also produced the zbus row in the traps table, and with it a guard: manual fan
control is **refused on a stale heartbeat**, because D-Bus survives a wedged runtime and
would otherwise hand the fan to a daemon that is not minding it.

**The firmware-floor clamp is done and hardware-verified** (`fw-helper-core/src/floor.rs`).
The EC's curve cannot be read, so it is reconstructed by composing two measured tables —
what firmware does at a temperature, and what a duty produces — and inverted to answer
"what duty must we hold to be at least as fast as firmware right now". `observe()` raises
it from live EC behaviour, closing the gap the static table has across the knee.

**It is enforced every poll tick, not at request time.** Clamping only when a duty is
requested protects nothing: a duty chosen at idle is safe when chosen and stuck-low a
minute later. `fw-helperctl fan 0` at idle is now legitimate — firmware is silent below
~45 °C too, and that silence is most of why anyone wants this.

Verified under 16-core load: duty walked 0 → 92 as the machine reached 74.8 °C and back to
0 at 44.9 °C, staying above firmware's own measured curve at every point. **The first run
failed that comparison** (2808 rpm where the EC does 2925) because enforcement judged
"below the floor?" against the quantized read-back with `DUTY_TOLERANCE` of slack, hiding a
real deficit inside a tolerance meant for verifying writes. Decisions are compared exactly
now; drift against hardware separately. Unit tests were happy throughout — only comparing
against measured firmware behaviour caught it.

**The ceiling override is done and hardware-verified** (`fw-helper-core/src/ceiling.rs`).
Derived from the control sensor's crit minus 15 °C, capped at 100 °C, falling back to
90 °C when nothing plausible is readable — every value validated, because the -273150
case would otherwise put the ceiling at absolute zero and kill manual fan control
permanently. Its ordering against the floor's full-duty point is a **`const` assertion**,
so getting it wrong fails the build.

Verified with `FW_HELPERD_DEBUG_CEILING_C=55` (it can only ever *lower* the ceiling): took
the fan at 39.9 °C, released at 55.9 °C, refused at 57.9 °C, allowed again at 44.9 °C.

**The sleep hook is done and hardware-verified.** The fan is released before suspend and
taken back after the wake, held open by a logind **delay inhibitor lock** — without it the
release merely races the suspend. The restore re-runs the full clamp rather than replaying
the raw duty, since the machine may wake warmer than it slept, and a pending restore of
duty 0 uses a separate flag rather than a sentinel, because 0 is a legitimate setting.

**Every safety point in ADR 0006 is now built and verified on hardware.** The last one,
`ExecStopPost`, was measured on 2026-08-21: unit installed, fan taken at duty 120 under
16-core load, `kill -9` on the daemon, **EC control restored in 0.27 s** against a 5 s
gate. `SIGKILL` runs no handler and the watchdog thread dies with the process, so nothing
in-process could have covered it.

**The systemd unit is now installed and enabled**, so `fw-helperd` starts at boot.
`sudo ./scripts/install-dev.sh --uninstall` reverses that.

Two of M3's three exit criteria are met (`kill -9` recovery, suspend/resume). The third is
the curve holding a target temperature under `stress-ng` without audible hunting, which
needs the curve to exist.

**The floor now reads firmware rather than modelling it** (ADR 0011). `pwm1` reports the
EC's own duty while `pwm1_enable=2`, so `FirmwareFloor` records what firmware actually did
at each temperature; the composed RPM tables survive only as a cold start. Only the
**ascending** branch is recorded, and an observation of duty 0 is an answer, not a gap —
which is what makes a genuinely silent machine reachable.

Verified on hardware: after watching firmware climb, `fw-helperctl fan 0` at **51.9 °C**
was honoured, where the modelled floor had demanded duty 63.

**ADR 0011 also reframes why the floor exists.** The CPU throttles at Tjmax (100 °C), so a
constrained fan costs performance, not hardware — the floor was never the CPU's guardian.
What has no protection of its own is the **battery, crit 49.9 °C**, and nothing watches it
yet. That is the open work ADR 0011 names and does not do.

**The curve engine is built and hardware-verified** (`fw-helper-core/src/curve.rs`). It
produces a *request*; the floor and battery guard are applied on top every tick, so
smoothing can never delay a safety response. Hysteresis is asymmetric — rising followed at
once, falling damped by 2 °C.

**The win is not where M0 predicted.** That reasoning used firmware's descending branch.
Climbing, firmware is silent through 55–70 °C, so there is little to win going up; the win
is coming *down*, where firmware holds duty 50–90 to 44.9 °C. Measured, the curve beats it
by 13–36 counts through that range, with no hunting.

**Open limitation:** the observed floor is lost on daemon restart, and the cold-start model
is the loud one — measured, a curve asking for silence at 55 °C got duty 61 right after a
restart. Persisting observations to `/var/lib/fw-helper/state` is the obvious fix.

Next: M3 is feature-complete; remaining is M4's power limits, which the plan says to design
alongside the curve — 10 W of power limit buys ~12 °C, so the two compose.

**`fw-helperctl fan` is still not a curve.** It pins one duty rather than following
temperature. What it is not, any more, is unbounded — the duty is clamped up to the
firmware floor and re-enforced every tick, and `fan 0` at idle is legitimate because
firmware is silent there too.

**Testing note carried forward.** Three defects this session were invisible to unit tests
and only appeared on hardware: the EC's quantized duty read-back, a floor deficit hiding
inside `DUTY_TOLERANCE`, and a stale release binary. The curve engine needs the same
treatment, and a longer loop — hysteresis and ramp limiting only misbehave over minutes of
changing load.

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
fw-helperctl fan 180 | fan 0 | fan auto    # duty 0 or 30-255, clamped up to the firmware floor
fw-helperctl fan curve | fan curve 55:0,70:65,85:120   # follow a temp->duty curve
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
| `PrepareForSleep` does **not** wait for you | It is a notification, not a request for permission. Without a logind **delay inhibitor lock**, a pre-suspend write races the suspend. Also: `rtcwake -m mem` writes `/sys/power/state` directly and never emits the signal at all, so it cannot test any of this |
| Handing the fan back to the EC **reduces** airflow | Firmware's curve tops out near 3100 rpm; manual reaches ~5200. Releasing is a last resort that defers to firmware's *whole* thermal protection, not a cooling escalation. Demand full duty first |
| **The EC's fan curve is hysteretic** | At 61.9 °C firmware runs duty **0** heating and **92** cooling, and holds the fan on down to 44.9 °C. "Never quieter than firmware" is meaningless without naming a branch. Only the ascending branch says what a temperature needs (ADR 0011) |
| Temperature direction needs **hysteresis of its own** | `peci-temp` is quantized to ~1 °C, so a cooldown reads as long runs of identical values. Deriving rising/falling from consecutive samples treats those as "steady" — count steady as rising and you record the whole descending branch. Carry the direction through plateaus |
| The CPU **protects itself** at Tjmax | `coretemp` crit = 100 °C on every core. A constrained fan costs performance, not hardware. `peci-temp` crit reads 119.8 °C, *above* Tjmax, so it is not a usable limit. What has no protection is the **battery** (crit 49.9 °C) |
| The machine reaches **92.8 °C** in normal use | Not 76.8 °C — that was one M0 PL1 test. Two thresholds were set from the lower figure and both sat below normal operation |
| Fan duty→RPM is **concave** | A line through the high points (120/160/181) predicts 1343 rpm at duty 0 and puts 2925 rpm at duty 77; the measured answer is ~85. Fitting a line would set the firmware floor *below* firmware. Interpolate the measured table |
| Fan stiction is between duty 20 and 30 | Duty 20 = 0 rpm, duty 30 = 1107 rpm. A duty of 1–29 is a stopped fan, not a slow one. Refuse it; do not accept and ignore it |
| **zbus does not run on the tokio runtime** | With default features zbus 5 uses its own `async-io` executor. Blocking every tokio worker leaves D-Bus answering normally with stale telemetry, so "the daemon is wedged" is not all-or-nothing. Never infer daemon health from the interface responding |
| `cat > "$file"` **follows symlinks** | An older `install-dev.sh` left `/usr/local/bin/fw-helperctl` as a symlink into `target/release/`. The newer one wrote the shim through it, overwriting the real binary, which then exec'd itself forever at 100% CPU — and clobbered cargo's hardlinked artifact so it would not rebuild. `rm -f` before writing, always |
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
