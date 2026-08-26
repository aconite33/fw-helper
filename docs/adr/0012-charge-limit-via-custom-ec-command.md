# 0012 — Charge limit via Framework's custom EC command

- **Status:** Accepted
- **Date:** 2026-08-26
- **Supersedes:** [0008](0008-charge-limit-via-module-parameter.md)

## Context

[ADR 0008](0008-charge-limit-via-module-parameter.md) chose the standard CrOS EC charge
control command, reached through `charge_control_end_threshold` after forcing the kernel
driver to bind with `probe_with_fwk_charge_control=1`. It explicitly declined to
implement Framework's custom EC command, on [ADR 0004](0004-sysfs-first-hardware-access.md)
grounds.

**That approach never worked.** The threshold is accepted, reads back, persists and
re-applies — and the battery charges past it to full. See ADR 0008's Outcome section for
the measurements and for why its precondition was never satisfiable.

The decisive measurement came from asking the custom command directly, on 2026-08-26,
with the standard threshold sitting at 80%:

```
ioctl request = 0xC014EC00
ioctl rc=2  ec_result=0
custom EC charge limit: max=100%  min=0%
--- sysfs says ---
80
```

**Two independent limits exist, and this is the one that governs.** It had been at its
100% default the whole time. `ec_result=0` also settles a question that would otherwise
have changed the plan entirely: this firmware implements the custom command and answers
it cleanly.

## Decision

**Drive the battery charge limit through Framework's custom EC command**,
`EC_CMD_CHARGE_LIMIT_CONTROL` (`0x3E03`), over `ioctl` on `/dev/cros_ec`.

`charge_control_end_threshold` is no longer read or trusted. It is still *written*, last
and best-effort, so UPower and GNOME do not display a third number that disagrees with
reality — but it is never consulted as evidence of anything.

### The protocol, verified from source

Definitions come from `FrameworkComputer/framework-system`
(`framework_lib/src/chromium_ec/{command,commands}.rs`), read directly rather than
summarised — **an intermediate lookup reported the command id as `0x3E07`, which is
wrong**, and only quoting the enum with its neighbours settled it.

| | |
|---|---|
| Command | `0x3E03`, version 0 |
| Request | 3 bytes: `modes`, `max_percentage`, `min_percentage` |
| Modes | `Disable=0x01`, `Set=0x02`, `Get=0x08`, `Override=0x80` |
| Response | `Get` only, 2 bytes: `max_percentage`, `min_percentage` |

Transport is `CROS_EC_DEV_IOCXCMD_V2` = `_IOWR(0xEC, 0, struct cros_ec_command_v2)`,
from `coreboot/chrome-ec` `util/cros_ec_dev.h`. The struct is five `u32`s
(`version, command, outsize, insize, result`) followed by the payload.

Note that **`max` precedes `min` on the wire**, which is the reverse of how the pair
reads everywhere else in this codebase. Transposing them sets a *minimum* of 80%, and on
a battery already above 80 that failure is indistinguishable from success — so it is
pinned by its own unit test.

### Where the code lives

- `fw_helper_core::ec` — the wire format as pure functions over bytes. No I/O, no
  dependencies, so the encoding is unit-tested with no hardware and no root
  ([ADR 0010](0010-dependency-boundary.md)).
- `fw_helperd::ec` — the `ioctl`, which needs libc and therefore cannot live in core.
  The transport is a trait, injected into `Daemon`.

That trait is the part worth defending. [ADR 0004](0004-sysfs-first-hardware-access.md)'s
rooted-`Sysfs` trick makes hardware replaceable by a fixture tree, and an `ioctl` has no
fixture-tree equivalent. Without a seam the charge-limit tests would have to touch the
real EC, which means they would not run in CI, which is how this feature went untested in
the first place.

## Rationale

ADR 0008 set its own reconsideration trigger: *"if interoperating with the UEFI setting
becomes a requirement rather than something we ask users to avoid."* That condition is
met, and more sharply than anticipated. The custom command is not a setting a user opts
into — it is what the EC firmware runs regardless. There is no configuration in which the
standard command wins on this board.

This does not weaken ADR 0004. That ADR ranks sysfs first *and provides for raw EC
commands as a fallback*; this is the first thing to reach the second tier, and it does so
with a measurement rather than a preference. Nothing else should follow it without the
same evidence.

## Consequences

**Positive**

- The feature works, having never worked before.
- No module parameter, no drop-in, no reboot, and no opt-in install step. The capability
  now depends only on `/dev/cros_ec`, which is present by default.
- Interoperates with the UEFI battery limit by construction: both drive the same
  mechanism, so the last writer wins in a way the user can predict.
- Read-back is meaningful for the first time, because it reads the mechanism that
  governs charging.

**Negative**

- We now own an EC command definition that is not a stable ABI, from a firmware fork.
  Pinned by tests, and it fails loudly (`EcError::Rejected`) rather than silently if it
  ever moves.
- Requires root and `cros_ec_chardev`. Both already hold — the daemon is root by design
  ([ADR 0003](0003-privileged-daemon-split.md)).
- Capability detection for this knob no longer lives in `Capabilities::probe`, which is
  sysfs-only by design. The daemon replaces core's verdict at startup. Slightly awkward,
  and preferable to core growing a dependency.
- `probe_with_fwk_charge_control` and the modprobe drop-in are now pointless. Left in
  place for this change and removed separately, since removing them is an install-story
  change with an uninstall path of its own.

## Verification

**Done:**

- The custom command answers on this firmware: `ec_result=0`, `max=100 min=0`, against a
  sysfs threshold of 80 — the measurement that motivated this ADR.
- Wire format, byte order, read-modify-write of the minimum, and rejection handling are
  unit-tested against a fake transport.

- **Charging stops.** Measured 2026-08-26, and this is the check ADR 0008 never made.
  With the limit at 80% and the EC reporting `max=80`, the battery charged from below the
  limit on AC and stopped at exactly 80%: `capacity=80`, `status=Not charging`,
  `current_now=0`, `charge_now` 3 859 000 of `charge_full` 4 821 000, with AC still
  connected. The mechanism it replaces, measured the same way, ran 88% \u2192 93% \u2192 100%
  with `status=Charging` throughout. The only variable changed between the two runs is
  the custom EC command's `max`, from 100 to 80.

**Read-back is not efficacy** \u2014 it was not under ADR 0008 and it is not now. What closes
this ADR is the paragraph above, not the read-back above it.

`scripts/q2-charge-limit-efficacy.sh` exists so this is repeatable rather than a story
about one afternoon. It refuses to run on battery, and refuses to run when the battery is
already above the limit \u2014 the condition under which the superseded mechanism looked
fine for weeks.

## Alternatives considered

- **Keep the standard command and ask users to set the UEFI limit.** Rejected: it makes a
  headline feature a documentation note, and leaves a control in the GUI that cannot work.
- **Write a kernel driver.** Correct in the long run and out of scope here; upstream's
  position is that Framework should reconcile the two commands.
- **Shell out to `framework_tool`.** Rejected for the same reason as in ADR 0008: not
  packaged for Ubuntu. Its source is the reference for the protocol, which is a different
  thing from being a runtime dependency.

## Sources

- [FrameworkComputer/framework-system — `chromium_ec/command.rs`](https://github.com/FrameworkComputer/framework-system/blob/main/framework_lib/src/chromium_ec/command.rs)
- [FrameworkComputer/framework-system — `chromium_ec/commands.rs`](https://github.com/FrameworkComputer/framework-system/blob/main/framework_lib/src/chromium_ec/commands.rs)
- [coreboot/chrome-ec — `util/cros_ec_dev.h`](https://github.com/coreboot/chrome-ec/blob/main/util/cros_ec_dev.h)
