# fw-helper — implementation plan

A G-Helper-equivalent for the Framework Laptop 13 on Ubuntu.

**Read first:** [hardware-baseline.md](hardware-baseline.md) for what the board actually
exposes, and [adr/](adr/) for why the architecture is shaped the way it is.

## What this is

One tray application that unifies the scattered firmware knobs on a Framework 13 into named
profiles you can switch with a hotkey — fan curve, power limits, charge limit, performance
profile — the way G-Helper does for ASUS laptops on Windows.

## Scope

**In scope (v1)**

| Feature | Mechanism | Confidence |
|---|---|---|
| Performance profiles | PPD D-Bus, layered ([0005](adr/0005-delegate-to-power-profiles-daemon.md)) | High — verified present |
| Fan curves | `cros_ec` hwmon `pwm1` | High — verified working on hardware |
| Power limits (PL1/PL2) | `intel-rapl-mmio` powercap | High — verified regulating to ±2% |
| Battery charge limit | sysfs, via module param ([0008](adr/0008-charge-limit-via-module-parameter.md)) | High — mechanism confirmed |
| Live telemetry | hwmon + powercap `energy_uj` | High |
| Keyboard backlight | `/sys/class/leds/chromeos::kbd_backlight` | High |

**Explicitly out of scope**

- **Undervolting** — not possible on this platform ([0007](adr/0007-no-undervolting.md))
- **GPU control** — no dGPU on FW13. FW16 users are served by LACT
- **Peripherals** — G-Helper's mouse/keyboard support is ASUS-specific
- **FW16 LED matrix** — `inputmodule-rs` already does this well
- **Replacing TLP / PPD** — we are a client, not a competitor

## Repository layout

```
fw-helper/
  crates/
    fw-helper-core/     shared types, profile schema, D-Bus interface definitions
    fw-helperd/         privileged daemon  -> /usr/libexec/fw-helperd
    fw-helper-gui/      GTK4 tray + window -> /usr/bin/fw-helper
    fw-helperctl/       CLI over the same D-Bus API
    fw-helper-restore-fan/  crash-path safety binary (ADR 0006)
  data/
    org.fwhelper.Daemon1.conf      D-Bus system bus policy
    org.fwhelper.policy            polkit actions
    fw-helperd.service             systemd unit
  docs/
  scripts/
    fw-probe.sh
```

## Milestones

Each milestone is independently useful and independently shippable. Do not start the GUI
until M1–M4 work from the CLI.

---

### M0 — Baseline and open questions  ✅ mostly done

Establish what the hardware does before designing around it.

- [x] Read-only hardware survey (`scripts/fw-probe.sh`)
- [x] Capture results in `docs/hardware-baseline.md`
- [x] ADRs 0001–0007
- [x] Q2 / Q5 answered from `max_power_uw`: authoritative zone is `intel-rapl-mmio:0`,
      envelope is 25 W PL1 / 60 W PL2
- [x] **Q1 answered — RAPL writes stick.** No firmware lock bit. M4 viable
- [x] **Q4 answered — `pwm1` drives the fan** (0 → 4681 RPM at 63% duty) and the EC reclaims
      control cleanly on release. M3 viable
- [x] **Q3 answered** — driver refuses to load by design; override via module parameter
      ([ADR 0008](adr/0008-charge-limit-via-module-parameter.md)). M2 viable
- [x] **Q6 answered — PL1 governs draw**, regulating to within 2% of setpoint
      (25 W → 24.67 W, 15 W → 14.68 W). 10 W of limit buys 12 °C

**Exit:** reached. Every gating question came back green; no milestone is cut. All four
hardware mechanisms (fan, power, charge, profiles) are verified on the target board before
a line of application code exists.

---

### M1 — Daemon skeleton, read-only

No hardware writes at all. Prove the plumbing.

- Cargo workspace, `rust-toolchain.toml`, CI (fmt, clippy, test)
- `fw-helperd` as a systemd service, D-Bus system-bus name `org.fwhelper.Daemon1`
- **Capability probing at startup** — detect each knob, expose via `Capabilities` property
  ([0003](adr/0003-privileged-daemon-split.md)). This is what stops the GUI offering dead controls
- `Telemetry` property: temps, fan RPM, package power (from `energy_uj` deltas), battery
- Sysfs access layer behind a trait, with a fake root for tests ([0004](adr/0004-sysfs-first-hardware-access.md))
- `fw-helperctl status` renders it

**Exit:** `fw-helperctl status` prints live telemetry on the real machine, and the sysfs layer
has unit tests running against a fixture directory with no hardware.

---

### M2 — Battery charge limit

Smallest real feature, lowest risk, immediately useful.

- Ship `/etc/modprobe.d/fw-helper.conf` with `probe_with_fwk_charge_control=1`, then use
  standard `charge_control_end_threshold` ([ADR 0008](adr/0008-charge-limit-via-module-parameter.md))
- Opt-in install step, not a silent postinst — it changes who governs charging
- Read back after write; a persistent mismatch means a UEFI battery limit is fighting us.
  Report that specifically, not as a generic failure
- polkit action `org.fwhelper.set-charge-limit`
- Persist across reboot; re-apply on resume

**Exit:** `fw-helperctl charge-limit 80` holds across a reboot and a suspend/resume cycle.
**Gated on Q3.**

---

### M3 — Fan control

The highest-value and highest-risk feature. Do not shortcut
[ADR 0006](adr/0006-fail-safe-fan-control.md).

- Curve engine: interpolated temp→duty points, configurable sensor, hysteresis to stop
  oscillation, ramp limiting so the fan does not step audibly
- **Where the benefit actually is** (from Q6): the stock EC curve sits at 0 rpm at 43.9 °C but
  ~2900 rpm at 64.8 °C, and only reaches ~3100 rpm at 76.8 °C. It ramps hard between roughly
  45 °C and 65 °C, then flattens. The audible win is a gentler curve through the 55–70 °C
  band — which is exactly where a 15 W profile parks the machine. Fan and power profiles
  compose; design them together, not separately
- Safety, in this order — **build these before the curve is user-editable**:
  - restore `pwm1_enable=2` on exit, signal, and panic
  - `ExecStopPost` restore binary for the crash path
  - watchdog thread with independent timer
  - firmware-floor clamp (never quieter than the EC would be)
  - `temp*_crit`-derived ceiling override, with sanity validation
  - refuse manual control if the sensor is unreadable
- logind `PrepareForSleep` handling

**Exit:** `kill -9` on the daemon under sustained load restores EC fan control within 5 s.
Suspend/resume leaves the fan in a correct state. Curve holds a target temperature under
`stress-ng` without audible hunting. **Gated on Q4.**

---

### M4 — Power limits

- Write PL1/PL2 via `intel-rapl-mmio:0` — confirmed as the authoritative zone (Q2)
- Envelope is **25 W PL1 / 60 W PL2** (Q5), and PL1 regulates to ±2% of setpoint (Q6)
- Profile values, grounded in the Q6 measurements (10 W ≈ 12 °C):

  | Profile | PL1 | Expected sustained CPU temp |
  |---|---|---|
  | Quiet | 15 W | ~65 °C |
  | Balanced | 20 W | ~71 °C |
  | Performance | 25 W (stock) | ~77 °C |

- Never expose the MSR zone's bogus 200 W as a slider maximum — clamp the UI to `max_power_uw`
- **Any power measurement must average over more than the ~32 s PL1 window.** Sampling sooner
  reads turbo as steady state — the Q6 run showed 29 W at t+25 s under a 25 W limit
- Validate writes stick — read back after a delay, and downgrade the capability to
  unavailable if firmware reverts them
- Clamp to a sane range; refuse values that would make the machine unusable
- Re-apply on resume (firmware commonly resets these)

**Exit:** already demonstrated by `scripts/q6-pl1-load-test.sh` — the milestone is to make the
daemon do what that script proved possible, and to re-run the script as a regression test.

---

### M5 — Profiles

Ties M2–M4 together into the actual product.

- Profile schema: PPD profile + fan curve + PL1/PL2 + charge limit
- Delegate the PPD axis over D-Bus; **subscribe** to PPD's `ActiveProfile` so the GNOME
  power slider stays authoritative and we follow it ([0005](adr/0005-delegate-to-power-profiles-daemon.md))
- Fallback to direct `platform_profile`/EPP writes if PPD is absent
- Ship three sane defaults (Quiet / Balanced / Performance); user profiles in
  `/etc/fw-helper/profiles.d/`
- AC/battery auto-switching

**Exit:** Moving the GNOME power slider applies the matching fan curve and power limits.
`fw-helperctl profile quiet` does the same and GNOME reflects it.

---

### M6 — GUI

- GTK4 + libadwaita main window: status, profile picker, curve editor, charge slider
- Tray icon via StatusNotifierItem. **Note:** stock Ubuntu GNOME needs the AppIndicator
  extension. Detect its absence and tell the user, rather than showing nothing
- Grey out controls the daemon reports as unavailable, with the reason on hover
- Show when a safety override is active and why (ADR 0006 makes this mandatory, not optional)
- Global hotkey for profile cycling via the desktop portal

**Exit:** Full feature parity with `fw-helperctl`, no root, nothing offered that does not work.

---

### M7 — Packaging

- `.deb` for daemon + GUI, with polkit policy, D-Bus conf, systemd unit
- Postinst that does **not** enable manual fan control by default — opt-in
- README documenting exactly which knobs work on which boards, and the honest limitations
  (no undervolting, and why)
- Consider a PPA

---

## Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Stuck-low fan cooks the machine | Severe | ADR 0006 in full; `kill -9` test is a release gate |
| RAPL locked by firmware | M4 cut | Q1 answers this in M0, before any code is written |
| UEFI battery limit fights us | Charge limit silently wrong | Read back after write; surface the conflict explicitly (ADR 0008) |
| PPD D-Bus name changes | Profiles break | Support both known names, prefer newer |
| Kernel/EC firmware drift | Silent breakage | Capability probing at startup; never assume |
| Scope creep toward FW16/AMD | Never ships | v1 is FW13 Intel only; revisit after M7 |

## Immediate next actions

1. `sudo ./scripts/fw-probe.sh --write-test` — answers Q1 and Q4
2. `sudo dmesg | grep -i charge` — answers Q3
3. Update `docs/hardware-baseline.md` with the results
4. Cut any milestone whose gating question failed, then start M1
