//! Temperature → duty curve, with the two things that separate a usable fan curve
//! from an irritating one: hysteresis and ramp limiting.
//!
//! **Where the win actually is.** The plan written during M0 expected it in the
//! 55–70 °C band, on the strength of firmware running 2020 rpm at 53.9 °C. That figure
//! turned out to be firmware's *descending* branch: measured while heating, firmware is
//! silent right through that band and does not start the fan until 66–73 °C. So there
//! is little to win on the way up.
//!
//! The opposite end is wide open. Firmware's hysteresis holds the fan at duty 50–90 all
//! the way down to 44.9 °C after a load spike, long after the machine has stopped being
//! hot — measured, duty 82 at 54.9 °C while cooling, where the same temperature climbing
//! gets nothing at all. **A curve that comes down promptly is the audible difference**,
//! and the firmware floor permits it precisely because the floor records only the
//! ascending branch (ADR 0011).
//!
//! This produces a *request*. The daemon clamps it up to the firmware floor and the
//! battery guard afterwards, so nothing here can make the machine unsafe — smoothing
//! never delays a safety response, because the safety response is applied after it and
//! is not smoothed.

use std::fmt;

/// How far the temperature must fall below the curve's working point before the duty
/// follows it down.
///
/// Rising is followed immediately; only falling is damped. That asymmetry is
/// deliberate: heat should be answered at once, while quiet can afford to arrive a
/// couple of degrees late. Kept small — 2 °C against firmware's ~20 °C — because
/// coming down promptly is the entire point.
pub const HYSTERESIS_C: f64 = 2.0;

/// Largest duty change per tick, upward and downward. The poll runs at 1 Hz.
///
/// Measured, the fan moves about 33 rpm per duty count, so 12 up is ~400 rpm/s and 4
/// down is ~130 rpm/s. Unlimited steps are what make a fan sound like it is hunting;
/// the downward limit is tighter because a fan winding down in steps is more noticeable
/// than one spinning up.
pub const RAMP_UP_PER_TICK: u8 = 12;
pub const RAMP_DOWN_PER_TICK: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub celsius: f64,
    pub duty: u8,
}

#[derive(Debug, PartialEq)]
pub enum CurveError {
    TooFewPoints,
    NotAscending {
        at: usize,
    },
    DutyFalls {
        at: usize,
    },
    ImplausibleTemperature {
        at: usize,
        celsius: f64,
    },
    /// A duty that cannot turn the fan and is not zero — see [`crate::STICTION_DUTY`].
    UnturnableDuty {
        at: usize,
        duty: u8,
    },
}

impl fmt::Display for CurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFewPoints => write!(f, "a curve needs at least two points"),
            Self::NotAscending { at } => write!(
                f,
                "point {at} is not warmer than the one before it; points must ascend"
            ),
            Self::DutyFalls { at } => write!(
                f,
                "point {at} asks for less airflow than a cooler point. A curve that \
                 falls as it heats is almost certainly a mistake, and the firmware \
                 floor would override it anyway"
            ),
            Self::ImplausibleTemperature { at, celsius } => write!(
                f,
                "point {at} is at {celsius:.1} C, outside the 0-120 C a fan curve can \
                 sensibly describe"
            ),
            Self::UnturnableDuty { at, duty } => write!(
                f,
                "point {at} asks for duty {duty}, which cannot turn the fan: use 0 for \
                 a stopped fan or at least {} for a turning one",
                crate::floor::STICTION_DUTY
            ),
        }
    }
}

impl std::error::Error for CurveError {}

/// A validated temperature → duty curve.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    points: Vec<Point>,
}

impl Curve {
    /// Validate and build. Every rejection names something the author can fix.
    pub fn new(points: Vec<Point>) -> Result<Self, CurveError> {
        if points.len() < 2 {
            return Err(CurveError::TooFewPoints);
        }
        for (i, p) in points.iter().enumerate() {
            if !p.celsius.is_finite() || !(0.0..=120.0).contains(&p.celsius) {
                return Err(CurveError::ImplausibleTemperature {
                    at: i,
                    celsius: p.celsius,
                });
            }
            if p.duty > 0 && p.duty < crate::floor::STICTION_DUTY {
                return Err(CurveError::UnturnableDuty {
                    at: i,
                    duty: p.duty,
                });
            }
            if i > 0 {
                if p.celsius <= points[i - 1].celsius {
                    return Err(CurveError::NotAscending { at: i });
                }
                if p.duty < points[i - 1].duty {
                    return Err(CurveError::DutyFalls { at: i });
                }
            }
        }
        Ok(Self { points })
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// The duty this curve asks for at `celsius`, interpolated between points and flat
    /// beyond the ends.
    pub fn duty_at(&self, celsius: f64) -> u8 {
        if !celsius.is_finite() {
            // Not this module's job to be safe — the floor handles that — but a NaN
            // must not produce an arbitrary duty.
            return u8::MAX;
        }
        let first = self.points[0];
        if celsius <= first.celsius {
            return first.duty;
        }
        let last = self.points[self.points.len() - 1];
        if celsius >= last.celsius {
            return last.duty;
        }
        for w in self.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            if celsius <= b.celsius {
                let t = (celsius - a.celsius) / (b.celsius - a.celsius);
                let duty = f64::from(a.duty) + t * (f64::from(b.duty) - f64::from(a.duty));
                return duty.round() as u8;
            }
        }
        last.duty
    }

    /// The default curve, drawn from measured hardware behaviour.
    ///
    /// Silent to 55 °C, which firmware also is while heating. Through the loaded band
    /// it sits slightly below firmware — at 78 °C it asks 88 where firmware chose
    /// 94–102 — and above 90 °C it stops trying to be clever, since the floor and the
    /// ceiling own that territory anyway.
    ///
    /// The difference a user actually hears is on the way *down*: at 55 °C after a load
    /// spike this asks for silence where firmware is still at duty 82.
    pub fn default_quiet() -> Self {
        Self::new(vec![
            Point {
                celsius: 55.0,
                duty: 0,
            },
            Point {
                celsius: 62.0,
                duty: 40,
            },
            Point {
                celsius: 70.0,
                duty: 65,
            },
            Point {
                celsius: 80.0,
                duty: 92,
            },
            Point {
                celsius: 90.0,
                duty: 130,
            },
            Point {
                celsius: 100.0,
                duty: 255,
            },
        ])
        .expect("the built-in curve is valid")
    }
}

/// Runs a [`Curve`] over time, applying hysteresis and ramp limiting.
#[derive(Debug, Clone)]
pub struct CurveEngine {
    curve: Curve,
    /// The temperature the curve is currently being evaluated at. Follows the real
    /// temperature up immediately, and down only past the deadband.
    working_c: Option<f64>,
    last_duty: Option<u8>,
}

impl CurveEngine {
    pub fn new(curve: Curve) -> Self {
        Self {
            curve,
            working_c: None,
            last_duty: None,
        }
    }

    pub fn curve(&self) -> &Curve {
        &self.curve
    }

    /// Replace the curve, keeping the ramp state so swapping curves does not produce a
    /// step change in fan speed.
    pub fn set_curve(&mut self, curve: Curve) {
        self.curve = curve;
    }

    /// Forget the ramp and hysteresis state.
    ///
    /// For discontinuities where the previous duty says nothing about what comes next —
    /// resuming from suspend, or taking the fan back after firmware held it.
    pub fn reset(&mut self) {
        self.working_c = None;
        self.last_duty = None;
    }

    /// One tick. Returns the duty to request.
    pub fn tick(&mut self, celsius: f64) -> u8 {
        if !celsius.is_finite() {
            self.reset();
            return u8::MAX;
        }

        let working = match self.working_c {
            None => celsius,
            Some(previous) if celsius >= previous => celsius,
            // Falling: hold the working point until it has fallen far enough to be
            // worth acting on, so a sensor wobbling between two values does not make
            // the fan hunt.
            Some(previous) if celsius <= previous - HYSTERESIS_C => celsius,
            Some(previous) => previous,
        };
        self.working_c = Some(working);

        let target = self.curve.duty_at(working);
        let duty = match self.last_duty {
            None => target,
            Some(last) if target > last => {
                last.saturating_add(RAMP_UP_PER_TICK.min(target.saturating_sub(last)))
            }
            Some(last) if target < last => {
                last.saturating_sub(RAMP_DOWN_PER_TICK.min(last.saturating_sub(target)))
            }
            Some(last) => last,
        };
        // A duty between 1 and stiction turns nothing, so ramping down through it would
        // spend several ticks pretending. Step straight to a stop.
        let duty = if duty > 0 && duty < crate::floor::STICTION_DUTY && target == 0 {
            0
        } else {
            duty
        };
        self.last_duty = Some(duty);
        duty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(celsius: f64, duty: u8) -> Point {
        Point { celsius, duty }
    }

    #[test]
    fn rejects_curves_that_cannot_be_meant() {
        assert_eq!(
            Curve::new(vec![p(50.0, 0)]).unwrap_err(),
            CurveError::TooFewPoints
        );
        assert!(matches!(
            Curve::new(vec![p(60.0, 0), p(50.0, 90)]).unwrap_err(),
            CurveError::NotAscending { at: 1 }
        ));
        assert!(matches!(
            Curve::new(vec![p(50.0, 90), p(60.0, 40)]).unwrap_err(),
            CurveError::DutyFalls { at: 1 }
        ));
        assert!(matches!(
            Curve::new(vec![p(50.0, 0), p(60.0, 15)]).unwrap_err(),
            CurveError::UnturnableDuty { at: 1, duty: 15 }
        ));
        assert!(matches!(
            Curve::new(vec![p(-5.0, 0), p(60.0, 90)]).unwrap_err(),
            CurveError::ImplausibleTemperature { at: 0, .. }
        ));
    }

    #[test]
    fn errors_say_what_to_do_about_it() {
        let msg = CurveError::UnturnableDuty { at: 2, duty: 12 }.to_string();
        assert!(msg.contains("use 0"), "got: {msg}");
        let msg = CurveError::DutyFalls { at: 1 }.to_string();
        assert!(msg.contains("firmware floor"), "got: {msg}");
    }

    #[test]
    fn interpolates_between_points_and_flattens_beyond_them() {
        let c = Curve::new(vec![p(50.0, 0), p(60.0, 100)]).unwrap();
        assert_eq!(c.duty_at(40.0), 0, "flat below the first point");
        assert_eq!(c.duty_at(50.0), 0);
        assert_eq!(c.duty_at(55.0), 50, "halfway");
        assert_eq!(c.duty_at(60.0), 100);
        assert_eq!(c.duty_at(90.0), 100, "flat above the last point");
    }

    #[test]
    fn the_default_curve_is_quieter_than_firmware_on_the_way_down() {
        // The measured win. Cooling through 55 C, firmware is still at duty 82.
        let c = Curve::default_quiet();
        assert_eq!(c.duty_at(55.0), 0);
        assert_eq!(c.duty_at(50.0), 0);
        // And it does not try to be clever where the floor and ceiling take over.
        assert_eq!(c.duty_at(100.0), 255);
    }

    #[test]
    fn the_default_curve_is_not_louder_than_firmware_under_load() {
        // Measured: firmware chose duty 94-102 at 78.8 C. Being far above that would
        // make the machine louder than stock in exactly the case people notice.
        let c = Curve::default_quiet();
        let at_78 = c.duty_at(78.8);
        assert!((80..=100).contains(&at_78), "duty {at_78} at 78.8 C");
    }

    #[test]
    fn rising_temperature_is_followed_at_once() {
        let mut e = CurveEngine::new(Curve::new(vec![p(50.0, 0), p(100.0, 250)]).unwrap());
        e.tick(50.0);
        // Heat must not be damped by the hysteresis meant for cooling.
        let mut duty = 0;
        for _ in 0..40 {
            duty = e.tick(70.0);
        }
        assert_eq!(
            duty,
            Curve::new(vec![p(50.0, 0), p(100.0, 250)])
                .unwrap()
                .duty_at(70.0)
        );
    }

    #[test]
    fn a_wobbling_sensor_does_not_make_the_fan_hunt() {
        // The sensor is quantized to ~1 C and jitters between adjacent values. Without
        // a deadband that is a fan audibly changing speed every second forever.
        let mut e = CurveEngine::new(Curve::new(vec![p(50.0, 30), p(90.0, 250)]).unwrap());
        for _ in 0..20 {
            e.tick(70.0);
        }
        let settled = e.tick(70.0);
        for _ in 0..10 {
            assert_eq!(e.tick(69.0), settled, "1 C of jitter must not move the fan");
            assert_eq!(e.tick(70.0), settled);
        }
    }

    #[test]
    fn a_real_fall_is_followed_once_past_the_deadband() {
        let mut e = CurveEngine::new(Curve::new(vec![p(50.0, 30), p(90.0, 250)]).unwrap());
        for _ in 0..40 {
            e.tick(70.0);
        }
        let hot = e.tick(70.0);
        let mut duty = hot;
        for _ in 0..60 {
            duty = e.tick(60.0);
        }
        assert!(duty < hot, "should have come down from {hot}, got {duty}");
    }

    #[test]
    fn the_fan_never_steps_audibly() {
        let mut e = CurveEngine::new(Curve::new(vec![p(40.0, 0), p(41.0, 255)]).unwrap());
        // A near-vertical curve is the worst case a user can draw.
        let mut previous = e.tick(40.0);
        for _ in 0..40 {
            let d = e.tick(100.0);
            assert!(
                d.abs_diff(previous) <= RAMP_UP_PER_TICK,
                "jumped {previous} -> {d}"
            );
            previous = d;
        }
        for _ in 0..80 {
            let d = e.tick(0.0);
            // The one permitted exception: stopping. Below stiction the fan turns at
            // all speeds equally, which is to say not at all, so easing down through
            // that band would be several ticks of pretending. The step from the
            // slowest turning duty to a stop is the smallest change that can stop a
            // fan, and there is no gentler version of it.
            // Already within one ramp step of the band where the fan turns at no
            // speed at all, so the next step lands in it and snaps to a stop.
            let stopping = d == 0 && previous < crate::floor::STICTION_DUTY + RAMP_DOWN_PER_TICK;
            assert!(
                stopping || d.abs_diff(previous) <= RAMP_UP_PER_TICK.max(RAMP_DOWN_PER_TICK),
                "jumped {previous} -> {d}"
            );
            previous = d;
        }
        assert_eq!(previous, 0, "should have reached a stop");
    }

    #[test]
    fn ramping_down_stops_the_fan_rather_than_creeping_through_stiction() {
        // Duties between 1 and stiction turn nothing, so easing down through them is
        // several ticks of pretending the fan is still slowing.
        let mut e = CurveEngine::new(Curve::new(vec![p(40.0, 0), p(80.0, 200)]).unwrap());
        for _ in 0..40 {
            e.tick(80.0);
        }
        let mut duty = 255;
        for _ in 0..200 {
            duty = e.tick(20.0);
            if duty == 0 {
                break;
            }
            assert!(
                duty >= crate::floor::STICTION_DUTY,
                "settled at {duty}, which turns nothing"
            );
        }
        assert_eq!(duty, 0);
    }

    #[test]
    fn an_unreadable_sensor_asks_for_everything_and_forgets_its_state() {
        let mut e = CurveEngine::new(Curve::default_quiet());
        e.tick(60.0);
        assert_eq!(e.tick(f64::NAN), u8::MAX);
        // State is dropped, so the next real reading starts clean rather than ramping
        // from a duty that meant nothing.
        assert_eq!(e.tick(40.0), 0);
    }
}
