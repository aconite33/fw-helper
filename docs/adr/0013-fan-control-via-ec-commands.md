# 0013 — Fan control via raw EC commands, without read-back

- **Status:** Accepted
- **Date:** 2026-08-29
- **Amends:** [0006](0006-fail-safe-fan-control.md) (point 3, and the read-back requirement)
- **Applies to:** the AMD Framework 13 (`FRANMGCP05`, EC `lilac-3.0.5`). The Intel board
  keeps the sysfs path unchanged.

## Context

Every fan write in this project goes through two `cros_ec` hwmon attributes: `pwm1_enable`
(1 = manual, 2 = EC automatic) and `pwm1` (duty 0–255). [ADR 0006](0006-fail-safe-fan-control.md)
is built on both — it releases the fan by writing `pwm1_enable=2`, and it verifies every
duty write by reading `pwm1` back.

**On this board neither attribute exists.** Measured 2026-08-29, `cros_ec` hwmon exposes
`fan1_input`, `fan1_target`, `fan1_fault`, four temperatures, and nothing else. There is no
mode register and no duty register. `Capabilities::probe` correctly reports
`Cap::No("cros_ec hwmon present but exposes no pwm1_enable")`, and every fan D-Bus method
refuses.

The fan is nonetheless controllable. `EC_FEATURE_PWM_FAN` is advertised
(`flags[0]=0x0207E6AE`, bit 2), and the EC's own commands work — verified on hardware before
any code was written:

```
baseline (EC owns fan)   rpm 0
manual 40% duty          rpm 3034 -> 4012
manual 70% duty          rpm 5682 -> 6105
released to EC auto      rpm 5313 -> 0 within 2 s
```

Both halves matter. Duty writes move the fan, **and** `EC_CMD_THERMAL_AUTO_FAN_CTRL` takes it
back promptly — which is the precondition ADR 0006 sets before any daemon may hold the fan.

What the EC does *not* offer is any way to read back what it is doing.
`EC_CMD_PWM_GET_FAN_TARGET_RPM` returned `0` throughout — under manual control and after
release alike. That is the Intel board's `fan1_target` trap arriving through a different
interface. `fan1_input` reports real RPM and is the only feedback of any kind.

## Decision

**Drive the fan through raw EC commands on this board**, behind a `FanBackend` trait so the
sysfs path and the EC path are two implementations of one contract.

| Command | Id | v0 parameters |
|---|---|---|
| `EC_CMD_PWM_SET_FAN_DUTY` | `0x0024` | `{ uint32_t percent; }` |
| `EC_CMD_THERMAL_AUTO_FAN_CTRL` | `0x0052` | none |

Verified against `torvalds/linux` `include/linux/platform_data/cros_ec_commands.h`, quoted
with their neighbours rather than recalled — ADR 0012 paid for that lesson once already
(`0x3E07` vs `0x3E03`). Pinned by unit test.

Code splits exactly as ADR 0012 established: wire format in `fw_helper_core::ec::fan`
(dependency-free, testable without hardware), `ioctl` in `fw_helperd::ec`, reusing the
existing `EcTransport` trait.

**Duty is a percentage, 0–100.** Not an 8-bit count. `255` is not "full duty" here, it is out
of range. Every duty constant in the tree is rescaled, and `u8::MAX` as a "maximum" sentinel
becomes `100`.

### Three things ADR 0006 requires that this board cannot provide

Each is a genuine reduction. They are recorded rather than worked around, because the
alternative is a safety story that reads as intact while resting on nothing.

**1. Duty writes cannot be verified. Re-assert instead.**

The project's hard rule is *write, then read back and verify — a silent override is the
expected failure here*. There is no duty register to read. `DUTY_TOLERANCE` and
`DutyNotApplied` become meaningless and are removed rather than left as decoration.

Replacing them: **unconditional periodic re-assertion.** The governing loop rewrites its
target duty on every tick regardless of what it believes the current state to be. This is
the pattern the PL1 work already arrived at from the other direction — *"re-assert on a
timer; an immediate read-back cannot see it"* — and it is strictly more robust than
verify-and-correct against a firmware that changes things behind us, because it needs no
detection step at all.

What is lost is narrower than it looks: detecting that *something else* moved the fan. Only
another EC client could, and it would be corrected within one tick anyway.

**2. There is no mode register. Release unconditionally.**

`FanControl` documents itself as holding no state: *"every question is answered by asking the
hardware. That matters for the recovery paths, which must work when in-process state is
exactly what cannot be trusted."* Two paths depend on that — `reclaim_at_startup`, asking
whether a previous instance died holding the fan, and the watchdog, asking whether the fan is
still held while the heartbeat is stale.

On this board that question has no answer. RPM cannot substitute: a manual duty of 0 and
EC-auto-at-idle both read 0 rpm, and those are precisely the two states that must be told
apart.

**So stop asking.** `AUTO_FAN_CTRL` is idempotent and cheap — handing the fan to an EC that
already owns it is a no-op. The daemon therefore:

- issues a release at startup, unconditionally, before anything else;
- issues a release on every watchdog tick where the heartbeat is stale, unconditionally,
  rather than first testing whether it holds the fan.

This is **point 3 of ADR 0006 amended**: recovery no longer proves the fan was stuck before
unsticking it. It is weaker as *diagnosis* and no weaker as *repair*, and repair is the
property that keeps a fan turning.

`release()` also loses its verification. It learns only that the EC accepted the command
(`ec_result == 0`), never that the fan is now firmware-managed. A rejected command is still
loud; a lie from the EC is now undetectable.

**3. Firmware's own duty cannot be observed. Observe RPM instead.**

[ADR 0011](0011-quiet-is-a-legitimate-choice.md)'s floor learns what firmware does by reading
`pwm1` while the EC owns the fan — firmware's real duty, no modelling. Unavailable here.

Observations are therefore recorded in **RPM**, from `fan1_input`, and converted to a duty on
demand through the measured duty→RPM table. That is **one** inversion. The Intel board's
*modelled* fallback composed two tables (temperature→RPM, then RPM→duty); this path drops the
first. So it sits between the two: worse than reading firmware's duty directly, better than
the model it replaces.

The inversion must interpolate the measured table and **must not fit a line to it**. Measured
on this board, a line through the high points predicts 2982 rpm at duty 10 where the real
answer is 967 — inverting that fit would return a duty roughly threefold too low, placing the
floor *below* firmware. That is the one direction that is not safe.

### `STICTION_DUTY` carries the break-away number, not the stall number

Measured 2026-08-29, and the two differ:

| | Duty 10% | Duty 11% |
|---|---|---|
| Already spinning | **967 rpm** | ~1100 rpm |
| From rest | **will not start** | **1098 rpm** |

Duty 10 sustains rotation and cannot begin it. A curve idling there would run correctly down
an entire cooldown and then silently fail to spin up from cold — ADR 0006's central failure
arriving through arithmetic rather than through a crash, and undetectable by definition,
since a fan that never starts sounds exactly like a quiet curve working well.

`STICTION_DUTY = 13`: the measured 11 plus two points of margin. The margin is deliberate and
is *not* a measurement — it covers a single observation taken warm, at one temperature, on a
clean fan, where bearing drag rises when cold and with dust, and where the failure direction
is silent. It costs about 235 rpm of minimum speed. **Re-measure from cold before treating
13 as settled.**

## Consequences

**Positive**

- Fan control exists at all on this board, having been unavailable.
- One `FanBackend` trait covers both boards; the sysfs path is unchanged and the Intel fork
  keeps its read-back.
- Percent duty removes the 8-bit→percent→8-bit quantization the Intel path spends
  `DUTY_TOLERANCE` absorbing. The scale the EC stores is now the scale we send.
- Unconditional release is simpler than conditional release and has fewer states to be wrong
  about.

**Negative**

- **Three of ADR 0006's guarantees are weaker**, as above. This is the cost of the board, not
  a choice between designs — there is no stronger option available on this hardware.
- `fw-helper-restore-fan` gains a `libc` dependency. It was deliberately built with exactly
  one dependency and "no allocation it can avoid", because it runs as `ExecStopPost` when the
  daemon has already failed. An `ioctl` is the only way to release the fan here, so the cost
  is unavoidable; hand-rolling a raw syscall in the crash-path binary would be worse.
- We depend on two more EC command definitions that are not a stable ABI. They fail loudly
  (`EcError::Rejected`) rather than silently if they move, and are pinned by tests.
- Capability detection for the fan leaves `Capabilities::probe`, which is sysfs-only by
  design ([ADR 0010](0010-dependency-boundary.md)). The daemon overrides core's verdict at
  startup, exactly as it already does for the charge limit.

**Neutral**

- This is the second interface to reach [ADR 0004](0004-sysfs-first-hardware-access.md)'s
  raw-EC tier, and like the first it arrives with a measurement rather than a preference: the
  sysfs attributes are absent, not merely inconvenient. Nothing else should follow without
  the same evidence.

## Verification

**Done:**

- The commands work and the EC reclaims the fan cleanly — the measurement above, which is
  what allowed this ADR to be written at all.
- Duty→RPM characterized across 21 points; stall at 8–10%, break-away at 11%.
- Wire format, little-endian encoding, percent clamping and empty release payload are
  unit-tested against a fake transport.

**Not yet done — none of these may be assumed:**

- **`kill -9` recovery**, which ADR 0006 makes a release gate. Unconditional startup release
  is the mechanism; it has not been exercised on this board.
- **Suspend/resume**, including whether the EC keeps or drops a manual duty across s2idle.
- **Whether the EC's curve here is hysteretic** the way the Intel board's is (duty 0 heating
  and 92 cooling at the same 61.9 °C). This decides where a custom curve wins, and is
  unmeasured.
- **Break-away from cold**, per the margin note above.
