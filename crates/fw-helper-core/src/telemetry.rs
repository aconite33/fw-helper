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
    pub battery_percent: Option<u64>,
    pub battery_status: Option<String>,
    pub platform_profile: Option<String>,
}

impl Telemetry {
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
}

impl Monitor {
    pub fn new(fs: Sysfs) -> Self {
        let caps = Capabilities::probe(&fs);
        let energy = if caps.package_power.is_available() {
            fs.read_u64(&format!("{}/max_energy_range_uj", paths::RAPL_MMIO))
                .ok()
                .map(EnergySampler::new)
        } else {
            None
        };
        Self { fs, caps, energy }
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

        if let Some(hwmon) = self.caps.ec_hwmon.clone() {
            // fan1_target stayed 0 under manual control during hardware validation
            // (baseline Q4) — fan1_input is the only trustworthy RPM source.
            t.fan_rpm = self.fs.read_u64(&format!("{hwmon}/fan1_input")).ok();
            t.temps = self.read_temps(&hwmon);
        }

        if let Some(sampler) = self.energy.as_mut() {
            let path = format!("{}/energy_uj", paths::RAPL_MMIO);
            if let Ok(uj) = self.fs.read_u64(&path) {
                t.package_watts = sampler
                    .sample(uj, Instant::now())
                    .map(EnergySampler::quantize);
            }
        }

        t
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
