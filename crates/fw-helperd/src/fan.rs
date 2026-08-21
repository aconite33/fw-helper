//! Daemon-side ownership of manual fan control.
//!
//! `fw-helper-core` provides the mechanism; this decides *when* the daemon holds the
//! lease and guarantees it lets go. ADR 0006 point 1: `pwm1_enable=2` on clean
//! shutdown, on `SIGTERM`/`SIGINT`, and on panic.
//!
//! Two design choices are about the failure paths rather than the happy one:
//!
//! - **No mutex.** The panic hook calls into here, and a panic can happen while some
//!   other thread holds a lock — a hook that blocks on one would hang the process
//!   with the fan still held. State is a single [`AtomicBool`] and everything else is
//!   asked of the hardware directly.
//! - **Releases are unconditional.** `release_now` writes `2` whether or not we think
//!   we hold the lease. The flag can be wrong in exactly the situation that matters —
//!   a previous instance killed while holding it — and the write is idempotent and
//!   cheap, so believing our own bookkeeping buys nothing and risks everything.

use fw_helper_core::{fan::MIN_TAKEOVER_DUTY, FanControl, FanError, FanMode, Sysfs};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Lowest duty the daemon will run the fan at, for now.
///
/// **Temporary, and deliberately conservative.** ADR 0006 point 4 calls for a clamp
/// against the EC's own floor for the current temperature, so that our curve may only
/// ever be *more* aggressive than firmware. That clamp does not exist yet, and until
/// it does there is nothing standing between a requested duty and a fan that is too
/// slow for the load. A flat floor is a poor substitute — it is quieter than the EC
/// at high temperature and louder at idle — but it is a floor, and it is honest about
/// being one. Replace it with the real clamp, do not merely lower it.
pub const MIN_DUTY: u8 = MIN_TAKEOVER_DUTY;

#[derive(Debug)]
pub enum LeaseError {
    /// Below [`MIN_DUTY`]. Not a hardware failure — a policy refusal.
    BelowFloor(u8),
    Fan(FanError),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BelowFloor(d) => write!(
                f,
                "duty {d}/255 is below the {MIN_DUTY}/255 floor this build enforces; \
                 the firmware-floor clamp that would make lower duties safe is not \
                 implemented yet (ADR 0006 point 4). Use 'auto' to hand the fan back \
                 to the EC, which can run it slower safely"
            ),
            Self::Fan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<FanError> for LeaseError {
    fn from(e: FanError) -> Self {
        Self::Fan(e)
    }
}

pub struct FanLease {
    fs: Sysfs,
    /// Whether *this process* believes it holds manual control. Used for logging and
    /// for deciding whether a shutdown is worth announcing — never as a precondition
    /// for releasing.
    held: AtomicBool,
}

impl FanLease {
    pub fn new(fs: Sysfs) -> Self {
        Self {
            fs,
            held: AtomicBool::new(false),
        }
    }

    fn control(&self) -> Result<FanControl<'_>, FanError> {
        FanControl::probe(&self.fs)
    }

    pub fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    pub fn mode(&self) -> Option<FanMode> {
        self.control().ok()?.mode().ok()
    }

    pub fn duty(&self) -> Option<u8> {
        self.control().ok()?.duty().ok()
    }

    /// Reclaim the fan at startup if it was left under manual control.
    ///
    /// The only way to arrive here is a previous instance dying without releasing and
    /// `ExecStopPost` not covering it either. Like M2's charge re-apply, this reads
    /// before writing so the log distinguishes "nothing to do" from "something had
    /// gone wrong", which makes every start a free check on whether the restore paths
    /// are actually working.
    pub fn reclaim_at_startup(&self) {
        match self.mode() {
            Some(FanMode::Auto) | None => {}
            Some(other) => {
                eprintln!("fan was left under {other} control by a previous instance; reclaiming");
                if self.release_now() {
                    eprintln!("fan returned to EC control");
                } else {
                    eprintln!("WARNING: could not return the fan to EC control");
                }
            }
        }
    }

    /// Set the fan duty, taking manual control first if we do not already hold it.
    ///
    /// Returns the duty the EC actually settled on, which is not necessarily what was
    /// asked for — the EC quantizes to whole percent.
    pub fn set_duty(&self, duty: u8) -> Result<u8, LeaseError> {
        if duty < MIN_DUTY {
            return Err(LeaseError::BelowFloor(duty));
        }
        let fan = self.control()?;

        // Trust the hardware over our own flag: if something else handed the fan back
        // to the EC, take it again rather than issue a write that would be rejected.
        let settled = if fan.mode()?.is_manual() {
            fan.set_duty(duty)?
        } else {
            fan.take_manual(duty)?
        };
        // Only after a verified write. take_manual releases on failure, so recording
        // the lease before this point could leave the flag set with the fan in EC
        // control — harmless, but it would make the shutdown log lie.
        self.held.store(true, Ordering::SeqCst);
        Ok(settled)
    }

    /// Hand the fan back and confirm the EC took it.
    pub fn release(&self) -> Result<(), LeaseError> {
        let r = self.control()?.release();
        self.held.store(false, Ordering::SeqCst);
        Ok(r?)
    }

    /// Release without the ability to fail, for panic hooks and signal handlers.
    ///
    /// Allocates nothing, locks nothing, and does not care what we believed about the
    /// lease. Returns whether the fan is now demonstrably in EC control.
    pub fn release_now(&self) -> bool {
        let ok = match self.control() {
            Ok(fan) => fan.release_best_effort(),
            // No fan control on this machine: nothing to restore, so nothing is wrong.
            Err(_) => true,
        };
        self.held.store(false, Ordering::SeqCst);
        ok
    }
}

impl Drop for FanLease {
    fn drop(&mut self) {
        // Last line of in-process defence. Ordinary shutdown releases explicitly and
        // this is then a no-op write.
        if !self.release_now() {
            eprintln!("WARNING: fan not confirmed back under EC control at shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU32;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fixture(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helperd-fan-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        let hwmon = root.join("sys/class/hwmon/hwmon7");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("name"), "cros_ec\n").unwrap();
        fs::write(hwmon.join("pwm1_enable"), "2\n").unwrap();
        fs::write(hwmon.join("pwm1"), "0\n").unwrap();
        fs::write(hwmon.join("fan1_input"), "0\n").unwrap();
        root
    }

    fn enable(root: &Path) -> String {
        fs::read_to_string(root.join("sys/class/hwmon/hwmon7/pwm1_enable"))
            .unwrap()
            .trim()
            .into()
    }

    #[test]
    fn refuses_a_duty_below_the_floor_without_touching_hardware() {
        let root = fixture("floor");
        let lease = FanLease::new(Sysfs::new(&root));

        assert!(matches!(
            lease.set_duty(MIN_DUTY - 1),
            Err(LeaseError::BelowFloor(_))
        ));
        assert_eq!(enable(&root), "2", "the EC must still own the fan");
        assert!(!lease.held());
    }

    #[test]
    fn floor_error_points_at_auto_as_the_way_to_go_quieter() {
        // A user who wants a quieter fan must not be left with no route at all.
        let msg = LeaseError::BelowFloor(10).to_string();
        assert!(msg.contains("auto"), "got: {msg}");
    }

    #[test]
    fn takes_the_lease_then_gives_it_back() {
        let root = fixture("lease");
        let lease = FanLease::new(Sysfs::new(&root));

        assert_eq!(lease.set_duty(200).unwrap(), 200);
        assert!(lease.held());
        assert_eq!(enable(&root), "1");

        lease.release().unwrap();
        assert!(!lease.held());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn release_now_works_whatever_we_believed() {
        let root = fixture("unconditional");
        let lease = FanLease::new(Sysfs::new(&root));
        lease.set_duty(200).unwrap();

        // Simulate bookkeeping that has gone wrong: the flag says we hold nothing,
        // the hardware says otherwise. The hardware wins.
        lease.held.store(false, Ordering::SeqCst);
        assert!(lease.release_now());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn reclaims_a_fan_left_manual_by_a_previous_instance() {
        let root = fixture("reclaim");
        fs::write(root.join("sys/class/hwmon/hwmon7/pwm1_enable"), "1\n").unwrap();

        FanLease::new(Sysfs::new(&root)).reclaim_at_startup();
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn startup_reclaim_leaves_an_untouched_fan_alone() {
        let root = fixture("no-reclaim");
        let lease = FanLease::new(Sysfs::new(&root));
        lease.reclaim_at_startup();
        assert_eq!(enable(&root), "2");
        assert!(!lease.held());
    }

    #[test]
    fn a_dropped_lease_releases() {
        let root = fixture("drop");
        {
            let lease = FanLease::new(Sysfs::new(&root));
            lease.set_duty(200).unwrap();
            assert_eq!(enable(&root), "1");
        }
        assert_eq!(enable(&root), "2", "Drop must have handed the fan back");
    }
}
