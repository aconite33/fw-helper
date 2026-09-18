# fw-helper — AMD and Intel Framework 13

Firmware control and system monitoring for the **Framework Laptop 13** on Linux — the
**AMD Ryzen AI 300** boards, alongside the **Intel Core Ultra Series 3** "Pro" that upstream
targets.

This is a hardware fork of [Wooloomooloo2/fw-helper](https://github.com/Wooloomooloo2/fw-helper).
That project's architecture, ADRs and safety model are carried over wholesale; what changed
is everything that touches the AMD boards, which turned out to be most of the hardware
layer. The Intel code path is kept intact beside it rather than replaced — see
[Which boards](#which-boards) for exactly what has been verified on each.

> **Status: fan control works on AMD, and survives `kill -9`.** Monitoring, profiles and
> the charge limit work on both. Power limits exist only on Intel. Read the table before
> expecting anything.

![The panel applet](docs/images/applet-panel.png)

CPU and memory sparklines, a usage bar per mounted drive, the battery with its percentage
inside it, then temperature, fan and power draw. Every screenshot here is from the machine
described under [Which boards](#which-boards) — nothing is mocked up.

## Why a fork rather than a patch

Four of the interfaces upstream depends on are absent or different here, and two of its
ADRs do not hold. This is not a matter of a few `#[cfg]` branches:

| | Intel Pro (upstream) | AMD Ryzen AI 300 |
|---|---|---|
| Board / EC | `FRANMJCP07`, `sakura-3.0.2` | `FRANMGCP05` / `FRANMGCP09`, **`lilac-3.0.5` / `lilac-4.0.2`** |
| Fan control | `pwm1` + `pwm1_enable` in sysfs | **neither exists** — raw EC commands only |
| Fan duty | 0–255, with read-back | **percent 0–100, no read-back at all** |
| Power limits | `intel-rapl-mmio:0`, PL1 writable | **no RAPL** (`enabled=0`, no MMIO zone) |
| Profiles | power-profiles-daemon | **not installed**; `amd-pmf` owns `platform_profile` |
| Sensors | 5, incl. `peci-temp`, `battery_temp` | 4 — **neither of those two** |

Everything above was measured, not assumed. The full survey is in
[docs/hardware-baseline-amd.md](docs/hardware-baseline-amd.md); upstream's Intel figures are
in `docs/hardware-baseline.md` and **do not carry over**.

## What works on the AMD boards, honestly

For the Intel Pro, see [Which boards](#which-boards) — it keeps upstream's feature set.

| Feature | Status |
|---|---|
| **Cinnamon panel applet** | **Working** — temps, fan, load, memory, disks, battery, top processes. Needs no daemon and no root |
| Live telemetry | **Working** — temps, fan RPM, battery draw and charge rate |
| Capability detection | **Working** — every knob reports available, or why not |
| Performance profiles | **Working** — writes `platform_profile` directly, since there is no PPD here to defer to |
| GUI | **Working**, including the fan controls and curve editor. The power-limit control stays inert, with its reason shown |
| Battery charge limit | **Working.** Framework's EC command `0x3E03` ([ADR 0012](docs/adr/0012-charge-limit-via-custom-ec-command.md)). On `FRANMGCP09`, charging from below on AC stopped at exactly 80%: `Not charging`, `charge_now` flat ([measurement](docs/measurements/charge-limit-hx370.txt)). Not yet run on `FRANMGCP05` |
| Fan control | **Working.** Driven over EC commands, bounded by a firmware floor built from this board's own measured fan and firmware curve. `kill -9` recovery verified: fan back with the EC within 1.29 s through the crash path alone ([ADR 0013](docs/adr/0013-fan-control-via-ec-commands.md)) |
| Power limits | **No mechanism exists.** No RAPL, and Framework's EC command set has no PPT or SOC power command. On AMD the limits move through `amd-pmf`'s profiles, so that is where power control lives |
| Undervolting | Not attempted |

## The Cinnamon applet

The most finished part of this fork, and independent of everything else — it reads `/proc`
and sysfs directly, so it needs no daemon, no D-Bus policy and no root.

```bash
./scripts/install-applet.sh          # per-user; --link instead, for development
```

Then right-click the panel → Applets → **Framework Monitor**.

<img src="docs/images/applet-menu.png" alt="The applet dropdown" width="380" align="right">

**Panel:** CPU and memory sparklines, a usage bar per mounted drive, a battery with its
percentage inside it, and CPU temperature, fan speed and power draw as text.

**Dropdown:** ring gauges for temperature, CPU usage and load; a usage-history chart;
per-core bars; a user/system/idle breakdown; every EC sensor with its critical threshold;
memory and swap; every mounted filesystem; battery charge in mAh, health against design
capacity, and cycle count; and the top processes by CPU. It scrolls, because on a laptop
screen it is taller than the display.

The dropdown's layout follows [Stats](https://github.com/exelban/stats) on macOS: gauges
first, then history, then the breakdown, then what is responsible. Its first entry opens
the full application; `install-dev.sh` puts `fw-helper` on `PATH` for that, and the
applet's settings take an explicit path if you are running from a build tree.

<br clear="right">

The screenshot above is a live one, which is why the disk list has three real entries —
`/`, `/boot`, and an unlocked VeraCrypt volume that the applet picked up on its own.

Details worth knowing:

- **Drives are detected from `/proc/mounts`**, so a VeraCrypt or USB volume appears when
  mounted and vanishes when closed, with nothing to configure. FUSE bookkeeping mounts are
  filtered out — including VeraCrypt's own auxiliary mount, which is not your data.
- **Power draw is only reported on battery.** On mains, `current_now` is what goes *into*
  the battery, so it is shown separately as a charge rate (`+32W`) and never as system draw.
- **The panel holds a constant width.** Readings are drawn into a canvas sized from the
  widest value each field can ever take, so a number that gains a digit — or a `+` that
  appears when you plug in — never shifts the applets beside it.
- **HiDPI is handled**, and the process list is only gathered while the dropdown is open,
  since walking every `/proc/PID` on the compositor's own main loop is the one thing here
  that could stutter the desktop.

## The application

![The GTK window](docs/images/gui.png)

Carried over from upstream, and worth reading as a status report in itself: the controls
that cannot work on a board say so rather than sitting there dead. The screenshot predates
the fan port, which is why the fan reads *unavailable* in it; on the AMD boards the fan
controls and curve editor are now live. The power limit still explains that there is no
`intel-rapl-mmio:0` zone — that one is a property of the hardware, not of the code.

## Which boards

A fan's behaviour belongs to the board: how fast a duty turns it, which sensor firmware
reads, what firmware does at each temperature. So those measurements live in **board
profiles**, keyed by the DMI board name they were taken on, and the daemon picks one at
startup. Applying one board's numbers to another is not conservative, it is wrong — the
Intel tables on the AMD fan would put the firmware floor up to **2553 rpm below firmware**.

| | AMD Ryzen AI 300 | Intel Core Ultra 3 ("Pro") |
|---|---|---|
| Boards | `FRANMGCP05`, `FRANMGCP09` | `FRANMJCP07` |
| Verified on | both, on hardware | upstream's hardware; **not re-tested since this fork's changes** |
| Fan control | EC commands, no read-back | `pwm1` sysfs, read back |
| Firmware follows | `cpu_f75303@4d`, not hysteretic against it | `peci-temp`, treated as hysteretic |
| Power limits | none — profiles only | PL1 via RAPL |
| Charge limit | EC `0x3E03`; stops charging, verified on `FRANMGCP09` | EC `0x3E03`; verified |
| Crash recovery | `kill -9` verified, 1.29 s | verified upstream, 0.27 s |

**On the Intel Pro, the fork behaves as upstream does** — it drives the fan through `pwm1`,
sends the EC nothing, and keeps upstream's tables, sensor and read-before-release reclaim.
That is pinned by tests with an EC transport present, so the AMD backend cannot quietly take
over a board that has `pwm1`. What it has not had is a run on Intel hardware since these
changes; if you have one, `scripts/verify-fan-recovery.sh` is the first thing to try. One
deliberate difference from upstream: the lowest non-zero fan duty is now 33/255 on every
board, not 30 — see [Safety](#safety).

**Anything else is refused fan control, with a reason.** A sibling of a measured board —
same board-name family, e.g. another `FRANMGCP` revision — is used with its family's profile
and says so in the log. An unrecognised board gets monitoring, profiles and the charge limit,
but not the fan: there is no floor that can be trusted to stay above its firmware. The
refusal names the probes that would measure it.

Measured on: Arch Linux, kernels 7.1.11 and 7.2.6, BIOS 03.05 and 04.02, EC firmware
`lilac-3.0.5` and `lilac-4.0.2`.

## Safety

Manual fan control is genuinely risky: once userspace takes over, the EC stops managing the
fan and holds the last duty written, so a crashed daemon can leave it stuck low under load.
[ADR 0006](docs/adr/0006-fail-safe-fan-control.md) specifies the mitigations.

**This board weakens three of them, and [ADR 0013](docs/adr/0013-fan-control-via-ec-commands.md)
records exactly how** rather than leaving the safety story reading as intact:

- **Duty writes cannot be verified.** There is no duty register to read back. Replaced by
  unconditional periodic re-assertion.
- **There is no fan mode register.** The daemon cannot ask whether it holds the fan, and RPM
  cannot answer — a manual duty of 0 and EC-auto-at-idle both read 0 rpm. So it releases
  unconditionally rather than conditionally, which is weaker as diagnosis and no weaker as
  repair.
- **Firmware's own duty cannot be observed** — but on EC `lilac-4.0.2` its *target rpm* can,
  through `fan1_target`, and it leads the actual speed. The floor learns from that, through
  this board's fan table, and ignores a target of 0 while the fan is plainly turning, which
  means a dead register rather than a silent firmware.

Measured on both AMD boards and worth carrying into any curve: the fan **stalls** below 10%
duty but will not **start** from rest below 11%, and a curve idling in that gap runs
correctly down a whole cooldown then silently fails to spin up from cold. So the lowest
non-zero duty on every board is now **33/255 (13%)** — the break-away plus two points for a
cold or dusty bearing. Also note `ddr_f75303@4d` reports its limit at **79.85 °C**, seven
degrees below the Intel board's.

**`kill -9` recovery is verified on `FRANMGCP09`.** With the fan held at 71% on an idle
machine and the daemon SIGKILLed, so that none of its own release paths could run,
`fw-helper-restore-fan` handed the fan back within 1.29 s and the restarted daemon released
it again as a second layer. `scripts/verify-fan-recovery.sh` repeats it.

**Still unverified on hardware:** suspend and resume while holding the fan, the floor
overriding a quiet curve live under load, and break-away from a cold fan.

## Build and run

```bash
cargo build --release --all
cargo test --all                     # no hardware, no root, no network
```

The GUI and daemon can run on the session bus without root, which is enough for telemetry:

```bash
FW_HELPERD_SESSION_BUS=1 ./target/release/fw-helperd &
FW_HELPERD_SESSION_BUS=1 ./target/release/fw-helper
```

For the charge limit you need the root daemon and its D-Bus policy:

```bash
sudo ./scripts/install-dev.sh --systemd
fw-helperctl status
```

There is no Arch package yet — upstream's `build-deb.sh` targets Debian.

## Poking at your own machine

All read-only unless stated:

```bash
./scripts/fw-probe.sh                     # general survey
sudo ./scripts/probe-power-amd.sh         # is there any usable power telemetry?

gcc -O2 -Wall -o probe-ec-amd  scripts/probe-ec-amd.c
gcc -O2 -Wall -o probe-fan-amd scripts/probe-fan-amd.c

sudo ./probe-ec-amd                       # EC feature flags and the charge limit
sudo ./probe-fan-amd                      # WRITES: spins the fan up, then releases it
sudo ./probe-fan-amd --sweep              # WRITES: duty -> RPM table
sudo ./probe-fan-amd --breakaway          # WRITES: lowest duty that starts the fan
```

The fan probes only ever spin the fan *up*, install their restore handler before the first
write, and release the fan on every exit path including SIGINT. Read them before running
them.

## Prior art

- [Wooloomooloo2/fw-helper](https://github.com/Wooloomooloo2/fw-helper) — upstream, for Intel boards
- [framework-system](https://github.com/FrameworkComputer/framework-system) — Framework's own tooling; the reference for EC host commands
- [fw-fanctrl](https://github.com/TamtamHero/fw-fanctrl) — established fan curve daemon
- [G-Helper](https://github.com/seerge/g-helper) — the original inspiration, for ASUS on Windows
- [Stats](https://github.com/exelban/stats) — the macOS monitor the applet's dropdown is modelled on

## Licence

GPL-3.0, as upstream. See [ADR 0001](docs/adr/0001-separate-repository.md#licensing-note).
