//! The firmware floor: never quieter than the EC would be.
//!
//! ADR 0006 point 4. A user-authored curve may only ever be *more* aggressive than
//! firmware, which bounds the damage from a badly drawn curve to "louder than
//! necessary" — an annoyance rather than a thermal event. This is what makes it
//! defensible to expose curve editing at all.
//!
//! The EC's curve lives in firmware and cannot be read, so it is reconstructed from
//! two measured tables (see `docs/hardware-baseline.md`):
//!
//! 1. What the EC does at a given temperature, in RPM.
//! 2. What a given duty produces, in RPM.
//!
//! Composing them answers the only question that matters: *what duty must we hold to
//! be at least as fast as firmware would be right now?*
//!
//! Two honesty notes about the tables:
//!
//! - The duty→RPM curve is **concave**, and badly approximated by a line. A linear fit
//!   through the three high points measured first predicted duty 77 for 2925 rpm; the
//!   real answer is ~85. Extrapolation here errs quiet, which is the dangerous
//!   direction, so the table is used directly and interpolated only between measured
//!   points.
//! - The EC table has **four points and a large gap across the knee** (44.9 °C to
//!   53.9 °C, over which it goes from silent to 2020 rpm). Linear interpolation across
//!   that gap is a guess. [`FirmwareFloor::observe`] is what turns it into a
//!   measurement: while the EC owns the fan, every sample raises the floor to what
//!   firmware was actually seen doing. The static table is only the cold start.

/// Below this the fan does not turn at all: duty 20 measured 0 rpm, duty 30 measured
/// 1107 rpm. A duty between 1 and 29 is not a slow fan, it is a stopped one, and
/// offering it would be a control that lies.
pub const STICTION_DUTY: u8 = 30;

/// Added to every non-zero floor duty.
///
/// The duty→RPM table was measured at ~39 °C while idle. Under load at 65.8 °C the
/// same duty produced measurably less air: duty 84 gave 2808 rpm where the table
/// predicts ~2886, and where the EC itself runs 2925 rpm at 64.8 °C. The table is
/// therefore slightly optimistic exactly where the floor matters most, and a floor
/// derived from an optimistic table is a floor that is too low.
///
/// Two counts is under 1% of full scale — inaudible — and buys back more than the
/// observed shortfall. It is not applied to a zero floor: silence must stay available
/// when firmware itself is silent.
const FLOOR_MARGIN_DUTY: u8 = 2;

/// Above this temperature the floor is full duty.
///
/// Beyond the hottest EC observation (76.8 °C) there is no data, and the safe
/// direction in unmeasured territory is loud. ADR 0006 point 5's ceiling override
/// takes over entirely above its own threshold; this only has to be sane until then.
const FULL_DUTY_ABOVE_C: f64 = 90.0;

/// What the EC does, unloaded and loaded, as measured. Temperature ascending.
const EC_CURVE: [(f64, u16); 5] = [
    (43.9, 0),
    (44.9, 0),
    (53.9, 2020),
    (64.8, 2925),
    (76.8, 3100),
];

/// What a written duty actually produces, measured 2026-08-21 at ~39 °C descending
/// from 180 so the fan never had to start from rest at a low duty. Duty ascending.
const DUTY_RPM: [(u8, u16); 12] = [
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
];

/// Width of an observation bucket, in degrees.
const BUCKET_C: f64 = 2.0;
const BUCKET_BASE_C: f64 = 30.0;
const BUCKETS: usize = 40; // 30 °C to 110 °C

/// The EC's floor, as a duty, for the current temperature.
#[derive(Debug, Clone)]
pub struct FirmwareFloor {
    /// Highest RPM the EC has been *seen* running at, per temperature bucket. Only
    /// ever raised: a single quiet sample proves nothing, because the EC may simply
    /// not have spun up yet, whereas a loud one is proof it will.
    observed: [u16; BUCKETS],
}

impl Default for FirmwareFloor {
    fn default() -> Self {
        Self::new()
    }
}

impl FirmwareFloor {
    pub fn new() -> Self {
        Self {
            observed: [0; BUCKETS],
        }
    }

    fn bucket(celsius: f64) -> Option<usize> {
        if !celsius.is_finite() || celsius < BUCKET_BASE_C {
            return None;
        }
        let idx = ((celsius - BUCKET_BASE_C) / BUCKET_C) as usize;
        (idx < BUCKETS).then_some(idx)
    }

    /// Record what firmware was seen doing. Call only while the EC owns the fan —
    /// under manual control the RPM is ours, and feeding it back would ratchet the
    /// floor up to whatever we last chose.
    pub fn observe(&mut self, celsius: f64, rpm: u64) {
        let Some(i) = Self::bucket(celsius) else {
            return;
        };
        let rpm = rpm.min(u64::from(u16::MAX)) as u16;
        if rpm > self.observed[i] {
            self.observed[i] = rpm;
        }
    }

    /// The RPM the EC would be running at this temperature, as best we know.
    pub fn floor_rpm(&self, celsius: f64) -> u16 {
        let modelled = interpolate_rpm(celsius);
        // A bucket only raises the floor. It never lowers it below the modelled
        // value, because "we have not seen the EC do more" is not evidence that it
        // would not.
        let seen = Self::bucket(celsius).map(|i| self.observed[i]).unwrap_or(0);
        modelled.max(seen)
    }

    /// The lowest duty we may hold at this temperature.
    ///
    /// Zero when firmware would have the fan off — the point of the clamp is to track
    /// the EC, and the EC runs the fan at 0 rpm below ~45 °C. A silent idle is
    /// therefore allowed, which is most of why anyone wants this feature.
    pub fn floor_duty(&self, celsius: f64) -> u8 {
        let rpm = self.floor_rpm(celsius);
        if rpm == 0 {
            return 0;
        }
        duty_for_rpm(rpm)
            .saturating_add(FLOOR_MARGIN_DUTY)
            .max(STICTION_DUTY)
    }

    /// Raise `duty` to the floor if it sits below it.
    ///
    /// Returns the duty to write and whether the clamp bit. Callers must surface the
    /// second half: ADR 0006 warns that a control silently ignoring the user reads as
    /// a bug.
    pub fn clamp(&self, duty: u8, celsius: f64) -> (u8, bool) {
        let floor = self.floor_duty(celsius);
        if duty < floor {
            (floor, true)
        } else {
            (duty, false)
        }
    }
}

/// The EC's RPM at `celsius`, interpolated between measured points.
fn interpolate_rpm(celsius: f64) -> u16 {
    if !celsius.is_finite() {
        // A sensor we cannot read is not permission to run the fan slowly.
        return u16::MAX;
    }
    let (last_c, last_rpm) = EC_CURVE[EC_CURVE.len() - 1];
    if celsius >= FULL_DUTY_ABOVE_C {
        return u16::MAX;
    }
    if celsius > last_c {
        // Unmeasured: ramp from the last known point to "as fast as possible" by the
        // time we reach FULL_DUTY_ABOVE_C.
        let span = FULL_DUTY_ABOVE_C - last_c;
        let t = (celsius - last_c) / span;
        let top = f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1);
        return lerp(f64::from(last_rpm), top, t) as u16;
    }
    if celsius <= EC_CURVE[0].0 {
        return EC_CURVE[0].1;
    }
    for w in EC_CURVE.windows(2) {
        let ((c0, r0), (c1, r1)) = (w[0], w[1]);
        if celsius <= c1 {
            let t = (celsius - c0) / (c1 - c0);
            return lerp(f64::from(r0), f64::from(r1), t) as u16;
        }
    }
    last_rpm
}

/// The lowest duty that reaches at least `rpm`, rounding **up**.
fn duty_for_rpm(rpm: u16) -> u8 {
    if rpm == 0 {
        return 0;
    }
    let top = DUTY_RPM[DUTY_RPM.len() - 1];
    if rpm >= top.1 {
        // Beyond anything measured: full duty. There is nothing faster to offer.
        return u8::MAX;
    }
    for w in DUTY_RPM.windows(2) {
        let ((d0, r0), (d1, r1)) = (w[0], w[1]);
        if rpm <= r1 {
            if r1 == r0 {
                return d1;
            }
            let t = (f64::from(rpm) - f64::from(r0)) / (f64::from(r1) - f64::from(r0));
            let duty = lerp(f64::from(d0), f64::from(d1), t);
            // Ceil, never round: landing one count short of firmware is the failure
            // this whole module exists to prevent.
            return duty.ceil().min(255.0) as u8;
        }
    }
    u8::MAX
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_allows_a_silent_fan() {
        // The EC runs the fan at 0 rpm below ~45 C, so we may too. If this ever
        // returns non-zero, the quiet-at-idle case the product exists for is gone.
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(30.0), 0);
        assert_eq!(f.floor_duty(43.9), 0);
        assert_eq!(f.floor_duty(44.9), 0);
    }

    #[test]
    fn floor_tracks_the_measured_ec_curve() {
        let f = FirmwareFloor::new();
        // Measured: EC at 2925 rpm at 64.8 C, and duty 90 produces 3052 rpm while
        // duty 77 produces only 2693. So the floor must be above 77.
        let d = f.floor_duty(64.8);
        assert!((78..=90).contains(&d), "floor at 64.8 C was {d}");

        // 53.9 C -> 2020 rpm, which sits between duty 50 (1879) and 65 (2296).
        let d = f.floor_duty(53.9);
        assert!((51..=65).contains(&d), "floor at 53.9 C was {d}");
    }

    #[test]
    fn the_floor_rises_with_temperature() {
        let f = FirmwareFloor::new();
        let mut previous = 0;
        for t in [40.0, 50.0, 55.0, 60.0, 65.0, 70.0, 75.0, 80.0, 85.0] {
            let d = f.floor_duty(t);
            assert!(d >= previous, "floor fell from {previous} to {d} at {t} C");
            previous = d;
        }
    }

    #[test]
    fn rounds_up_never_down() {
        // Landing a count short of firmware defeats the entire purpose.
        for rpm in [1u16, 1108, 1500, 2000, 2700, 3000, 3500] {
            let d = duty_for_rpm(rpm);
            let produced = interpolate_duty_rpm(d);
            assert!(
                produced >= f64::from(rpm) - 0.5,
                "duty {d} produces {produced} rpm, short of {rpm}"
            );
        }
    }

    /// Forward lookup, only used to check that `duty_for_rpm` did not round down.
    fn interpolate_duty_rpm(duty: u8) -> f64 {
        if duty >= DUTY_RPM[DUTY_RPM.len() - 1].0 {
            return f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1);
        }
        for w in DUTY_RPM.windows(2) {
            let ((d0, r0), (d1, r1)) = (w[0], w[1]);
            if duty <= d1 {
                let t = (f64::from(duty) - f64::from(d0)) / (f64::from(d1) - f64::from(d0));
                return lerp(f64::from(r0), f64::from(r1), t);
            }
        }
        f64::from(DUTY_RPM[DUTY_RPM.len() - 1].1)
    }

    #[test]
    fn never_offers_a_duty_that_cannot_turn_the_fan() {
        // Anything in 1..STICTION_DUTY is a stopped fan pretending otherwise.
        let f = FirmwareFloor::new();
        for t in [46.0, 48.0, 50.0, 52.0] {
            let d = f.floor_duty(t);
            assert!(
                d == 0 || d >= STICTION_DUTY,
                "floor at {t} C was {d}, which cannot turn the fan"
            );
        }
    }

    #[test]
    fn an_unreadable_sensor_demands_full_duty() {
        // No temperature is not permission to run the fan slowly.
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(f64::NAN), 255);
    }

    #[test]
    fn very_hot_means_full_duty() {
        let f = FirmwareFloor::new();
        assert_eq!(f.floor_duty(FULL_DUTY_ABOVE_C), 255);
        assert_eq!(f.floor_duty(110.0), 255);
        // And the unmeasured band above the last EC point ramps rather than flattening.
        assert!(f.floor_duty(85.0) > f.floor_duty(76.8));
    }

    #[test]
    fn observation_raises_the_floor_but_never_lowers_it() {
        let mut f = FirmwareFloor::new();
        let modelled = f.floor_duty(48.0);

        // The knee is the big gap in the static table. Seeing the EC at 2500 rpm at
        // 48 C is proof it does that, whatever the model interpolated.
        f.observe(48.0, 2500);
        let learned = f.floor_duty(48.0);
        assert!(learned > modelled, "{learned} should exceed {modelled}");

        // A later quiet sample proves nothing: the EC may simply not have spun up.
        f.observe(48.0, 0);
        assert_eq!(f.floor_duty(48.0), learned);
    }

    #[test]
    fn the_floor_beats_the_ec_at_every_measured_point() {
        // The invariant, checked against firmware's own measured behaviour rather than
        // against our model of it. Hardware caught a violation here that the model
        // was happy with, so assert on the measurements directly.
        let f = FirmwareFloor::new();
        for (celsius, ec_rpm) in EC_CURVE {
            if ec_rpm == 0 {
                continue;
            }
            let duty = f.floor_duty(celsius);
            let ours = interpolate_duty_rpm(duty);
            assert!(
                ours >= f64::from(ec_rpm),
                "at {celsius} C the EC runs {ec_rpm} rpm but our floor of {duty}/255 \
                 produces only {ours:.0} rpm"
            );
        }
    }

    #[test]
    fn clamp_reports_whether_it_bit() {
        let f = FirmwareFloor::new();
        // Idle: the user's choice stands, including silence.
        assert_eq!(f.clamp(0, 40.0), (0, false));
        // Hot: raised, and the caller is told so it can explain itself.
        let (duty, clamped) = f.clamp(0, 64.8);
        assert!(clamped);
        assert!(duty >= 78, "got {duty}");
    }

    #[test]
    fn observations_outside_the_bucket_range_are_ignored_not_panicking() {
        let mut f = FirmwareFloor::new();
        f.observe(-40.0, 5000);
        f.observe(500.0, 5000);
        f.observe(f64::NAN, 5000);
        assert_eq!(f.floor_duty(40.0), 0);
    }
}
