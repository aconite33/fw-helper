//! Persisted daemon state.
//!
//! `charge_control_end_threshold` does not survive a reboot, so the desired value
//! is stored here and re-applied at startup. Kept in the daemon rather than in
//! `fw-helper-core`, which stays free of config parsing (ADR 0010).
//!
//! It also holds the observed firmware fan floor. That is learned by watching the EC
//! while it owns the fan, and losing it on every restart is not cosmetic: the cold-start
//! model is built from descending-branch measurements and is the *loud* one, so a fresh
//! daemon overrides a quiet curve until it has watched a heating cycle. Measured — a
//! curve asking for silence at 55 °C got duty 61 immediately after a restart.
//!
//! Format is deliberately trivial `key=value` — this holds a handful of integers and one
//! list, not a configuration language.

use std::fs;
use std::path::PathBuf;

const STATE_DIR: &str = "/var/lib/fw-helper";
const STATE_FILE: &str = "state";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    pub charge_limit: Option<u8>,
    /// Sustained CPU power limit in watts. Firmware commonly resets these across
    /// suspend and they do not survive a reboot, so the desired value lives here.
    pub power_limit: Option<u32>,
    /// Active profile name, re-applied at startup.
    pub profile: Option<String>,
    /// Observed firmware fan duty by temperature, as `(celsius, duty)`.
    pub floor: Vec<(f64, u8)>,
}

fn path() -> PathBuf {
    PathBuf::from(STATE_DIR).join(STATE_FILE)
}

/// Parse `55:0,60:40,...`. Malformed entries are skipped rather than failing the whole
/// file: a damaged floor costs some quiet until it is relearned, while refusing to load
/// would also lose the charge limit, which matters more.
fn parse_floor(value: &str) -> Vec<(f64, u8)> {
    value
        .split(',')
        .filter_map(|pair| {
            let (c, d) = pair.trim().split_once(':')?;
            Some((c.trim().parse().ok()?, d.trim().parse().ok()?))
        })
        .collect()
}

impl State {
    pub fn load() -> Self {
        let Ok(text) = fs::read_to_string(path()) else {
            return Self::default();
        };
        let mut s = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "charge_limit" => s.charge_limit = value.trim().parse().ok(),
                "power_limit" => s.power_limit = value.trim().parse().ok(),
                "profile" => s.profile = Some(value.trim().to_string()).filter(|v| !v.is_empty()),
                "fan_floor" => s.floor = parse_floor(value),
                _ => {}
            }
        }
        s
    }

    /// Best-effort. Failing to persist must not fail the hardware change that was
    /// already applied successfully — report and carry on.
    pub fn save(&self) {
        if let Err(e) = fs::create_dir_all(STATE_DIR) {
            eprintln!("cannot create {STATE_DIR}: {e}");
            return;
        }
        let mut out = String::from("# written by fw-helperd\n");
        if let Some(v) = self.charge_limit {
            out.push_str(&format!("charge_limit={v}\n"));
        }
        if let Some(v) = self.power_limit {
            out.push_str(&format!("power_limit={v}\n"));
        }
        if let Some(v) = &self.profile {
            out.push_str(&format!("profile={v}\n"));
        }
        if !self.floor.is_empty() {
            let pairs: Vec<String> = self
                .floor
                .iter()
                .map(|(c, d)| format!("{c:.0}:{d}"))
                .collect();
            out.push_str(&format!("fan_floor={}\n", pairs.join(",")));
        }
        if let Err(e) = fs::write(path(), out) {
            eprintln!("cannot write state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_floor;

    #[test]
    fn parses_a_floor_list() {
        assert_eq!(
            parse_floor("44:0,60:40,70:92"),
            vec![(44.0, 0u8), (60.0, 40), (70.0, 92)]
        );
    }

    #[test]
    fn skips_damaged_entries_rather_than_losing_the_whole_file() {
        // The charge limit lives in the same file and matters more than some quiet.
        assert_eq!(
            parse_floor("44:0,rubbish,70:92,80:"),
            vec![(44.0, 0u8), (70.0, 92)]
        );
        assert_eq!(parse_floor(""), vec![]);
    }

    #[test]
    fn out_of_range_duties_do_not_parse_as_something_else() {
        // 300 does not fit a u8 and must be dropped, not wrapped to 44.
        assert_eq!(parse_floor("60:300"), vec![]);
    }
}
