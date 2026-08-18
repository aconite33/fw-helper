# 0009 — Package power telemetry is rate-limited and quantized

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

`/sys/class/powercap/*/energy_uj` is mode `0400`, root-only. This is not incidental
permissions tightening — it is the mitigation for **PLATYPUS / CVE-2020-8694**, in which
unprivileged processes correlated RAPL energy readings with victim activity to recover AES
keys and defeat KASLR.

[ADR 0003](0003-privileged-daemon-split.md) has the daemon read this as root and publish
telemetry over D-Bus, including to unprivileged clients (the GUI). **That partially reverses
the mitigation.** Any client that can subscribe to our telemetry gets a power side channel
that the kernel deliberately withheld from it.

The exposure is not equivalent to raw access — the published signal is derived, averaged, and
slow — but the difference is one of degree, and degree is exactly what the attack depends on.
The PLATYPUS work relied on fine-grained sampling; its effectiveness collapses as resolution
and rate drop.

## Decision

Publish package power as a **derived, rate-limited, quantized** value:

1. **Never expose the raw counter.** `energy_uj` and `max_energy_range_uj` are not on the
   D-Bus interface in any form. Only computed average watts over a completed interval.
2. **Rate limit to 1 Hz.** The daemon may poll faster for its own control loops, but the
   published property changes at most once per second.
3. **Quantize to 0.1 W.** Round before publishing. Sub-100 mW structure is where the useful
   signal for an attacker lives and is of no value in a UI.
4. **No burst or on-demand sampling.** There is no "read power now" method. A client cannot
   drive sampling to correlate with its own activity — the cadence is ours, not theirs.

The GUI needs a number that updates about once a second and renders to one decimal place.
That requirement is fully met by the above, so this costs us nothing real.

## Consequences

**Positive**

- Removes the high-rate, high-resolution primitive the attack class depends on, while
  keeping the feature.
- Point 4 is the load-bearing one: it denies attacker-controlled sampling timing, which is
  worth more than resolution limits alone.
- Documented and deliberate, so a future contributor adding a "high resolution mode" has to
  supersede this ADR rather than quietly reintroduce the exposure.

**Negative**

- No fine-grained power graphs, and no client-side integration for accurate short-window
  energy accounting. Anyone wanting that must run privileged and read the counter directly —
  which is exactly the boundary the kernel already draws.
- A determined local attacker still gains *something* over having no access. We reduce the
  channel; we do not close it. Stated plainly rather than claimed away.

## Correctness requirements (separate from the security decision)

The same counter has two arithmetic hazards, both live on this hardware:

- **Wrap.** `max_energy_range_uj` = 262,143 J — under 3 h at a 25 W load, ~1.2 h at PL2.
  Deltas must add the range back on rollover.
- **Multi-wrap and suspend.** A gap spanning more than one wrap is indistinguishable from a
  short interval, and across suspend the counter may reset while wall-clock advances.
  Neither is recoverable, so both must be **detected and the sample discarded** — never
  interpolated. `EnergySampler` in `fw-helper-core` enforces a maximum sample gap and an
  implausible-wattage ceiling for this reason, and exposes `invalidate()` for the
  resume path.

## Alternatives considered

- **Do not publish power at all.** Safest, and rejected as over-correction: live power draw
  is a genuinely useful readout and the mitigated form is weak as an attack primitive.
- **Publish at full rate, restrict the D-Bus interface to root clients.** Rejected: it makes
  the unprivileged GUI unable to show power, which defeats the purpose of the split.
- **Gate telemetry behind polkit.** Rejected as the primary control — an authorization prompt
  to see a number is poor UX, and users would grant it reflexively, so it buys little.
  Reconsider if a high-resolution mode is ever genuinely needed.
