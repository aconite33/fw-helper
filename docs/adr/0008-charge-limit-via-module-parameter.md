# 0008 — Charge limit via `probe_with_fwk_charge_control`, not a custom EC command

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

`BAT1` has no `charge_control_end_threshold` despite `cros_charge_control` being loaded.
`dmesg` gives the reason directly:

```
cros-charge-control cros-charge-control.6.auto: Framework charge control detected, preventing load
```

This is deliberate upstream behaviour, not a bug. From the LKML series that added the driver:

> Framework laptops implement a custom charge control EC command, while the upstream CrOS EC
> command is also present and functional but can get overridden by the custom one. Until
> Framework make both commands compatible or remove their custom one, the driver doesn't load
> on those machines.

So the board has **two** working charge-control mechanisms:

1. **Framework's custom EC command** — what the UEFI setup screen's battery limit uses, and
   what `framework_tool` drives
2. **The standard CrOS EC command** — present and functional, but the custom one can override it

The driver declines to load rather than race the firmware. Upstream provides an opt-out:

```
/sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control   # bool, 0644, currently N
```

> If the user knows they are not going to use the custom command they can use a module
> parameter to load cros_charge-control anyways.

## Decision

**Load `cros_charge-control` with `probe_with_fwk_charge_control=1`** and use the resulting
standard `charge_control_end_threshold` sysfs attribute.

Ship a modprobe drop-in:

```
# /etc/modprobe.d/fw-helper.conf
options cros_charge-control probe_with_fwk_charge_control=1
```

**Do not** implement Framework's custom EC command.

The condition upstream attaches to the parameter — "if the user knows they are not going to
use the custom command" — becomes an install-time requirement we own: **fw-helper is the
charge-limit authority, and the UEFI battery limit setting must be left at default.**

## Rationale

This follows [ADR 0004](0004-sysfs-first-hardware-access.md) exactly. The standard sysfs
attribute is the sanctioned interface; the module parameter is upstream's own supported
escape hatch, not a hack around it. We also get UPower and any other standard consumer
seeing the same value, for free.

Implementing the custom command instead would mean shipping a second, parallel
implementation of a mechanism the kernel already exposes, in order to interoperate with a
BIOS setting we are asking the user not to use anyway.

## Consequences

**Positive**

- M2 becomes a sysfs read/write. No `ioctl`, no host command encoding, no
  `framework_system` dependency for this feature.
- Standard interface, so UPower/GNOME and monitoring tools stay consistent with us.
- Dies naturally: if Framework ever reconciles the two commands upstream, the driver loads
  on its own and our modprobe drop-in becomes a no-op.

**Negative**

- **Requires a module reload, and therefore an install step with a reboot** (or an explicit
  `modprobe -r` / `modprobe`). Not a pure userspace install.
- **Conflicts with the UEFI battery limit setting.** If the user has set one there, the
  custom command may override us unpredictably. The daemon must detect a mismatch between
  the requested and observed limit and surface it rather than silently losing.
- Ties us to a module parameter whose name is not a stable ABI. Detect its presence; do not
  assume it.

## Implementation notes

- Probe order: use `charge_control_end_threshold` if present → else check whether the
  parameter exists and tell the user how to enable it → else report the capability
  unavailable ([ADR 0003](0003-privileged-daemon-split.md)). **Never fail silently.**
- After writing a limit, read it back. A persistent mismatch means the custom command is
  fighting us — surface it as "a BIOS battery limit appears to be set", which is actionable,
  rather than as a generic error.
- Installer must not enable this without consent: it changes how the machine's charge limit
  is governed. Make it an explicit opt-in step, not a postinst side effect.

## Alternatives considered

- **Implement Framework's custom EC command over `/dev/cros_ec`.** Rejected as the default:
  duplicates a kernel-provided mechanism and contradicts ADR 0004. Reconsider if the module
  parameter is ever removed, or if interoperating with the UEFI setting becomes a
  requirement rather than something we ask users to avoid.
- **Shell out to `framework_tool`.** Rejected — see ADR 0004; it is not packaged for Ubuntu.
- **Ship no charge limit at all.** Rejected: it is the single most requested laptop battery
  feature and the cheapest to deliver here.

## Sources

- [PATCH v5 5/5 — power: supply: cros_charge-control: don't load if Framework control is present](https://lkml.iu.edu/2406.3/08725.html)
- [PATCH v4 5/5 — same series, v4](https://lkml.iu.edu/hypermail/linux/kernel/2406.2/00329.html)
- [PATCH v4 0/5 — ChromeOS Embedded Controller charge control driver](https://lkml.rescloud.iu.edu/2406.2/00328.html)
