use crate::{paths, Cap, Capabilities, EnergySampler, Sysfs};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct TempReading {
    pub label: String,
    pub celsius: f64,
    /// From `temp*_crit`. `None` when the board reports an implausible value —
    /// on the reference machine every `temp*_max` reads -273150 (unset), so any
    /// threshold must be sanity-checked before it is trusted (ADR 0006).
    pub critical: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Telemetry {
    pub temps: Vec<TempReading>,
    pub fan_rpm: Option<u64>,
    /// Quantized to 0.1 W and updated at most 1 Hz — ADR 0009.
    pub package_watts: Option<f64>,
    /// Lowest package power seen since this monitor started.
    ///
    /// The package draws several watts with nothing running at all - measured 3.6 W
    /// idle on the AMD board against 32.6 W with every core busy - and that floor is
    /// caused by no process. Anything attributing power to processes needs to know it,
    /// and it cannot be a constant: it is a property of the machine, its display, and
    /// what else is plugged into it.
    pub package_watts_floor: Option<f64>,
    pub battery_percent: Option<u64>,
    pub battery_status: Option<String>,
    pub platform_profile: Option<String>,
    /// What the whole machine is drawing, in watts.
    ///
    /// Only measurable **on battery**, where it is the battery's own discharge rate and
    /// therefore genuinely everything: CPU, screen, USB, the lot. On mains nothing
    /// reports total system draw, so this is `None` rather than a guess — the RAPL
    /// figure is the CPU package alone and is typically a fraction of it.
    pub system_watts: Option<f64>,
    /// Minutes until the battery is empty at the current rate, when discharging.
    pub battery_minutes: Option<u64>,
    /// True on mains, false on battery, `None` when no mains supply can be found.
    ///
    /// Resolved by the supply's `type` being `Mains`, never by its name: this board
    /// calls it `ACAD`, others call it `AC` or `ADP1`, and the same reasoning that
    /// applies to hwmon indices applies here.
    pub on_ac: Option<bool>,
}

impl Telemetry {
    /// The battery's own temperature sensor, if this board exposes one.
    ///
    /// Worth having separately from [`Self::control_temp`] because the battery is the
    /// one component here with a low limit and **no protection of its own**: on the
    /// reference board `battery_temp@b` reports crit at 49.9 °C, against 100 °C for the
    /// CPU, and the CPU throttles while the battery simply degrades (ADR 0011).
    ///
    /// Resolved by label, never by index — sensor ordering is no more stable than
    /// hwmon numbering.
    pub fn battery_temp(&self) -> Option<&TempReading> {
        self.temps.iter().find(|t| t.label.starts_with("battery"))
    }

    /// The sensor a fan curve should follow. `peci-temp` is the CPU package on the
    /// reference board; fall back to anything CPU-ish, then to the hottest reading.
    pub fn control_temp(&self) -> Option<&TempReading> {
        self.temps
            .iter()
            .find(|t| t.label == "peci-temp")
            .or_else(|| self.temps.iter().find(|t| t.label.contains("cpu")))
            .or_else(|| {
                self.temps
                    .iter()
                    .max_by(|a, b| a.celsius.total_cmp(&b.celsius))
            })
    }
}

/// Polls hardware and produces [`Telemetry`].
pub struct Monitor {
    fs: Sysfs,
    caps: Capabilities,
    energy: Option<EnergySampler>,
    watts_floor: Option<f64>,
    /// A reading below the floor waits here for a second one to confirm it.
    pending_low: Option<f64>,
}

impl Monitor {
    pub fn new(fs: Sysfs) -> Self {
        let caps = Capabilities::probe(&fs);
        let energy = caps.energy_zone.as_ref().and_then(|zone| {
            fs.read_u64(&format!("{zone}/max_energy_range_uj"))
                .ok()
                .map(EnergySampler::new)
        });
        Self {
            fs,
            caps,
            energy,
            watts_floor: None,
            pending_low: None,
        }
    }

    /// Track the lowest package power seen, as the machine's idle floor.
    ///
    /// The floor only ever falls, and it takes **two consecutive** readings below the
    /// current value to move it. That second reading is the whole point. The fan floor
    /// in this project learned the same lesson the expensive way: a value that only
    /// rises within a bucket let one bad sample stick permanently, and a table came back
    /// claiming 5200 rpm at a temperature where its neighbours said 2000. A minimum that
    /// accepts any single low sample has exactly that shape, and a floor pinned too low
    /// would over-attribute power to processes for the rest of the daemon's life.
    ///
    /// Readings at or below 0.05 W are refused outright: a running package cannot draw
    /// nothing, so such a sample is a wrapped or mis-scaled counter rather than an
    /// unusually quiet machine.
    fn observe_floor(&mut self, watts: f64) {
        if !watts.is_finite() || watts <= 0.05 {
            return;
        }
        match self.watts_floor {
            None => self.watts_floor = Some(watts),
            Some(floor) if watts < floor => match self.pending_low.take() {
                // Confirmed. Take the higher of the pair, so the floor is never set by
                // whichever of the two happened to be the lower outlier.
                Some(previous) => self.watts_floor = Some(previous.max(watts)),
                None => self.pending_low = Some(watts),
            },
            _ => self.pending_low = None,
        }
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Call on resume from suspend: the energy counter may have reset while
    /// wall-clock advanced, so the reference point is no longer meaningful.
    pub fn on_resume(&mut self) {
        if let Some(e) = self.energy.as_mut() {
            e.invalidate();
        }
    }

    pub fn sample(&mut self) -> Telemetry {
        let mut t = Telemetry {
            platform_profile: self.fs.read_string(paths::PLATFORM_PROFILE).ok(),
            battery_percent: self
                .fs
                .read_u64(&format!("{}/capacity", paths::BATTERY))
                .ok(),
            battery_status: self
                .fs
                .read_string(&format!("{}/status", paths::BATTERY))
                .ok(),
            ..Default::default()
        };

        t.on_ac = self.read_on_ac();
        let (watts, minutes) = self.read_battery_rate();
        t.system_watts = watts;
        t.battery_minutes = minutes;

        if let Some(hwmon) = self.caps.ec_hwmon.clone() {
            // fan1_target stayed 0 under manual control during hardware validation
            // (baseline Q4) — fan1_input is the only trustworthy RPM source.
            t.fan_rpm = self.fs.read_u64(&format!("{hwmon}/fan1_input")).ok();
            t.temps = self.read_temps(&hwmon);
        }

        if let (Some(sampler), Some(zone)) = (self.energy.as_mut(), self.caps.energy_zone.as_ref())
        {
            let path = format!("{zone}/energy_uj");
            if let Ok(uj) = self.fs.read_u64(&path) {
                t.package_watts = sampler
                    .sample(uj, Instant::now())
                    .map(EnergySampler::quantize);
            }
        }
        if let Some(w) = t.package_watts {
            self.observe_floor(w);
        }
        t.package_watts_floor = self.watts_floor;

        t
    }

    /// Discharge rate and time remaining, in whichever unit family the board uses.
    ///
    /// Two conventions exist and boards pick one: **energy** (`power_now` in µW,
    /// `energy_now` in µWh) or **charge** (`current_now` in µA, `voltage_now` in µV,
    /// `charge_now` in µAh). The reference machine reports the charge family and has no
    /// `power_now` at all, so reading only the energy one would silently show nothing.
    ///
    /// Returns `(None, None)` while charging or on mains: the current then describes
    /// what is going *into* the battery, which is not what the machine is using and
    /// gives no time-to-empty at all.
    fn read_battery_rate(&self) -> (Option<f64>, Option<u64>) {
        let bat = paths::BATTERY;
        if self
            .fs
            .read_string(&format!("{bat}/status"))
            .ok()
            .as_deref()
            != Some("Discharging")
        {
            return (None, None);
        }

        // Energy family first: when present it is already in watts and needs no
        // multiplication.
        if let Ok(uw) = self.fs.read_u64(&format!("{bat}/power_now")) {
            let watts = uw as f64 / 1_000_000.0;
            let minutes = self
                .fs
                .read_u64(&format!("{bat}/energy_now"))
                .ok()
                .filter(|_| uw > 0)
                .map(|uwh| (uwh as f64 * 60.0 / uw as f64) as u64);
            return (Some(watts), minutes);
        }

        // Charge family: watts is V x I, and hours is charge over current.
        let (Ok(ua), Ok(uv)) = (
            self.fs.read_u64(&format!("{bat}/current_now")),
            self.fs.read_u64(&format!("{bat}/voltage_now")),
        ) else {
            return (None, None);
        };
        if ua == 0 {
            return (None, None);
        }
        let watts = (ua as f64 / 1_000_000.0) * (uv as f64 / 1_000_000.0);
        let minutes = self
            .fs
            .read_u64(&format!("{bat}/charge_now"))
            .ok()
            .map(|uah| (uah as f64 * 60.0 / ua as f64) as u64);
        (Some(watts), minutes)
    }

    /// Find the mains supply by type and read whether it is online.
    fn read_on_ac(&self) -> Option<bool> {
        let dir = self.fs.path("sys/class/power_supply");
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let base = entry.file_name().to_str()?.to_string();
            let rel = format!("sys/class/power_supply/{base}");
            if self.fs.read_string(&format!("{rel}/type")).ok().as_deref() != Some("Mains") {
                continue;
            }
            if let Ok(online) = self.fs.read_u64(&format!("{rel}/online")) {
                return Some(online == 1);
            }
        }
        None
    }

    fn read_temps(&self, hwmon: &str) -> Vec<TempReading> {
        let mut out = Vec::new();
        for i in 1..=16 {
            let Ok(milli) = self.fs.read_i64(&format!("{hwmon}/temp{i}_input")) else {
                continue;
            };
            let label = self
                .fs
                .read_string(&format!("{hwmon}/temp{i}_label"))
                .unwrap_or_else(|_| format!("temp{i}"));
            out.push(TempReading {
                label,
                celsius: milli as f64 / 1000.0,
                critical: self
                    .fs
                    .read_i64(&format!("{hwmon}/temp{i}_crit"))
                    .ok()
                    .map(|c| c as f64 / 1000.0)
                    .filter(|c| Self::plausible_threshold(*c)),
            });
        }
        out
    }

    /// Guards against the unset-sensor case: `temp*_max` reads -273150 on the
    /// reference board, and trusting it would put the fan-safety ceiling at
    /// absolute zero, disabling manual control permanently (ADR 0006).
    fn plausible_threshold(celsius: f64) -> bool {
        (0.0..=150.0).contains(&celsius)
    }
}

/// Human-readable one-liner used by `fw-helperctl status` and, later, logs.
pub fn describe(cap: &Cap) -> String {
    cap.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unset_temperature_thresholds() {
        // the -273150 millidegree "unset" value seen on real hardware
        assert!(!Monitor::plausible_threshold(-273.15));
        assert!(Monitor::plausible_threshold(87.85));
        assert!(!Monitor::plausible_threshold(1000.0));
    }

    #[test]
    fn finds_the_battery_sensor_by_label() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "peci-temp".into(),
                    celsius: 70.0,
                    critical: Some(119.85),
                },
                TempReading {
                    label: "battery_temp@b".into(),
                    celsius: 38.0,
                    critical: Some(49.9),
                },
            ],
            ..Default::default()
        };
        let b = t.battery_temp().expect("battery sensor");
        assert_eq!(b.celsius, 38.0);
        assert_eq!(b.critical, Some(49.9));
        // And it must not be mistaken for the fan curve's input.
        assert_eq!(t.control_temp().unwrap().label, "peci-temp");
    }

    #[test]
    fn no_battery_sensor_is_not_an_error() {
        let t = Telemetry {
            temps: vec![TempReading {
                label: "peci-temp".into(),
                celsius: 70.0,
                critical: None,
            }],
            ..Default::default()
        };
        assert!(t.battery_temp().is_none());
    }

    #[test]
    fn control_temp_prefers_peci() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "ddr_f75303@4d".into(),
                    celsius: 90.0,
                    critical: None,
                },
                TempReading {
                    label: "peci-temp".into(),
                    celsius: 46.8,
                    critical: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.control_temp().unwrap().label, "peci-temp");
    }

    #[test]
    fn control_temp_falls_back_to_hottest() {
        let t = Telemetry {
            temps: vec![
                TempReading {
                    label: "battery_temp@b".into(),
                    celsius: 30.0,
                    critical: None,
                },
                TempReading {
                    label: "local_f75397@4c".into(),
                    celsius: 55.0,
                    critical: None,
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.control_temp().unwrap().celsius, 55.0);
    }
}

#[cfg(test)]
mod floor_tests {
    use super::*;

    fn monitor() -> Monitor {
        Monitor::new(Sysfs::new("/nonexistent-for-floor-tests"))
    }

    #[test]
    fn the_first_reading_sets_the_floor() {
        let mut m = monitor();
        m.observe_floor(6.0);
        assert_eq!(m.watts_floor, Some(6.0));
    }

    #[test]
    fn a_single_low_reading_does_not_move_the_floor() {
        // The fan floor learned this the expensive way: a table that accepted any single
        // sample kept one 5200 rpm outlier forever, against neighbours saying 2000. A
        // baseline pinned too low would over-attribute power for the daemon's lifetime.
        let mut m = monitor();
        m.observe_floor(6.0);
        m.observe_floor(0.5); // one anomaly
        assert_eq!(m.watts_floor, Some(6.0), "one low sample moved the floor");
        m.observe_floor(6.2); // back to normal: the anomaly is forgotten
        m.observe_floor(0.5);
        assert_eq!(m.watts_floor, Some(6.0), "a later lone sample moved it");
    }

    #[test]
    fn two_consecutive_low_readings_move_it_to_the_higher_of_the_pair() {
        let mut m = monitor();
        m.observe_floor(6.0);
        m.observe_floor(3.6);
        m.observe_floor(3.9);
        // The higher of the confirmed pair, so the floor is never set by whichever of
        // the two happened to be the lower outlier.
        assert_eq!(m.watts_floor, Some(3.9));
    }

    #[test]
    fn the_floor_never_rises() {
        let mut m = monitor();
        m.observe_floor(4.0);
        m.observe_floor(2.0);
        m.observe_floor(2.1);
        assert_eq!(m.watts_floor, Some(2.1));
        for w in [30.0, 12.0, 8.0] {
            m.observe_floor(w);
        }
        assert_eq!(m.watts_floor, Some(2.1), "load raised the idle floor");
    }

    #[test]
    fn implausible_readings_are_refused() {
        // A running package cannot draw nothing; such a sample is a wrapped or
        // mis-scaled counter, and accepting it would pin the floor at zero forever.
        let mut m = monitor();
        m.observe_floor(5.0);
        for bad in [0.0, -1.0, 0.01, f64::NAN, f64::INFINITY] {
            m.observe_floor(bad);
            m.observe_floor(bad);
        }
        assert_eq!(m.watts_floor, Some(5.0));
    }
}
