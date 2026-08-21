# 0006 — Fan control must fail safe, never fail silent

- **Status:** Accepted — **point 4 amended by [0011](0011-quiet-is-a-legitimate-choice.md)**
- **Date:** 2026-08-18

## Context

The Framework EC's fan curve lives in EC firmware and cannot be replaced at runtime. The only
available mechanism is:

```
echo 1   > /sys/class/hwmon/hwmon11/pwm1_enable   # take manual control
echo 128 > /sys/class/hwmon/hwmon11/pwm1          # hold this duty
```

Once `pwm1_enable=1`, **the EC stops managing the fan and holds whatever duty was last
written, indefinitely.** The failure mode is severe and asymmetric:

- Daemon crashes while duty is low → fan stays low under load → thermal throttling, and in
  the worst case sustained high temperatures with no audible warning
- Daemon crashes while duty is high → fan stays loud → annoying, harmless

The user cannot tell the difference by looking. A silently-stuck-low fan is the single most
dangerous thing this application can do.

## Decision

Manual fan control is treated as a **lease**, not a state. Specifically:

1. **Restore on every exit path.** `pwm1_enable=2` on clean shutdown, on `SIGTERM`/`SIGINT`,
   and on panic (via a panic hook). systemd unit gets
   `ExecStopPost=/usr/libexec/fw-helper-restore-fan`, which is a tiny separate binary that
   does nothing but write `2` — so it works even if the daemon is unrecoverably broken.

2. **Restore across suspend.** Subscribe to logind `PrepareForSleep`. Release control before
   suspend, re-acquire after resume. Do not assume EC state survives S3/s2idle.

3. **Watchdog.** The control loop refreshes a deadline every tick. A separate thread with its
   own timer restores EC control if the deadline is missed by >5 s. This covers deadlock and
   scheduler starvation, which `ExecStopPost` does not.

4. **Never write a duty below the EC's own floor for the current temperature.** Our curve may
   only ever be *more* aggressive than firmware, never less. This bounds the damage from a
   bad user-authored curve to "louder than necessary".

5. **Temperature ceiling override.** Above a hard threshold, release manual control entirely
   and let the EC do its job. User configuration does not get a vote above this line.
   Derive the threshold from `temp*_crit` at runtime, **not** `temp*_max` — on this board
   every `temp*_max` reads `-273150` (unset), so a naive read of it would produce a ceiling
   of absolute zero and disable manual control permanently. Validate the value is sane
   (0–150 °C) before trusting it, and fall back to a conservative constant if it is not.

6. **Refuse to start manual control if the sensor is unreadable.** No temperature, no manual
   fan. Fall back to EC automatic.

## Consequences

**Positive**

- The dangerous failure mode is engineered out rather than documented around.
- Point 4 means a user cannot silently cook their machine with a badly drawn curve, which
  makes it safe to expose curve editing in the GUI at all.
- Point 5 gives a defensible answer to "is this safe?" — the firmware's thermal protection is
  never actually removed.

**Negative**

- Meaningfully more implementation work than a naive poll-and-write loop.
- Points 4 and 5 mean the fan will sometimes ignore the user's curve. The GUI must show
  *when* an override is active and why, or it will read as a bug.
- Requires an extra installed binary (`fw-helper-restore-fan`) that exists solely for the
  crash path.

## Notes

`fw-fanctrl` handles some of this (restore on exit, resume hooks) and is worth reading as
prior art. It does not implement the watchdog or the firmware-floor clamp.

Test plan must include: `kill -9` the daemon under load and verify the fan recovers.
