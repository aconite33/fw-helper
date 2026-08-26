//! Battery charge limit.
//!
//! The first hardware write in the project, and deliberately the mildest: a bad
//! value here means the battery charges to the wrong percentage, not that the
//! machine overheats.
//!
//! On Framework hardware the kernel driver refuses to bind unless
//! `cros_charge-control.probe_with_fwk_charge_control=1` is set, because Framework's
//! EC implements a *custom* charge control command alongside the standard one and the
//! custom one can override it. See ADR 0008.
//!
//! **This path does not work on the target machine, and never has.** Measured
//! 2026-08-26: threshold written as 80, read back as 80, persisted and re-applied
//! across suspend and reboot — and the battery charged straight through to 100%
//! (`charge_now` 4804000 of `charge_full` 4806000). No UEFI battery limit is set, so
//! the override is the EC firmware's own custom command, exactly the case upstream
//! declines to bind into.
//!
//! The read-back below therefore proves *less* than it appears to. It confirms the
//! value the kernel stores, not that charging stops — and this firmware leaves the
//! value alone while ignoring it, so a mismatch never occurs and
//! [`ChargeError::NotApplied`] can never fire for the failure that actually happens.
//! **Read-back is not efficacy.** The only honest test is watching `charge_now` and
//! `status` across the threshold. Kept because it is still the right check for a
//! firmware that *does* write the value back.

use crate::{paths, Sysfs};
use std::fmt;

/// Below this a "limit" is almost certainly a typo rather than an intent. The EC
/// would accept smaller values; we decline to pass them on.
pub const MIN_LIMIT: u8 = 20;
pub const MAX_LIMIT: u8 = 100;

const ATTR: &str = "charge_control_end_threshold";

#[derive(Debug)]
pub enum ChargeError {
    /// No sysfs attribute. Usually means the module parameter is unset (ADR 0008).
    Unsupported,
    OutOfRange(u8),
    Io(std::io::Error),
    /// Written successfully, but reading back gave something else.
    ///
    /// Note what this does *not* catch: the measured failure on this board leaves the
    /// value intact and ignores it, so this variant stays silent through it. See the
    /// module note.
    NotApplied {
        requested: u8,
        observed: u8,
    },
}

impl fmt::Display for ChargeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "charge control unavailable; set \
                 cros_charge-control.probe_with_fwk_charge_control=1 (see ADR 0008)"
            ),
            Self::OutOfRange(v) => {
                write!(
                    f,
                    "{v}% is outside the accepted range {MIN_LIMIT}–{MAX_LIMIT}%"
                )
            }
            Self::Io(e) => write!(f, "{e}"),
            Self::NotApplied {
                requested,
                observed,
            } => write!(
                f,
                "wrote {requested}% but the EC reports {observed}%; \
                 something else is governing charging — check for a battery limit \
                 in UEFI setup"
            ),
        }
    }
}

impl std::error::Error for ChargeError {}

impl From<std::io::Error> for ChargeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub struct ChargeControl<'a> {
    fs: &'a Sysfs,
}

impl<'a> ChargeControl<'a> {
    pub fn new(fs: &'a Sysfs) -> Self {
        Self { fs }
    }

    fn attr(&self) -> String {
        format!("{}/{ATTR}", paths::BATTERY)
    }

    pub fn is_supported(&self) -> bool {
        self.fs.exists(&self.attr())
    }

    pub fn read(&self) -> Result<u8, ChargeError> {
        if !self.is_supported() {
            return Err(ChargeError::Unsupported);
        }
        let v = self.fs.read_u64(&self.attr())?;
        Ok(v.min(u64::from(u8::MAX)) as u8)
    }

    /// Set the limit and confirm the EC actually took it.
    ///
    /// The read-back is not defensive padding — see the module note. A silent
    /// override is the expected failure on this hardware, so it must surface as a
    /// specific, actionable error rather than as apparent success.
    pub fn set(&self, percent: u8) -> Result<(), ChargeError> {
        if !(MIN_LIMIT..=MAX_LIMIT).contains(&percent) {
            return Err(ChargeError::OutOfRange(percent));
        }
        if !self.is_supported() {
            return Err(ChargeError::Unsupported);
        }
        self.fs.write_string(&self.attr(), &percent.to_string())?;

        let observed = self.read()?;
        if observed != percent {
            return Err(ChargeError::NotApplied {
                requested: percent,
                observed,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_values_outside_the_accepted_range() {
        let fs = Sysfs::new("/nonexistent");
        let c = ChargeControl::new(&fs);
        // Range is checked before support, so a typo reports as a typo even on a
        // machine where charge control is unavailable.
        assert!(matches!(c.set(0), Err(ChargeError::OutOfRange(0))));
        assert!(matches!(c.set(19), Err(ChargeError::OutOfRange(19))));
        assert!(matches!(c.set(101), Err(ChargeError::OutOfRange(101))));
    }

    #[test]
    fn reports_unsupported_when_the_attribute_is_absent() {
        let fs = Sysfs::new("/nonexistent");
        let c = ChargeControl::new(&fs);
        assert!(matches!(c.read(), Err(ChargeError::Unsupported)));
        assert!(matches!(c.set(80), Err(ChargeError::Unsupported)));
    }

    #[test]
    fn unsupported_error_names_the_module_parameter() {
        // The message is the user's only route to fixing this, so assert on it.
        let msg = ChargeError::Unsupported.to_string();
        assert!(msg.contains("probe_with_fwk_charge_control"), "got: {msg}");
    }

    #[test]
    fn not_applied_error_reports_both_values_and_names_a_check() {
        let msg = ChargeError::NotApplied {
            requested: 80,
            observed: 100,
        }
        .to_string();
        // It must not assert a cause it cannot know. On the target machine no UEFI
        // limit is set and the EC overrides us anyway, so "UEFI is probably
        // overriding it" was a guess stated as the likely fact.
        assert!(msg.contains("80"), "got: {msg}");
        assert!(msg.contains("100"), "got: {msg}");
        assert!(msg.contains("UEFI setup"), "got: {msg}");
    }
}
