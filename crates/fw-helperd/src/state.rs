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
    /// Profiles to switch to when the power source changes. Both `None` means the
    /// feature is off, which is the default: a machine that changes behaviour when a
    /// cable is plugged in, without being asked to, is a machine behaving strangely.
    pub profile_on_ac: Option<String>,
    pub profile_on_battery: Option<String>,
    /// Observed firmware fan duty by temperature, as `(celsius, duty)`.
    pub floor: Vec<(f64, u8)>,
    /// DMI board name the floor was learned on.
    ///
    /// A learned floor describes one board's firmware and one board's fan. The state
    /// file outlives the hardware - it moves with the disk - so without this a floor
    /// learned on one board would bound the fan on the next.
    pub floor_board: Option<String>,
}

/// Whether a stored floor may be used on `current`.
///
/// Carried across boards only when both resolve to the same profile: the two AMD boards
/// share a fan and a firmware curve, so a floor from one is a floor for the other.
///
/// A floor with no board recorded predates the field. Builds before it could learn a
/// floor only where `pwm1` reports firmware's duty - the Intel board - so such a floor is
/// kept there and dropped anywhere else.
pub fn floor_applies(floor_board: Option<&str>, current: fw_helper_core::board::Board) -> bool {
    use fw_helper_core::board::{identify_name, INTEL_CORE_ULTRA_3};
    let Some(current) = current.profile() else {
        return false;
    };
    match floor_board {
        Some(name) => identify_name(name)
            .profile()
            .is_some_and(|learned| learned.name == current.name),
        None => current.name == INTEL_CORE_ULTRA_3.name,
    }
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
                "profile_on_ac" => {
                    s.profile_on_ac = Some(value.trim().to_string()).filter(|v| !v.is_empty())
                }
                "profile_on_battery" => {
                    s.profile_on_battery = Some(value.trim().to_string()).filter(|v| !v.is_empty())
                }
                "fan_floor" => s.floor = parse_floor(value),
                "fan_floor_board" => {
                    s.floor_board = Some(value.trim().to_string()).filter(|v| !v.is_empty())
                }
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
        if let Some(v) = &self.profile_on_ac {
            out.push_str(&format!("profile_on_ac={v}\n"));
        }
        if let Some(v) = &self.profile_on_battery {
            out.push_str(&format!("profile_on_battery={v}\n"));
        }
        if !self.floor.is_empty() {
            let pairs: Vec<String> = self
                .floor
                .iter()
                .map(|(c, d)| format!("{c:.0}:{d}"))
                .collect();
            out.push_str(&format!("fan_floor={}\n", pairs.join(",")));
            if let Some(board) = &self.floor_board {
                out.push_str(&format!("fan_floor_board={board}\n"));
            }
        }
        if let Err(e) = fs::write(path(), out) {
            eprintln!("cannot write state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{floor_applies, parse_floor};
    use fw_helper_core::board::{identify_name, Board};

    #[test]
    fn a_floor_follows_its_own_board() {
        assert!(floor_applies(
            Some("FRANMGCP09"),
            identify_name("FRANMGCP09")
        ));
    }

    #[test]
    fn a_floor_carries_between_boards_that_share_a_fan() {
        // Both AMD boards: same fan to within 1%, same firmware curve.
        assert!(floor_applies(
            Some("FRANMGCP05"),
            identify_name("FRANMGCP09")
        ));
    }

    #[test]
    fn a_floor_does_not_cross_to_a_different_board() {
        // The disk moved from an Intel machine to an AMD one, or back. Either way the
        // stored floor describes firmware and a fan that are no longer there.
        assert!(!floor_applies(
            Some("FRANMJCP07"),
            identify_name("FRANMGCP09")
        ));
        assert!(!floor_applies(
            Some("FRANMGCP09"),
            identify_name("FRANMJCP07")
        ));
    }

    #[test]
    fn an_unrecorded_floor_is_kept_only_where_old_builds_could_learn_one() {
        assert!(floor_applies(None, identify_name("FRANMJCP07")));
        assert!(!floor_applies(None, identify_name("FRANMGCP09")));
    }

    #[test]
    fn no_floor_applies_to_an_unmeasured_board() {
        assert!(!floor_applies(Some("FRANMGCP09"), Board::Unknown));
        assert!(!floor_applies(None, Board::Unknown));
    }

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
