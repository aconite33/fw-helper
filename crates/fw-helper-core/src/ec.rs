//! Framework's custom EC charge-limit command, expressed as bytes.
//!
//! Encoding and decoding only — no I/O. The transport is an `ioctl` on
//! `/dev/cros_ec`, which needs libc and therefore lives in the daemon (ADR 0010).
//! Keeping the wire format here means the fiddly part — which is the byte *order* —
//! is unit-tested with no hardware and no root.
//!
//! Why this exists at all: the standard CrOS EC charge command is present on this
//! board and does nothing, because Framework's EC also implements a custom one that
//! overrides it. Measured 2026-08-26 with the standard threshold at 80%, this command
//! reported `max=100 min=0` — two independent limits, and this is the one that
//! governs. See ADR 0012, and the Outcome section of ADR 0008 for what it replaces.
//!
//! Definitions verified against `FrameworkComputer/framework-system`,
//! `framework_lib/src/chromium_ec/{command,commands}.rs` — not from memory. An earlier
//! lookup produced `0x3E07` for the command id, which is wrong.

/// `EcCommands::ChargeLimitControl`.
pub const CHARGE_LIMIT_CONTROL: u32 = 0x3E03;

/// Fan control over raw EC commands.
///
/// The Intel board drives the fan through `pwm1` / `pwm1_enable` in `cros_ec` hwmon.
/// **This board exposes neither** — only read-only `fan1_input`, `fan1_target` and
/// `fan1_fault` — so the fan is reachable only through the EC's own commands.
/// `EC_FEATURE_PWM_FAN` is advertised (`flags[0]=0x0207E6AE`, bit 2) and both commands
/// below were verified on hardware 2026-08-29: 40% gave ~4000 rpm, 70% ~6200 rpm, and
/// the EC took the fan back to 0 rpm within 2 s.
///
/// This is the second interface to reach ADR 0004's raw-EC tier, after the charge limit,
/// and for the same reason: there is no sysfs path that works.
///
/// Definitions verified against `torvalds/linux`
/// `include/linux/platform_data/cros_ec_commands.h`, not from memory. Pinned in tests
/// below because a wrong opcode is neither a compile error nor reliably a runtime one —
/// the EC simply answers a different question.
pub mod fan {
    /// `EC_CMD_PWM_SET_FAN_DUTY`. v0 params: `{ uint32_t percent; }`.
    pub const SET_FAN_DUTY: u32 = 0x0024;
    /// `EC_CMD_THERMAL_AUTO_FAN_CTRL`. v0 takes no parameters. Hands the fan back to
    /// firmware, and is the command every release path in the daemon depends on.
    pub const AUTO_FAN_CTRL: u32 = 0x0052;
    /// `EC_CMD_PWM_GET_FAN_TARGET_RPM`.
    ///
    /// **Measured to return 0 on this board**, under manual control and after release
    /// alike. It is the Intel board's `fan1_target` trap arriving through a different
    /// interface. Defined here so the number is written down once, with the reason not
    /// to use it; read `fan1_input` for actual RPM.
    pub const GET_FAN_TARGET_RPM: u32 = 0x0020;

    /// Full duty on this interface. **Duty is a percentage here, not an 8-bit count** —
    /// the single most consequential difference from the hwmon path, where 255 is full.
    /// A duty of 255 sent to this command is not "maximum", it is out of range.
    pub const MAX_DUTY: u8 = 100;

    /// Encode a duty request.
    ///
    /// The EC's wire format is little-endian regardless of host, so this is explicit
    /// rather than relying on `to_ne_bytes` happening to agree on x86.
    ///
    /// Duty is clamped rather than rejected: this is the last layer before the wire, and
    /// range policy belongs above it. Refusing a duty that cannot turn the fan is a
    /// separate decision made against the measured stall and break-away points.
    pub fn set_duty_request(percent: u8) -> [u8; 4] {
        (percent.min(MAX_DUTY) as u32).to_le_bytes()
    }

    /// Release the fan to firmware. Version 0 carries no payload.
    ///
    /// Idempotent: handing the fan to an EC that already owns it is a no-op. That
    /// property is load-bearing on this board — with no mode register to read, the
    /// daemon cannot ask whether it holds the fan, so it releases unconditionally
    /// instead of conditionally.
    pub fn auto_request() -> [u8; 0] {
        []
    }
}

/// Bytes in a charge-limit request: `modes`, `max_percentage`, `min_percentage`.
pub const REQUEST_LEN: usize = 3;
/// Bytes the EC returns for a `Get`: `max_percentage`, `min_percentage`.
pub const GET_RESPONSE_LEN: usize = 2;

/// `ChargeLimitControlModes`. A bitfield in Framework's source, but the modes we use
/// are issued singly.
pub mod mode {
    /// Disable all settings; charging goes back to being handled automatically.
    pub const DISABLE: u8 = 0x01;
    /// Set the maximum and minimum percentage.
    pub const SET: u8 = 0x02;
    /// Read the current setting. The only mode that returns a response.
    pub const GET: u8 = 0x08;
    /// Charge to full this once, without clearing the limit.
    pub const OVERRIDE: u8 = 0x80;
}

/// A charge window as the EC holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeLimits {
    pub min: u8,
    pub max: u8,
}

/// Ask the EC what it currently holds.
///
/// The `0xFF` padding is what Framework's own tool sends; the EC ignores it for a
/// `Get`, but sending the same bytes keeps us on the tested path.
pub fn get_request() -> [u8; REQUEST_LEN] {
    [mode::GET, 0xFF, 0xFF]
}

/// Set the charge window.
///
/// Note the order: **`max` precedes `min` on the wire**, which is the reverse of how
/// the pair reads in most of this codebase and the easiest thing here to get backwards.
/// Getting it wrong sets a *minimum* of 80%, which on a battery sitting above 80 would
/// look exactly like success.
pub fn set_request(limits: ChargeLimits) -> [u8; REQUEST_LEN] {
    [mode::SET, limits.max, limits.min]
}

/// Hand charging back to the EC's own management.
pub fn disable_request() -> [u8; REQUEST_LEN] {
    [mode::DISABLE, 0xFF, 0xFF]
}

/// Decode a `Get` response. `None` if the EC returned too few bytes to trust.
pub fn parse_limits(response: &[u8]) -> Option<ChargeLimits> {
    if response.len() < GET_RESPONSE_LEN {
        return None;
    }
    Some(ChargeLimits {
        max: response[0],
        min: response[1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_id_is_the_one_verified_against_framework_source() {
        // Pinned deliberately. A wrong opcode here is not a compile error and not a
        // runtime error either - the EC would simply answer a different question.
        assert_eq!(CHARGE_LIMIT_CONTROL, 0x3E03);
    }

    #[test]
    fn set_puts_max_before_min_on_the_wire() {
        // The single most dangerous transposition in this module: swapping these sets
        // a minimum rather than a maximum, and on a battery already above the value
        // that failure is indistinguishable from success.
        let req = set_request(ChargeLimits { min: 0, max: 80 });
        assert_eq!(req, [mode::SET, 80, 0]);
    }

    #[test]
    fn get_request_matches_framework_tool() {
        assert_eq!(get_request(), [0x08, 0xFF, 0xFF]);
    }

    #[test]
    fn parses_the_response_in_max_min_order() {
        // The probe run on hardware returned exactly these bytes while the standard
        // sysfs threshold read 80 - which is the measurement this whole module exists
        // because of.
        assert_eq!(
            parse_limits(&[100, 0]),
            Some(ChargeLimits { min: 0, max: 100 })
        );
    }

    #[test]
    fn refuses_a_short_response_rather_than_inventing_a_limit() {
        assert_eq!(parse_limits(&[]), None);
        assert_eq!(parse_limits(&[80]), None);
    }

    #[test]
    fn fan_command_ids_are_the_ones_verified_against_the_kernel_header() {
        // Pinned for the same reason as the charge command above: a wrong opcode is not
        // a compile error and often not a runtime error either. These three were read
        // out of cros_ec_commands.h with their neighbours, not recalled.
        assert_eq!(fan::SET_FAN_DUTY, 0x0024);
        assert_eq!(fan::AUTO_FAN_CTRL, 0x0052);
        assert_eq!(fan::GET_FAN_TARGET_RPM, 0x0020);
    }

    #[test]
    fn duty_is_a_percent_not_an_eight_bit_count() {
        // The difference that breaks every constant carried over from the hwmon path.
        // 255 is full duty there and out of range here, so it must clamp to 100 rather
        // than wrap to 255 % 256 = 255 or truncate to some middling speed.
        assert_eq!(fan::MAX_DUTY, 100);
        assert_eq!(fan::set_duty_request(255), 100u32.to_le_bytes());
        assert_eq!(fan::set_duty_request(101), 100u32.to_le_bytes());
    }

    #[test]
    fn duty_request_is_little_endian() {
        // Explicit rather than to_ne_bytes: the EC's wire format is little-endian
        // regardless of host, and on x86 a native-endian bug would never show up.
        assert_eq!(fan::set_duty_request(70), [70, 0, 0, 0]);
        assert_eq!(fan::set_duty_request(0), [0, 0, 0, 0]);
    }

    #[test]
    fn releasing_the_fan_carries_no_payload() {
        // Version 0 of AUTO_FAN_CTRL takes no parameters. Sending bytes it does not
        // expect risks the EC reading them as a fan index.
        assert!(fan::auto_request().is_empty());
    }

    #[test]
    fn a_duty_that_starts_the_fan_survives_encoding() {
        // 11% is the measured break-away duty on this board (2026-08-29): 10% sustains
        // rotation at 967 rpm but will not start the fan from rest, and 11% starts it
        // at 1098 rpm. Encoding must not round or clamp anywhere near here.
        assert_eq!(fan::set_duty_request(11), [11, 0, 0, 0]);
    }

    #[test]
    fn round_trips_through_the_wire_format() {
        let want = ChargeLimits { min: 20, max: 80 };
        let req = set_request(want);
        // What the EC would echo back to a Get, in its own order.
        assert_eq!(parse_limits(&[req[1], req[2]]), Some(want));
    }
}
