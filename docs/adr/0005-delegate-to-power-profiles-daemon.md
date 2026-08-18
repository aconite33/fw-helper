# 0005 — Delegate profile switching to power-profiles-daemon

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

power-profiles-daemon (PPD) is active on the target machine and ships by default on Ubuntu
GNOME. It already drives both levers we care about:

```
$ powerprofilesctl list
  performance:  CpuDriver: intel_pstate   PlatformDriver: platform_profile
* balanced:     CpuDriver: intel_pstate   PlatformDriver: platform_profile
  power-saver:  CpuDriver: intel_pstate   PlatformDriver: platform_profile
```

GNOME's power slider and the battery menu are wired to PPD. If `fw-helperd` writes
`/sys/firmware/acpi/platform_profile` or `energy_performance_preference` directly, we get
last-writer-wins: the GNOME slider silently overrides us, or we silently override it, and the
UI shows a state that is not real.

This is the single most likely source of "the app doesn't work" bug reports.

## Decision

**Do not fight PPD. Delegate to it, and layer on top.**

`fw-helper` profiles are defined as a PPD profile *plus* the knobs PPD does not manage:

```
profile "Quiet" {
    ppd_profile   = power-saver     # -> PPD sets platform_profile + EPP
    fan_curve     = quiet           # ours (hwmon pwm1)
    pl1_uw        = 15_000_000      # ours (powercap)
    pl2_uw        = 35_000_000      # ours
    charge_limit  = 80              # ours
}
```

The daemon calls PPD's D-Bus API (`net.hadess.PowerProfiles`) rather than writing
`platform_profile` itself. It also **subscribes** to PPD's `ActiveProfile` property: when the
user moves the GNOME slider, `fw-helperd` applies the matching fan curve and power limits.

If PPD is not installed or not running, `fw-helperd` falls back to writing `platform_profile`
and EPP directly, and reports that in `Capabilities`.

## Consequences

**Positive**

- The GNOME power slider keeps working and stays truthful. Users are not forced to choose
  between the desktop's own UI and ours.
- We inherit PPD's `HoldProfile` semantics — applications that request a temporary
  performance hold (games, video encoders) still work.
- Much smaller surface: we do not re-implement CPU governor policy, which PPD does well.
- Avoids the `tlp` vs PPD conflict class entirely; we are a PPD client, not a competitor.

**Negative**

- Hard dependency on a running PPD for the primary path, plus a fallback path to maintain
  and test — two code paths for one feature.
- We are limited to PPD's three profiles as the top-level axis. Finer-grained user profiles
  must be expressed as different fan/power layers over the same PPD profile.
- PPD's D-Bus name has moved historically (`net.hadess.PowerProfiles` →
  `org.freedesktop.UPower.PowerProfiles`). Support both, prefer the newer.

## Alternatives considered

- **Mask PPD and take over `platform_profile`.** This is what several tuning tools do.
  Rejected: it breaks the GNOME power slider, which is a worse regression than any feature
  we would gain. It also makes the app feel like malware to anyone who did not read the docs.
- **Write `platform_profile` directly and hope.** Rejected — this is the last-writer-wins bug
  described above, and it is nondeterministic.
- **Ignore profiles entirely; expose only fan and power.** Rejected: profile switching *is*
  the product concept inherited from G-Helper. Without it we are a fan applet.
