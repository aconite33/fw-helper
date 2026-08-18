# 0002 — Rust for the daemon, GTK4/libadwaita for the GUI

- **Status:** Accepted
- **Date:** 2026-08-18

## Context

Nothing carries over from G-Helper's C#/WinForms stack ([0001](0001-separate-repository.md)),
so the implementation language is a free choice. Requirements:

- A long-running root daemon touching sysfs and, as fallback, `ioctl` on `/dev/cros_ec`
- A desktop GUI that looks native on Ubuntu (GNOME)
- Ideally, reuse of existing Framework ecosystem code

## Decision

**Rust** for both the daemon and the GUI. GTK4 + libadwaita via `gtk4-rs` for the UI layer,
`zbus` for D-Bus.

## Rationale

The deciding factor is ecosystem alignment, not language preference:

- `framework_system` / `framework_tool` (Framework's own tooling) is Rust and usable as a
  library crate, not just a CLI — this is our reference for EC host command encoding
- `inputmodule-rs` (FW16 LED matrix) is Rust, if we ever extend to FW16
- `zbus` is a mature pure-Rust D-Bus implementation with good systemd integration
- One language across daemon and GUI means shared types in `fw-helper-core` with no FFI
  or serialization boundary beyond D-Bus itself

Memory safety matters more than usual here: this is root-privileged code writing to
hardware registers.

## Consequences

**Positive**

- Shared type definitions between daemon and GUI, compile-time checked.
- Single static-ish binary per component; straightforward `.deb` packaging.
- Direct reuse of `framework_system` for the EC fallback path.

**Negative**

- `gtk4-rs` is more verbose than PyGObject for UI work. Expect the GUI to cost more effort
  per screen than the daemon does per feature.
- Ubuntu 24.04 ships an older `rustc` than we want. Use `rustup`; pin via `rust-toolchain.toml`.
- Longer compile times than a Python prototype during early iteration.

## Alternatives considered

- **Python + PyGObject.** Faster to prototype, and `fw-fanctrl` proves it works. Rejected for
  the daemon: packaging a root Python service with dependencies is worse than shipping a
  binary, and we lose `framework_system` reuse. Still a reasonable choice for throwaway
  spike scripts.
- **C# / .NET + Avalonia.** Would let concepts transfer more literally from G-Helper, and
  Avalonia runs on Linux. Rejected: no meaningful code reuse anyway, a heavier runtime
  dependency, and it is culturally alien to the Framework/Linux ecosystem we want to
  borrow from and contribute back to.
- **Qt/QML.** Fine technically. Rejected: libadwaita is the better fit for stock Ubuntu GNOME,
  including dark-mode and accent-colour following.
