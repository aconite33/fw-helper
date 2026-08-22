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
| M3 — fan control | **complete**: all six ADR 0006 safety points and the curve engine verified on hardware |
| M4 — power limits | PL1 control complete and verified (15 W setpoint → 15.02 W sustained) |
| M5 — profiles | complete: PPD delegation, user profiles, save/delete, AC/battery switching |
| M7 | planned; every mechanism pre-verified |
| M6 — GUI | controls land: profile, save/delete, power limit, charge limit, fan release, auto-switching. Curve editing outstanding |
| M7 — packaging | `.deb` builds with auto-computed deps, desktop entry, opt-in charge control. **Install not yet verified** |

Read `docs/plan.md` for milestones and `docs/hardware-baseline.md` for what the board
actually exposes. **Do not re-derive hardware facts — they are measured and recorded.**

### Resume here — verify the package, then the curve editor

Last session ended 2026-08-22. M0–M5 are complete and hardware-verified. M6 has working
controls and M7 has a `.deb` that builds but **has not been installed even once**.

**Do this first.** `./scripts/build-deb.sh` produces `fw-helper_<version>_amd64.deb`;
install it with `sudo apt install ./fw-helper_*.deb` and check three things nobody has
checked: that it installs and starts, that **fw-helper launches from the GNOME app grid**
rather than a terminal, and that `sudo apt remove fw-helper` returns the fan to the EC.
The prerm stops the unit so `ExecStopPost` runs, but that is reasoning, not evidence.

**The machine is currently bare.** `install-dev.sh --uninstall` ran, so there is no daemon
installed and the modprobe drop-in is gone — the charge limit is inactive until
`sudo fw-helper-enable-charge-control` (packaged) or `install-dev.sh --enable-charge-control`
(dev). `/var/lib/fw-helper/state` survives, so profiles and the learned fan floor are intact.

**Then: the fan curve editor**, the last real M6 feature. Everything under it exists — the
curve engine validates, the daemon accepts a curve over D-Bus, and `fw-helperctl fan curve
55:0,70:65` works. What is missing is a way to draw one. The measurements that should shape
it are in `docs/hardware-baseline.md`: the useful range is duty 30 (stiction, 1107 rpm) to
about 100, the interesting temperatures are 45–85 °C, and the win is on the way *down*
rather than up.

**Testing discipline, which this project keeps proving the hard way.** Roughly a dozen
defects across two sessions were invisible to unit tests and appeared only on hardware or in
the real UI. They fall into two families:

- *A constant or model chosen by reasoning rather than measurement* — the EC's percent
  quantization, its 20 °C of curve hysteresis, thresholds set from a 76.8 °C peak when the
  real one is 92.8 °C.
- *Plumbing that only fails outside the happy path* — the interactive polkit branch had
  never once executed because every earlier test ran as root; `systemctl enable --now` does
  not restart, so a fix sat unused on disk through three test rounds; zbus caches properties,
  which only a long-lived client can reveal.

**Verify the thing you are testing is the thing you built.** The daemon logs its own binary
age at startup for this reason. A GUI smoke test also passed while never building a window,
because an instance was already running and GTK is single-instance — kill any instance first
and treat "still alive when the timeout fires" as the pass.

**Fault injection**, never set in production:

```bash
FW_HELPERD_DEBUG_WEDGE_AFTER=15   # blocks every tokio worker; proves the watchdog
FW_HELPERD_DEBUG_CEILING_C=55     # lowers the ceiling into reach; can only ever lower it
FW_HELPER_DEBUG_WIDGETS=1         # GUI: traces control signals and command results
```

**Open, and deliberately not done:**

- The **panic path** is implemented and unit-tested but has never been triggered live.
- Floor observations only ever **rise** within a bucket and now persist, so a one-off
  anomaly is sticky. Errs loud, costing quiet rather than safety.
- The **battery guard has never fired** and is sized so it should not (ADR 0011).
- **PL2 is untouched**; its `max_power_uw` reads 0.
- The curve's **sensor is not configurable** — `control_temp()` picks `peci-temp`.
- `/etc/sudoers.d/fw-helper-dev` grants passwordless `install-dev.sh`. It is scoped to a
  script in a writable directory, so treat it as standing root and remove it when done.

**Running hardware tests:** hand the user the command prefixed with `!` and `tee` the output
to a file — terminal output does not always reach the transcript.

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
fw-helperctl power-limit 15               # sustained CPU watts; ~32s to take effect
fw-helperctl profile | profile quiet      # quiet | balanced | performance; moves the GNOME slider
/etc/fw-helper/profiles.d/*.conf          # user profiles; see data/example-profile.conf
./target/debug/fw-helper                  # the GUI

./scripts/fw-probe.sh                     # read-only hardware survey
sudo ./scripts/fw-probe.sh --write-test   # writes and restores; read it first
sudo ./scripts/q6-pl1-load-test.sh        # PL1 efficacy; also M4's regression test
```

`FW_HELPERD_SESSION_BUS=1` runs daemon and clients on the session bus — development only,
avoids needing root and an installed policy.

## Hard rules

**Never leave the fan in a state nobody is managing.** Once `pwm1_enable=1` the EC stops
managing it and holds the last duty **forever** — through a crash, a deadlock, a suspend.
Stuck-high is merely loud; stuck-low looks identical from outside and is silent by
definition. Every path taking manual control must restore `pwm1_enable=2` on exit, signal,
panic and suspend. ADR 0006, non-negotiable, and `kill -9` recovery is a release gate.

Note what the danger *is*, since ADR 0011 sharpened it: the CPU throttles at Tjmax (100 °C)
and protects itself, so a fan held too low costs performance rather than hardware. What has
no protection of its own is the **battery** (crit 49.9 °C) and the board/DDR sensors
(~87 °C). A user choosing quiet is making a trade, not a mistake — but a *daemon* that dies
holding the fan made the choice for them, which is the thing all of ADR 0006 exists to
prevent.

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
| A **verified** PL1 write still does not stick | Read back 25 W, was 33 W seconds later — above the advertised `max_power_uw`, so that field does not bind firmware either. Switching `platform_profile` makes firmware re-derive PL1 asynchronously. Re-assert on a timer; an immediate read-back cannot see it |
| Setting PPD **echoes back** | Our own `ActiveProfile` write emits a change signal indistinguishable from the user moving the GNOME slider. Mark what you set, or you apply everything twice |
| `constraint_1_max_power_uw` = **0** | Unset, not "no power allowed" — same trap as `temp*_max` = -273150. Clamping a slider to `max_power_uw` is right for PL1 (25 W) and silently zeroes PL2. Validate first |
| **Root cannot overwrite your file in `/tmp`** | `fs.protected_regular=2` blocks root `O_CREAT`ing a file owned by another user in a sticky world-writable dir. Test scripts run as both users across a session; put their data outside `/tmp` |
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
| **zbus proxies cache properties** | A property with no change signal is fetched once and frozen for the proxy's life. The daemon signals only `Telemetry` and `CriticalTemperatures`, so a long-lived client saw a stale profile list and a stale power limit. The CLI cannot show this — it builds a fresh proxy per run. Mark properties `emits_changed_signal = "false"` or emit the signal |
| An "active profile" derived from **PPD alone** is wrong | PPD has three positions and any number of profiles can share one, so a user profile reports back as whichever built-in shares its axis — and a client that trusts the report moves its selection there. Report what was applied, while PPD still agrees |
| **zbus handlers are not on the tokio runtime** | `tokio::time::timeout` in an interface method panics with "there is no reactor running" and takes the connection's executor thread down. Hand timer work to a captured `Handle`. Cost three test rounds because the path only runs for *unprivileged* callers |
| `systemctl enable --now` does **not** restart | It starts a unit only if it is not already running, so every reinstall after the first leaves the old process serving the old binary while the files on disk look new. Use `enable` + `restart` |
| A long method **stalls the poll loop** | The loop took zbus's interface *write* lock every tick; any method awaiting a polkit prompt holds the read lock meanwhile. A password dialog stopped the heartbeat for 6 s and the fan watchdog took the fan back. Keep published state behind its own mutex and read-lock only |
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
- PL1 25 W → 24.67 W sustained, 76.8 °C, ~3100 rpm (M0, controlled)
- PL1 15 W → 14.68 W sustained, 64.8 °C, ~2925 rpm (M0, controlled)
- PL1 15 W → **15.02 W, +0.1%**, 62.2 °C — driven through the daemon (M4)
- **10 W of power limit buys ~12 °C.** Why ADR 0007 can drop undervolting. Rests on the
  M0 runs; M4's 25 W figure was heat-soaked and does not re-confirm it
- Tjmax **100 °C** (`coretemp` crit). Peak in ordinary use **92.8 °C**, not 76.8 °C
- Duty → RPM is **concave**: 30→1107, 50→1879, 77→2693, 90→3052, 120→3840, 180→5201 rpm.
  Stiction between duty 20 and 30
- **The EC's curve is hysteretic**, and the descending branch is what M0 recorded. Climbing,
  firmware is silent past 64.8 °C and starts at 66–73 °C. Falling, it holds duty 50–90 all
  the way to 44.9 °C — duty 0 vs 92 at the same 61.9 °C.
  **That descent is where a custom curve wins**, not the "flat top" M0 predicted: measured,
  the built-in curve beats firmware by 13–36 duty counts through 50–60 °C
