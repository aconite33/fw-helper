//! A backstop for the one component that cannot protect itself.
//!
//! The CPU throttles at Tjmax (ADR 0011), so constraining the fan costs performance
//! rather than hardware. The battery has no equivalent: `battery_temp@b` reports crit
//! at **49.9 °C**, the lowest threshold on the board, and a hot battery simply degrades.
//!
//! **This is a backstop for an unmeasured case, not a response to an observed one, and
//! the distinction matters when reading the constants below.** Measured 2026-08-21
//! under five minutes of 16-core load with firmware driving the fan, the battery rose
//! from 31.9 °C to **33.9 °C** — a 2 °C rise, leaving 16 °C of headroom. It is well
//! isolated from the CPU and lagged so heavily it was still creeping upward during the
//! cooldown. On that evidence the battery is in no danger from CPU heat.
//!
//! What has *not* been measured is the same load with the fan held low for much longer
//! than five minutes, which is exactly what a user-authored curve will make possible.
//! So this exists, and it should essentially never fire. If it fires often, the
//! thresholds are wrong or the situation is genuinely new — either way, worth knowing.

/// Used when a battery sensor exists but reports no usable critical threshold.
///
/// 50 °C is the conventional upper limit for Li-ion discharge and matches what this
/// board reports. It is only ever a fallback: a real reading is always preferred.
pub const FALLBACK_CRIT_C: f64 = 50.0;

/// How far below crit the guard starts asking for airflow.
///
/// 8 °C puts the start at 41.9 °C on this board — comfortably above the 33.9 °C peak
/// measured under full load, so ordinary work does not trip it, while leaving room to
/// respond before crit rather than at it.
const RAMP_BELOW_CRIT_C: f64 = 8.0;

/// How far below crit manual fan control is given up entirely.
const RELEASE_BELOW_CRIT_C: f64 = 2.0;

/// A battery critical threshold outside this range is not believable, and trusting one
/// would either disable the guard permanently or pin the fan at full duty forever.
const PLAUSIBLE: std::ops::RangeInclusive<f64> = 20.0..=80.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryGuard {
    crit: f64,
}

impl Default for BatteryGuard {
    fn default() -> Self {
        Self {
            crit: FALLBACK_CRIT_C,
        }
    }
}

impl BatteryGuard {
    /// Build from the battery sensor's own critical threshold.
    pub fn from_crit(crit: Option<f64>) -> Self {
        match crit {
            Some(c) if c.is_finite() && PLAUSIBLE.contains(&c) => Self { crit: c },
            _ => Self::default(),
        }
    }

    pub fn crit(&self) -> f64 {
        self.crit
    }

    /// Where the guard starts asking for airflow.
    pub fn ramp_start(&self) -> f64 {
        self.crit - RAMP_BELOW_CRIT_C
    }

    /// Above this the battery is close enough to its limit that a quiet fan is no
    /// longer the user's call.
    pub fn release_above(&self) -> f64 {
        self.crit - RELEASE_BELOW_CRIT_C
    }

    /// The minimum duty this battery temperature justifies, independent of the CPU.
    ///
    /// Composed with the firmware floor by taking whichever is higher: the CPU may be
    /// idle while the battery is hot from charging or from a warm room.
    pub fn floor_duty(&self, celsius: f64) -> u8 {
        if !celsius.is_finite() {
            // Unlike the CPU, a missing battery reading is not grounds for full duty:
            // plenty of machines have no battery sensor at all, and this guard must
            // not turn "no sensor" into "fan at maximum forever". The caller decides
            // whether a battery exists; this only answers for one that does.
            return 0;
        }
        let start = self.ramp_start();
        if celsius <= start {
            return 0;
        }
        let span = self.release_above() - start;
        if span <= 0.0 {
            return u8::MAX;
        }
        let t = ((celsius - start) / span).clamp(0.0, 1.0);
        let duty = (t * f64::from(u8::MAX)).ceil();
        (duty as u8).max(crate::floor::STICTION_DUTY)
    }

    /// Is the battery too close to its limit for a user's quiet setting to stand?
    pub fn exceeded_by(&self, celsius: f64) -> bool {
        celsius.is_finite() && celsius >= self.release_above()
    }
}

impl std::fmt::Display for BatteryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "battery limit {:.1} C", self.crit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_fire_at_temperatures_measured_under_full_load() {
        // 33.9 C was the peak across five minutes of 16-core load. If the guard fires
        // there it is not a backstop, it is a nuisance that would pin the fan on
        // during ordinary work.
        let g = BatteryGuard::from_crit(Some(49.9));
        assert_eq!(g.floor_duty(33.9), 0);
        assert!(!g.exceeded_by(33.9));
        assert_eq!(g.floor_duty(31.9), 0);
    }

    #[test]
    fn asks_for_airflow_as_it_approaches_the_limit() {
        let g = BatteryGuard::from_crit(Some(49.9));
        assert_eq!(g.floor_duty(g.ramp_start()), 0);

        let mid = g.floor_duty(45.0);
        assert!(mid >= crate::floor::STICTION_DUTY, "got {mid}");
        assert!(mid < u8::MAX);
        // Monotonic: hotter must never ask for less.
        let mut previous = 0;
        for t in [42.0, 43.0, 44.0, 45.0, 46.0, 47.0, 48.0] {
            let d = g.floor_duty(t);
            assert!(d >= previous, "floor fell from {previous} to {d} at {t} C");
            previous = d;
        }
    }

    #[test]
    fn gives_up_manual_control_before_crit_not_at_it() {
        let g = BatteryGuard::from_crit(Some(49.9));
        assert!(g.release_above() < 49.9);
        assert!(g.exceeded_by(48.0));
        assert!(g.exceeded_by(49.9));
        assert_eq!(g.floor_duty(48.0), u8::MAX);
    }

    #[test]
    fn implausible_thresholds_fall_back() {
        for bad in [None, Some(f64::NAN), Some(-273.15), Some(0.0), Some(120.0)] {
            assert_eq!(
                BatteryGuard::from_crit(bad).crit(),
                FALLBACK_CRIT_C,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_missing_reading_does_not_mean_full_duty() {
        // The opposite of the CPU rule, deliberately. No battery sensor is a normal
        // state for a desktop or a machine with the pack removed, and it must not pin
        // the fan at maximum forever.
        let g = BatteryGuard::from_crit(Some(49.9));
        assert_eq!(g.floor_duty(f64::NAN), 0);
        assert!(!g.exceeded_by(f64::NAN));
    }
}
