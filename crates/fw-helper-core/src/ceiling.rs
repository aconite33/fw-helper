//! The temperature above which the user does not get a vote.
//!
//! ADR 0006 point 5: past a hard threshold, release manual control entirely and let
//! firmware do its job. This is what makes "is this safe?" answerable — the firmware's
//! own thermal protection is never actually removed, only deferred to.
//!
//! **Releasing does not cool the machine faster.** Measured: the EC's curve tops out
//! around 3100 rpm, while manual control reaches ~5200 rpm at full duty. Handing the
//! fan back therefore *reduces* airflow. That is not an argument against doing it —
//! firmware's protection is more than the fan, and a daemon that has driven the
//! machine to this temperature has forfeited the benefit of the doubt — but it does
//! dictate the ordering: [`crate::FirmwareFloor`] demands full duty well before this
//! point, so maximum airflow is tried first and this is the last resort.
//!
//! The threshold is derived from `temp*_crit` and **never** from `temp*_max`: on the
//! reference board every `temp*_max` reads -273150 (unset, 0 K), and a naive read
//! would put the ceiling at absolute zero and disable manual fan control permanently.
//! Every value is validated before it is trusted.

/// How far below the sensor's critical point to intervene.
///
/// Releasing *at* crit would be releasing after the point firmware already considers
/// critical. This buys the EC time to react to a fan it has just been handed.
pub const CEILING_MARGIN_C: f64 = 15.0;

/// The CPU's own throttle point, measured from `coretemp`'s `temp*_crit`: **100 °C**
/// on every core and on the package.
///
/// This is the number that matters, and it is not the one `peci-temp` reports.
/// `peci-temp` gives crit as 119.85 °C — *above* Tjmax — so deriving a ceiling from it
/// alone describes a limit the CPU never reaches. Past Tjmax the CPU throttles itself;
/// it does not need us.
pub const TJMAX_C: f64 = 100.0;

/// Hard cap on the derived ceiling, however high the sensor's critical point is.
///
/// Tjmax, because there is nothing useful above it: the CPU is already throttling, and
/// releasing the fan to firmware at that point protects nothing that is not already
/// protected.
pub const MAX_CEILING_C: f64 = TJMAX_C;

/// Used when no sensor offers a usable critical point.
///
/// **Was 90 °C, which was wrong.** That number came from a comment claiming this
/// machine "runs at 76.8 °C under sustained full load" — a figure from the M0 PL1 test
/// that turns out not to be the hottest it gets. Measured 2026-08-21 under ordinary
/// multi-core load with firmware driving the fan, `peci-temp` reached **92.8 °C**. A
/// 90 °C fallback therefore sat *below* normal operation and would have fired during
/// an ordinary long compile, taking manual fan control away repeatedly for no reason.
///
/// 97 °C keeps it below [`MAX_CEILING_C`] — not knowing the limit is still a reason to
/// be more cautious — while staying clear of temperatures the machine genuinely uses.
pub const FALLBACK_CEILING_C: f64 = 97.0;

/// Plausible range for a critical threshold, in Celsius.
const PLAUSIBLE: std::ops::RangeInclusive<f64> = 0.0..=150.0;

/// The temperature at which manual fan control is given up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ceiling {
    celsius: f64,
    derived: bool,
}

impl Ceiling {
    /// Derive from a sensor's critical threshold.
    ///
    /// `None`, or anything implausible, falls back to a constant rather than trusting
    /// it — the -273150 case is not hypothetical, it is what this board reports for
    /// every `temp*_max`.
    pub fn from_crit(crit: Option<f64>) -> Self {
        match crit {
            Some(c) if c.is_finite() && PLAUSIBLE.contains(&c) => Self {
                celsius: (c - CEILING_MARGIN_C).min(MAX_CEILING_C),
                derived: true,
            },
            _ => Self {
                celsius: FALLBACK_CEILING_C,
                derived: false,
            },
        }
    }

    pub fn celsius(&self) -> f64 {
        self.celsius
    }

    /// Whether this came from hardware or from the fallback constant. Worth logging:
    /// a machine running on the fallback is one whose sensors we could not read.
    pub fn is_derived(&self) -> bool {
        self.derived
    }

    /// Is the machine too hot for us to be holding the fan?
    ///
    /// An unreadable temperature counts as exceeded. Not knowing is not permission.
    pub fn exceeded_by(&self, celsius: f64) -> bool {
        !celsius.is_finite() || celsius >= self.celsius
    }
}

impl Default for Ceiling {
    fn default() -> Self {
        Self::from_crit(None)
    }
}

impl std::fmt::Display for Ceiling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} C", self.celsius)?;
        if !self.derived {
            write!(f, " (fallback; no usable temp*_crit)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unset_sensor_value_never_becomes_a_ceiling() {
        // -273150 millidegrees is what every temp*_max reads on the reference board.
        // Trusting it would put the ceiling at absolute zero, and manual fan control
        // would be permanently disabled with no obvious reason why.
        let c = Ceiling::from_crit(Some(-273.15));
        assert_eq!(c.celsius(), FALLBACK_CEILING_C);
        assert!(!c.is_derived());
    }

    #[test]
    fn implausible_values_fall_back_rather_than_being_trusted() {
        for bad in [
            None,
            Some(f64::NAN),
            Some(-1.0),
            Some(151.0),
            Some(f64::INFINITY),
        ] {
            let c = Ceiling::from_crit(bad);
            assert_eq!(c.celsius(), FALLBACK_CEILING_C, "for {bad:?}");
            assert!(!c.is_derived());
        }
    }

    #[test]
    fn derives_below_the_sensors_critical_point() {
        // The board sensors report 87.85 C.
        let c = Ceiling::from_crit(Some(87.85));
        assert!(c.is_derived());
        assert!(c.celsius() < 87.85, "must intervene before crit, not at it");
        assert_eq!(c.celsius(), 87.85 - CEILING_MARGIN_C);
    }

    #[test]
    fn a_very_high_critical_point_is_capped() {
        // peci-temp reports 119.85 C, which would otherwise put the ceiling near
        // 105 C. This is a laptop, not a furnace.
        let c = Ceiling::from_crit(Some(119.85));
        assert_eq!(c.celsius(), MAX_CEILING_C);
    }

    #[test]
    fn the_fallback_is_more_cautious_than_the_cap() {
        // Not knowing the limit is a reason to be more careful, not less. A const
        // block so violating it fails the build rather than a test run.
        const { assert!(FALLBACK_CEILING_C < MAX_CEILING_C) };
    }

    #[test]
    fn does_not_fire_at_temperatures_this_machine_actually_reaches() {
        // 92.8 C was measured under ordinary multi-core load with firmware driving the
        // fan. An earlier version of this test asserted only up to 85 C, on the belief
        // that full load meant 76.8 C, and the fallback ceiling was set to 90 C on the
        // same belief. Both were wrong. If the ceiling trips at temperatures the
        // machine reaches in normal use, manual control becomes useless exactly when
        // it is wanted.
        for ceiling in [Ceiling::from_crit(Some(119.85)), Ceiling::from_crit(None)] {
            assert!(!ceiling.exceeded_by(76.8), "{ceiling}");
            assert!(
                !ceiling.exceeded_by(92.8),
                "{ceiling} fires in normal operation"
            );
        }
    }

    #[test]
    fn the_cap_is_the_cpus_own_throttle_point() {
        // Above Tjmax the CPU throttles itself. There is nothing for us to add, so
        // there is no reason for a ceiling above it.
        assert_eq!(MAX_CEILING_C, TJMAX_C);
        assert!(Ceiling::from_crit(Some(119.85)).exceeded_by(TJMAX_C));
    }

    #[test]
    fn an_unreadable_temperature_counts_as_too_hot() {
        let c = Ceiling::from_crit(Some(119.85));
        assert!(c.exceeded_by(f64::NAN));
    }

    #[test]
    fn fires_above_the_threshold() {
        let c = Ceiling::from_crit(Some(119.85));
        assert!(c.exceeded_by(MAX_CEILING_C));
        assert!(c.exceeded_by(110.0));
    }
}
