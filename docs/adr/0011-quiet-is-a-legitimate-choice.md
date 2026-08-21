# 0011 — The fan floor tracks firmware's own behaviour, not the CPU's safety

- **Status:** Accepted
- **Date:** 2026-08-21
- **Amends:** [0006](0006-fail-safe-fan-control.md) point 4

## Context

ADR 0006 point 4 says our curve "may only ever be *more* aggressive than firmware, never
less", so that a badly drawn curve is bounded to "louder than necessary". The reasoning
was that a fan held too low under load could drive sustained high temperatures.

Three measurements taken on 2026-08-21, after the clamp was built, undercut parts of that
reasoning and one of them contradicts an assumption it rested on.

**The CPU protects itself.** `coretemp` reports `temp*_crit` as **100 °C** on every core
and on the package — Tjmax. Past it the CPU throttles. Constraining the fan therefore
costs *performance*, not hardware: a user who wants a quiet machine and accepts a slower
one is making a legitimate trade, not a dangerous mistake. Note also that `peci-temp`
reports crit as 119.85 °C, *above* Tjmax, so a limit derived from that sensor alone
describes a temperature the CPU never reaches.

**Firmware's own curve has enormous hysteresis.** Measured at 61.9 °C, the EC runs duty
**0** while the temperature is rising and duty **92** while it is falling, and it does not
stop the fan until below 44.9 °C. Climbing from cold under load it kept the fan entirely
off past 64.8 °C. "Never quieter than firmware" is therefore ambiguous: quieter than which
branch? Tracking the descending branch would forbid a silent idle for roughly ten minutes
after any load — which is most of what the feature is for.

**The numbers the thresholds were built on were unrepresentative.** `FULL_DUTY_ABOVE_C`
(85 °C) and the fallback ceiling (90 °C) were both justified by "this machine runs at
76.8 °C under sustained full load", taken from the M0 PL1 test. Measured under ordinary
multi-core load with firmware driving, `peci-temp` reached **92.8 °C** while firmware
chose duty 94/255. The fallback ceiling sat *below* normal operation, and the floor would
have pinned the fan at full duty — roughly 5200 rpm against firmware's 3321 — in a band
the machine uses routinely.

Separately: **`pwm1` reports firmware's own duty** while `pwm1_enable=2`, confirmed across
60 samples whose RPM matched the measured duty→RPM table to within 2.5% on average. The
floor no longer has to be inferred at all.

## Decision

1. **The floor is read, not modelled.** While the EC owns the fan, record the duty it
   chose at each temperature. The composed RPM tables survive only as a cold start, and
   any direct observation supersedes them.

2. **Only the ascending branch is recorded.** Firmware's descending duty is hysteresis —
   an anti-oscillation measure while shedding heat — not an answer to "what does this
   temperature require". Recording it would ratchet the floor to a duty firmware itself
   only uses on the way down.

3. **An observation of duty 0 is an answer, not a gap.** Firmware running the fan off at
   60 °C while heating is firmware's judgement that no airflow is needed there, and we
   honour it. This is what makes a genuinely silent machine reachable.

4. **Thresholds are set from measured operating temperatures**, not from an assumed peak:
   full duty above 95 °C, ceiling capped at Tjmax (100 °C), fallback ceiling 97 °C.

5. **The floor bounds a user's curve against firmware's own behaviour, not against CPU
   damage.** The CPU's protection is its own. What this does not yet address is stated
   plainly below.

## Consequences

**Positive**

- A silent idle, and silence well up the temperature range, become reachable — firmware
  itself is silent to ~65 °C while heating, and we may now be too.
- The floor stops inheriting the interpolation error of two composed tables.
- Thresholds no longer fire during ordinary work.
- Point 4's guarantee is now *checkable*: it is "not quieter than firmware was observed to
  be while heating at this temperature", which is a measurement rather than a model.

**Negative**

- The floor is now weaker than ADR 0006 implied. A user's curve can be quieter than
  firmware's descending branch. The defence is that the CPU throttles at Tjmax, which is a
  performance consequence the user chose.
- The floor depends on observations accumulated at run time. A daemon restarted mid-load
  starts from the cold-start model, which is the loud direction, but is still a
  discontinuity in behaviour.
- **The battery is not covered by any of this, and it is the real exposure.**
  `battery_temp` reports crit at **49.9 °C**, by far the lowest threshold on the board,
  and unlike the CPU it has no throttling of its own — a hot chassis degrades it silently
  over time. Board and DDR sensors sit at ~87 °C with the same lack of self-protection.
  Nothing currently watches them. **A quiet mode should be bounded by the components that
  cannot protect themselves, not by the CPU that can.** That is the next piece of work
  this ADR implies and does not do.

## Notes

ADR 0006's other five points are untouched: releasing on every exit path, the crash-path
binary, the watchdog, the ceiling override, and refusing manual control without a sensor.
All six are verified on hardware. What changes here is only *how low the floor sits* and
why.

## Measured afterwards

Appended after the fact; the decision above is unchanged.

- **The battery claim above was asserted, not measured, and the measurement weakens it.**
  This ADR calls the battery "the real exposure". Measured 2026-08-21 across five minutes
  of 16-core load with firmware driving the fan, on battery power so discharge heating
  counted too, `battery_temp` rose from **31.9 °C to 33.9 °C** while the CPU went from
  40.9 °C to 78.8 °C. A 2 °C rise, leaving **16 °C of headroom** below its 49.9 °C crit.
  The battery is well isolated from the CPU and lags heavily — it was still creeping
  upward during the cooldown. On this evidence it is in no danger from CPU heat.
- **Airflow does reach it, though.** During the post-load cooldown, with the fan still
  running hard, the battery fell to **26.9 °C — below its 31.9 °C idle baseline**. So
  raising the fan is a real lever on battery temperature, not a hopeful one.
- **What remains unmeasured is the case that matters**: the same load with the fan held
  *low* for far longer than five minutes, which is exactly what a user-authored curve
  will permit. The guard implemented in `fw-helper-core/src/battery.rs` is a backstop for
  that, sized so it does not fire at any temperature yet observed. It should essentially
  never trigger; if it does, either the thresholds are wrong or the situation is genuinely
  new, and both are worth knowing.
