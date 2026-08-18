use std::time::{Duration, Instant};

/// Power draw above this is taken as evidence of a missed counter wrap rather than
/// a real reading. PL4 (`peak_power`) on the reference board is 175 W and is a
/// microsecond-scale current ceiling, so nothing sustained can approach it.
const IMPLAUSIBLE_WATTS: f64 = 200.0;

/// Default maximum interval between samples. Longer gaps are discarded: the counter
/// may have wrapped more than once, and a multi-wrap delta is indistinguishable from
/// a short one.
const DEFAULT_MAX_GAP: Duration = Duration::from_secs(60);

/// Converts a monotonically increasing, wrapping `energy_uj` counter into average watts.
///
/// Three hazards, all live on the reference hardware:
///
/// 1. **Wrap.** The counter is `max_energy_range_uj` wide and rolls over. On the
///    reference board that is 262,143 J — under 3 h at a 25 W load, ~1.2 h at PL2.
/// 2. **Multiple wraps.** A delta spanning more than one wrap looks exactly like a
///    short one. This is not recoverable, only detectable, so such samples are dropped.
/// 3. **Suspend.** Across s2idle the counter may reset while wall-clock advances.
///    Call [`invalidate`](Self::invalidate) on resume.
///
/// Never interpolate across a discarded sample — report "unknown" instead. A plausible
/// wrong number in a power readout is worse than a gap.
#[derive(Debug)]
pub struct EnergySampler {
    range_uj: u64,
    last: Option<(u64, Instant)>,
    max_gap: Duration,
}

impl EnergySampler {
    /// `range_uj` comes from the zone's `max_energy_range_uj`.
    pub fn new(range_uj: u64) -> Self {
        Self {
            range_uj,
            last: None,
            max_gap: DEFAULT_MAX_GAP,
        }
    }

    pub fn with_max_gap(mut self, gap: Duration) -> Self {
        self.max_gap = gap;
        self
    }

    /// Drop the reference point. Call on resume from suspend, and any time the
    /// sampling loop has been stalled.
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// Feed a raw `energy_uj` reading. Returns average watts since the previous
    /// accepted sample, or `None` when no trustworthy figure can be derived —
    /// first sample, gap too long, or an implausible result.
    pub fn sample(&mut self, energy_uj: u64, now: Instant) -> Option<f64> {
        let previous = self.last.replace((energy_uj, now));
        let (prev_uj, prev_at) = previous?;

        let dt = now.saturating_duration_since(prev_at);
        if dt.is_zero() || dt > self.max_gap {
            return None;
        }

        let delta_uj = if energy_uj >= prev_uj {
            energy_uj - prev_uj
        } else {
            // Wrapped exactly once — the only case we can correct for.
            self.range_uj.checked_sub(prev_uj)?.checked_add(energy_uj)?
        };

        let watts = (delta_uj as f64 / 1e6) / dt.as_secs_f64();
        if !watts.is_finite() || watts > IMPLAUSIBLE_WATTS {
            return None; // missed wrap, or a counter reset we cannot see
        }
        Some(watts)
    }

    /// Round to the published resolution. See ADR 0009 — sub-100 mW structure is
    /// where the PLATYPUS-style side channel lives and is useless in a UI.
    pub fn quantize(watts: f64) -> f64 {
        (watts * 10.0).round() / 10.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: u64 = 262_143_328_850; // reference board's max_energy_range_uj

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn first_sample_has_no_reference() {
        let mut s = EnergySampler::new(RANGE);
        assert_eq!(s.sample(1_000_000, Instant::now()), None);
    }

    #[test]
    fn computes_watts_from_delta() {
        let t0 = Instant::now();
        let mut s = EnergySampler::new(RANGE);
        s.sample(0, t0);
        // 25 J over 1 s == 25 W
        let w = s
            .sample(25_000_000, at(t0, 1))
            .expect("should yield a value");
        assert!((w - 25.0).abs() < 0.001, "got {w}");
    }

    #[test]
    fn corrects_single_wrap() {
        let t0 = Instant::now();
        let mut s = EnergySampler::new(RANGE);
        s.sample(RANGE - 10_000_000, t0); // 10 J short of the top
                                          // wraps and lands 15 J past zero => 25 J total over 1 s
        let w = s
            .sample(15_000_000, at(t0, 1))
            .expect("should yield a value");
        assert!((w - 25.0).abs() < 0.001, "got {w}");
    }

    #[test]
    fn discards_samples_beyond_max_gap() {
        let t0 = Instant::now();
        let mut s = EnergySampler::new(RANGE).with_max_gap(Duration::from_secs(10));
        s.sample(0, t0);
        // 11 s later: the counter could have wrapped an unknown number of times
        assert_eq!(s.sample(25_000_000, at(t0, 11)), None);
    }

    #[test]
    fn rejects_implausible_wattage() {
        let t0 = Instant::now();
        let mut s = EnergySampler::new(RANGE);
        s.sample(0, t0);
        // 500 J in 1 s == 500 W: a missed wrap, not a reading
        assert_eq!(s.sample(500_000_000, at(t0, 1)), None);
    }

    #[test]
    fn invalidate_drops_the_reference_point() {
        let t0 = Instant::now();
        let mut s = EnergySampler::new(RANGE);
        s.sample(0, t0);
        s.invalidate(); // e.g. resumed from suspend
        assert_eq!(s.sample(25_000_000, at(t0, 1)), None);
    }

    #[test]
    fn quantizes_to_tenths() {
        assert_eq!(EnergySampler::quantize(24.6712), 24.7);
        assert_eq!(EnergySampler::quantize(1.7700), 1.8);
    }
}
