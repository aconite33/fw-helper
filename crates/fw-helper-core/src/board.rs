//! What each supported board is, as measured.
//!
//! The fan's behaviour is a property of the board, not of the code: how fast a given duty
//! turns it, which sensor firmware follows, and what firmware does at each temperature.
//! Upstream hardcoded one board's answers and applied them everywhere. On the AMD boards
//! that would have put the firmware floor up to 2553 rpm *below* firmware through the
//! middle of the range - the one direction the floor exists to prevent - so the answers
//! live here, keyed by the DMI board name they were measured on.
//!
//! An unmeasured board gets no profile, and fan control is refused with a reason naming
//! the probes that would measure it. Guessing is how the floor ends up below firmware.

use crate::Sysfs;

/// DMI board name, relative to the [`Sysfs`] root.
pub const DMI_BOARD_NAME: &str = "sys/class/dmi/id/board_name";

/// A board's measured fan characteristics.
#[derive(Debug)]
pub struct BoardProfile {
    /// For logs and capability messages.
    pub name: &'static str,
    /// DMI board names these measurements were taken on.
    pub measured_on: &'static [&'static str],
    /// Board-name prefix of the family: siblings that are expected, not shown, to match.
    pub family_prefix: &'static str,
    /// The sensor firmware's fan decision follows. The floor must be expressed on the
    /// same sensor, or "never quieter than firmware" compares against the wrong thing.
    pub control_sensor: &'static str,
    /// What a written duty produces, 0-255 duty ascending. Measured descending, so the
    /// fan never had to start from rest at a low duty.
    pub duty_rpm: &'static [(u8, u16)],
    /// What firmware does, as control-sensor temperature -> rpm, ascending. The floor's
    /// cold-start model, superseded by anything observed.
    pub firmware_curve: &'static [(f64, u16)],
    /// Whether firmware's curve depends on direction against [`Self::control_sensor`].
    /// When it does, only the heating branch says what a temperature needs (ADR 0011).
    pub firmware_curve_hysteretic: bool,
}

/// Framework Laptop 13 Pro, Intel Core Ultra Series 3.
///
/// Upstream's measurements, 2026-08-21, unchanged. Its hysteresis was measured against
/// `peci-temp`, the die sensor. On the AMD boards the same shape turned out to be thermal
/// lag between the die and the thermistor firmware actually reads - whether that is also
/// true here is unmeasured, so this keeps the hysteretic treatment it was verified with.
pub static INTEL_CORE_ULTRA_3: BoardProfile = BoardProfile {
    name: "Framework 13 Pro (Intel Core Ultra Series 3)",
    measured_on: &["FRANMJCP07"],
    family_prefix: "FRANMJCP",
    control_sensor: "peci-temp",
    duty_rpm: &[
        (0, 0),
        (20, 0),
        (30, 1107),
        (40, 1512),
        (50, 1879),
        (65, 2296),
        (77, 2693),
        (90, 3052),
        (100, 3355),
        (120, 3840),
        (150, 4551),
        (180, 5201),
    ],
    firmware_curve: &[
        (43.9, 0),
        (44.9, 0),
        (53.9, 2020),
        (64.8, 2925),
        (76.8, 3100),
    ],
    firmware_curve_hysteretic: true,
};

/// Framework Laptop 13, AMD Ryzen AI 300.
///
/// Measured on two boards that turned out to carry the same fan: `FRANMGCP05` (Ryzen AI
/// 5 340, EC `lilac-3.0.5`, 2026-08-29) and `FRANMGCP09` (Ryzen AI 9 HX 370, EC
/// `lilac-4.0.2`, 2026-09-18). Their duty sweeps agree within about 1% at every point.
///
/// **The duty table takes the lower of the two readings at each point.** The floor
/// inverts it to ask which duty reaches firmware's rpm, and a table that overstates the
/// rpm a duty produces answers with a duty too low - a floor below firmware.
///
/// **Firmware follows `cpu_f75303@4d`**, a board thermistor, not the die (`cpu@4c`,
/// which tracks `k10temp` `Tctl`). Against the thermistor its curve is identical heating
/// and cooling, to the rpm; against the die the same run looks hysteretic by 2300 rpm at
/// one temperature, which is thermal lag and not firmware memory. So no branch logic.
///
/// The step at the bottom is written so the floor can never be quieter than firmware:
/// firmware is off at 49.9 °C and at 2265 rpm by 50.9, and the sensor reads in whole
/// degrees, so the floor jumps straight to 2265 above 49.9 rather than interpolating
/// through speeds firmware never uses.
pub static AMD_RYZEN_AI_300: BoardProfile = BoardProfile {
    name: "Framework 13 (AMD Ryzen AI 300)",
    measured_on: &["FRANMGCP05", "FRANMGCP09"],
    family_prefix: "FRANMGCP",
    control_sensor: "cpu_f75303@4d",
    duty_rpm: &[
        (0, 0),
        (20, 0),
        (26, 967),
        (31, 1221),
        (36, 1456),
        (41, 1689),
        (46, 1916),
        (51, 2155),
        (64, 2664),
        (77, 3160),
        (89, 3614),
        (102, 4045),
        (115, 4468),
        (128, 4890),
        (153, 5585),
        (179, 6182),
        (204, 6779),
        (230, 7336),
        (255, 7864),
    ],
    firmware_curve: &[
        (49.9, 0),
        (50.0, 2265),
        (51.9, 2472),
        (52.9, 2679),
        (53.9, 2928),
        (55.9, 3342),
        (57.9, 3797),
        (59.9, 4212),
        (61.9, 4667),
        (63.9, 5081),
        (65.8, 5537),
        (67.8, 5951),
        (68.8, 6200),
    ],
    firmware_curve_hysteretic: false,
};

/// Every profile, in the order they are tried.
///
/// Profiles are `static`, not `const`, so each has exactly one address. A `const` is
/// copied at every use, and two copies of the same profile compare unequal by address -
/// which made "is this the same board?" answer no for the same board.
pub static PROFILES: [&BoardProfile; 2] = [&INTEL_CORE_ULTRA_3, &AMD_RYZEN_AI_300];

/// What is known about the board this is running on.
#[derive(Debug, Clone, Copy)]
pub enum Board {
    /// These measurements were taken on exactly this board.
    Measured(&'static BoardProfile),
    /// A sibling of a measured board. Expected to match, not shown to.
    Family(&'static BoardProfile, &'static str),
    /// Nothing measured applies.
    Unknown,
}

impl Board {
    /// The profile to use, if any.
    pub fn profile(&self) -> Option<&'static BoardProfile> {
        match *self {
            Board::Measured(p) | Board::Family(p, _) => Some(p),
            Board::Unknown => None,
        }
    }
}

/// Identify this board from DMI.
pub fn identify(fs: &Sysfs) -> Board {
    let Ok(name) = fs.read_string(DMI_BOARD_NAME) else {
        return Board::Unknown;
    };
    identify_name(name.trim())
}

/// Identify a board from its DMI name. Separate from [`identify`] so the matching rules
/// are testable without a filesystem.
pub fn identify_name(name: &str) -> Board {
    for profile in PROFILES {
        if profile.measured_on.contains(&name) {
            return Board::Measured(profile);
        }
    }
    for profile in PROFILES {
        if name.starts_with(profile.family_prefix) {
            // Leaked rather than owned: identification happens once per process, and a
            // 'static name keeps Board copyable for every caller that needs it.
            return Board::Family(profile, Box::leak(name.to_string().into_boxed_str()));
        }
    }
    Board::Unknown
}

impl std::fmt::Display for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Board::Measured(p) => write!(f, "{} (measured)", p.name),
            Board::Family(p, name) => write!(
                f,
                "{name}, treated as {} - same family, not itself measured",
                p.name
            ),
            Board::Unknown => write!(f, "unrecognised board"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_amd_boards_are_measured() {
        for name in ["FRANMGCP05", "FRANMGCP09"] {
            assert!(
                matches!(identify_name(name), Board::Measured(p) if p.name == AMD_RYZEN_AI_300.name),
                "{name}"
            );
        }
    }

    #[test]
    fn the_intel_pro_is_measured() {
        assert!(matches!(
            identify_name("FRANMJCP07"),
            Board::Measured(p) if p.name == INTEL_CORE_ULTRA_3.name
        ));
    }

    #[test]
    fn an_unmeasured_sibling_is_family_not_measured() {
        // Close enough to use, not close enough to claim. The log has to say which.
        match identify_name("FRANMGCP11") {
            Board::Family(p, name) => {
                assert_eq!(p.name, AMD_RYZEN_AI_300.name);
                assert_eq!(name, "FRANMGCP11");
            }
            other => panic!("expected a family match, got {other:?}"),
        }
    }

    #[test]
    fn an_older_framework_board_is_not_guessed_at() {
        // Earlier Framework 13 generations use different ECs entirely. Applying either
        // profile to them is exactly the guess this module exists to stop.
        for name in ["FRANMACP04", "FRANDGCP04", "FRANMDCP08", "", "Standard"] {
            assert!(
                matches!(identify_name(name), Board::Unknown),
                "{name:?} should be unknown"
            );
        }
    }

    fn ascending_duty(table: &[(u8, u16)]) -> bool {
        table
            .windows(2)
            .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1)
    }

    fn ascending_curve(table: &[(f64, u16)]) -> bool {
        table
            .windows(2)
            .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1)
    }

    #[test]
    fn every_table_is_monotonic() {
        // Interpolation and inversion both assume it. A table that dips would let the
        // floor answer a hotter temperature with a quieter fan.
        for p in PROFILES {
            assert!(ascending_duty(p.duty_rpm), "{} duty table", p.name);
            assert!(
                ascending_curve(p.firmware_curve),
                "{} firmware curve",
                p.name
            );
        }
    }

    #[test]
    fn the_amd_fan_table_never_exceeds_either_board() {
        // Conservative by construction: each point is the lower of the two sweeps, so
        // inverting it can only ever ask for more duty than either board needed.
        let b05: [(u8, u16); 4] = [(51, 2155), (128, 4890), (179, 6261), (255, 7864)];
        let b09: [(u8, u16); 4] = [(51, 2155), (128, 4890), (179, 6182), (255, 7927)];
        for ((d, a), (_, b)) in b05.iter().zip(b09.iter()) {
            let table = AMD_RYZEN_AI_300
                .duty_rpm
                .iter()
                .find(|(duty, _)| duty == d)
                .map(|(_, rpm)| *rpm)
                .expect("measured point present");
            assert!(table <= *a && table <= *b, "duty {d}: {table} vs {a}/{b}");
        }
    }

    #[test]
    fn the_amd_floor_is_never_quieter_than_firmware_at_the_step() {
        // Firmware is off at 49.9 and running by 50.9. Nothing between may interpolate
        // through speeds below 2265 rpm, which firmware never runs at.
        let curve = AMD_RYZEN_AI_300.firmware_curve;
        let off = curve.iter().position(|&(c, _)| c == 49.9).unwrap();
        assert_eq!(curve[off].1, 0);
        assert!(
            curve[off + 1].0 - curve[off].0 < 0.15,
            "step must be immediate"
        );
        assert_eq!(curve[off + 1].1, 2265);
    }
}
