//! Persisted daemon state.
//!
//! `charge_control_end_threshold` does not survive a reboot, so the desired value
//! is stored here and re-applied at startup. Kept in the daemon rather than in
//! `fw-helper-core`, which stays free of config parsing (ADR 0010).
//!
//! Format is deliberately trivial `key=value` — this holds a handful of integers,
//! not a configuration language.

use std::fs;
use std::path::PathBuf;

const STATE_DIR: &str = "/var/lib/fw-helper";
const STATE_FILE: &str = "state";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    pub charge_limit: Option<u8>,
}

fn path() -> PathBuf {
    PathBuf::from(STATE_DIR).join(STATE_FILE)
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
            if key.trim() == "charge_limit" {
                s.charge_limit = value.trim().parse().ok();
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
        if let Err(e) = fs::write(path(), out) {
            eprintln!("cannot write state: {e}");
        }
    }
}
