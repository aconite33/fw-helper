# 0001 — Separate repository, not a fork of G-Helper

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

G-Helper is an existing, working application that does what we want, for ASUS laptops on
Windows. The obvious-looking move is to extend it with Linux/Framework support.

The local checkout at `~/Dev/GitHub/g-helper` is a **fork** of `seerge/g-helper` with an
`upstream` remote configured and actively tracked.

Inspection of the codebase:

- `app/GHelper.csproj` targets `net8.0-windows` with `UseWindowsForms=True`
- Hardware access is entirely through `app/AsusACPI.cs`, which P/Invokes ASUS's proprietary
  WMI/ACPI interface (`\_SB.ATKD.WMNB`) with ASUS-specific device IDs
- Large portions are irrelevant to Framework: `AniMatrixControl.cs`, `NvidiaGpuControl.cs`,
  `GladiusII*.cs` / `Keris*.cs` / `Harpe*.cs` (ASUS mice), `AllyControl.cs` (ROG Ally)

## Decision

Create a new, independent repository. Do not modify the G-Helper fork.

## Consequences

**Positive**

- The fork stays clean: `git fetch upstream && git merge` continues to work, and
  contributing back to `seerge/g-helper` remains possible.
- Free choice of language, build system, packaging, and release cadence.
- No dead Windows-only code to carry or explain.

**Negative**

- No code reuse. Everything is written from scratch.
- Two repositories to maintain if we ever want shared concepts.

**Neutral**

- What we *do* inherit from G-Helper is the product design: a single tray application that
  unifies scattered firmware knobs into named profiles with hotkey switching. That is the
  valuable part and it transfers as a specification, not as source.

## Licensing note

G-Helper is GPL-3.0. Since we share no code, we are unconstrained — but we adopt GPL-3.0
anyway to keep the option of borrowing from `fw-fanctrl` (GPL-3.0). `framework_system` is
BSD-3-Clause and is compatible either way.

## Alternatives considered

- **Add a Linux backend to the G-Helper fork.** Rejected: poisons the upstream relationship
  for zero code reuse, and `net8.0-windows`/WinForms cannot be made to build on Linux.
- **Fork `fw-fanctrl` and grow it.** Rejected: it is a single-purpose Python fan daemon built
  around shelling out to `ectool`. See [0004](0004-sysfs-first-hardware-access.md) — that
  dependency is unnecessary on this kernel, so we would inherit an architecture we intend
  to discard.
