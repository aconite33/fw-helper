# 0007 — No undervolting; RAPL power limits only

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

Undervolting is a headline G-Helper feature and an obvious thing to want. On this hardware
it is not available.

Intel disabled the undervolting interface (`MSR 0x150`, `OC Mailbox`) as the mitigation for
**Plundervolt / CVE-2019-11157**, where controlled voltage manipulation was used to induce
faults in SGX enclaves and extract keys. From roughly 10th-gen onward the MSR is locked by
firmware, and on a Core Ultra X7 358H it is not reachable. Some vendors expose an unlock in
BIOS; Framework's BIOS 03.02 does not.

The realistic alternatives were:

- Ask Framework to expose an undervolt unlock — out of our control, and asks a vendor to
  re-open a documented security mitigation
- Patch the MSR anyway — blocked by the lock bit; not a software problem

## Decision

**Do not implement undervolting. Do not put a disabled or "coming soon" control in the UI.**

Power tuning is delivered through RAPL package power limits only:

```
/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw   # PL1, sustained
/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw   # PL2, turbo
```

The README states plainly that undervolting is not possible on this platform and why, so
users arriving from G-Helper get an answer instead of assuming the feature is missing.

## Consequences

**Positive**

- Honest scope. No effort spent on a feature that cannot ship.
- Most of the *practical* benefit of undervolting on a laptop — lower sustained temps, longer
  battery, quieter fan — is achievable through PL1/PL2 anyway. Undervolting buys performance
  per watt; power limits buy the thermal and acoustic outcome users actually asked for.
  **This is now measured, not assumed:** dropping PL1 from 25 W to 15 W took the CPU from
  76.8 °C to 64.8 °C under sustained load, with PL1 regulating to within 2% of setpoint.
  See Q6 in the hardware baseline.
- Keeps us clear of a security mitigation, which is the right place to be.

**Negative**

- A real capability gap versus G-Helper on ASUS/AMD hardware. Users who specifically want
  more performance at the same thermals cannot get it here.
- Power limiting is a blunter instrument: it trades away peak performance rather than
  improving efficiency.

## Revisit if

- Framework exposes an undervolt/OC unlock in a future BIOS **and** documents the risk
- The project ever targets AMD Framework boards, where the calculus differs — though note
  that Curve Optimizer is not reliably exposed on AMD mobile parts either, so this would need
  its own investigation rather than an assumption

## Verification status

The replacement mechanism is confirmed working on this board. Q1 showed RAPL writes are not
locked by firmware; Q6 showed PL1 regulates sustained package power to within ~2% of setpoint
via `intel-rapl-mmio:0`. So this ADR trades an impossible feature for a verified one, rather
than for a hoped-for one.
