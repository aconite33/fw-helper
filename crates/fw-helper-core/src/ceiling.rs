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

/// Hard cap on the derived ceiling, however high the sensor's critical point is.
///
/// `peci-temp` reports crit at 119.85 °C on the reference board, which would put the
/// ceiling near 105 °C. That is a defensible number for the *sensor*, but this is a
/// laptop that runs at 76.8 °C under full load with the stock curve — anything above
/// about 100 °C means something has already gone badly wrong, and there is no reason
/// to let a user's configuration keep control that far out.
pub const MAX_CEILING_C: f64 = 100.0;

/// Used when no sensor offers a usable critical point.
///
/// Deliberately below [`MAX_CEILING_C`]: not knowing the limit is a reason to be more
/// cautious, not less. Still well above the 76.8 °C measured under sustained full
/// load, so it does not fire in normal operation.
pub const FALLBACK_CEILING_C: f64 = 90.0;

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
        // 76.8 C is the measured steady state under sustained full load with the
        // stock curve. If the ceiling ever trips there, manual control becomes
        // useless exactly when it is wanted.
        let c = Ceiling::from_crit(Some(119.85));
        assert!(!c.exceeded_by(76.8));
        assert!(!c.exceeded_by(85.0));
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
