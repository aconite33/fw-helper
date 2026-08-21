//! Independent watchdog over manual fan control.
//!
//! ADR 0006 point 3. `ExecStopPost` covers the daemon *dying*; the signal and panic
//! hooks cover it exiting deliberately. Neither covers the daemon still being alive
//! and simply not working — a deadlock, a wedged async runtime, scheduler starvation.
//! From outside, that looks identical to a healthy daemon, and the fan sits at
//! whatever duty it was last given, forever.
//!
//! Three decisions make this actually independent rather than nominally so:
//!
//! - **A real OS thread, not a `tokio` task.** The failure being guarded against
//!   includes the runtime not scheduling anything. A task waiting on that same
//!   runtime would be wedged alongside everything else, which is worse than no
//!   watchdog because it looks like protection.
//! - **The trip condition is read from hardware, not from our own flag.** The
//!   question is "is the fan under manual control while nobody is minding it", and
//!   `pwm1_enable` answers that directly. In-process state is the thing least worth
//!   trusting when the process is suspect — and reading hardware means a failed
//!   release is retried on the next tick instead of being forgotten.
//! - **The heartbeat is the telemetry poll loop**, because that is what proves the
//!   runtime is still turning. A dedicated timer that only proved *itself* alive
//!   would be circular.

use crate::fan::FanLease;
use fw_helper_core::FanMode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the heartbeat may go unrefreshed before the fan is taken away from us.
///
/// The poll loop ticks at 1 Hz, so this is five missed ticks — comfortably past
/// scheduler jitter, and ADR 0006's stated bound.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// How often the watchdog thread wakes to check.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub struct Watchdog {
    lease: Arc<FanLease>,
    start: Instant,
    /// Milliseconds since `start` at the last heartbeat. An atomic rather than a
    /// mutex so neither side can ever block the other.
    last_beat_ms: AtomicU64,
    timeout: Duration,
    tripped: AtomicBool,
}

impl Watchdog {
    pub fn new(lease: Arc<FanLease>) -> Arc<Self> {
        Self::with_timeout(lease, TIMEOUT)
    }

    pub fn with_timeout(lease: Arc<FanLease>, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            lease,
            start: Instant::now(),
            last_beat_ms: AtomicU64::new(0),
            timeout,
            tripped: AtomicBool::new(false),
        })
    }

    /// Called from the poll loop every tick. Cheap and lock-free by design: it runs
    /// on the hot path of a loop whose whole job is to be regular.
    pub fn beat(&self) {
        let ms = self.start.elapsed().as_millis() as u64;
        self.last_beat_ms.store(ms, Ordering::SeqCst);
    }

    pub fn since_beat(&self) -> Duration {
        let now = self.start.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_beat_ms.load(Ordering::SeqCst)))
    }

    /// Whether the watchdog has ever had to intervene. Worth surfacing: a trip means
    /// the daemon stopped working while holding the fan, which is a bug even though
    /// the machine was protected from it.
    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    /// One check. Returns true if it intervened.
    ///
    /// Separated from the thread loop so it can be tested without sleeping through
    /// real time.
    pub fn check(&self) -> bool {
        if self.since_beat() <= self.timeout {
            return false;
        }
        // Stale heartbeat alone is not enough: if the EC already owns the fan there is
        // nothing at risk, and a wedged daemon that never took the fan is not this
        // module's problem.
        if self.lease.mode() != Some(FanMode::Manual) {
            return false;
        }

        let first = !self.tripped.swap(true, Ordering::SeqCst);
        if first {
            eprintln!(
                "WATCHDOG: no heartbeat for {:.1}s while holding the fan; \
                 returning it to the EC",
                self.since_beat().as_secs_f64()
            );
        }
        if self.lease.release_now() {
            eprintln!("WATCHDOG: fan returned to EC control");
        } else {
            // Deliberately not a one-shot. If the write failed the fan is still held,
            // so the next tick finds it manual again and tries once more.
            eprintln!("WATCHDOG: could not return the fan to EC control; will retry");
        }
        true
    }

    /// Start the watchdog thread. It runs for the life of the process.
    pub fn spawn(self: &Arc<Self>) {
        let wd = Arc::clone(self);
        let started = std::thread::Builder::new()
            .name("fan-watchdog".into())
            .spawn(move || loop {
                std::thread::sleep(CHECK_INTERVAL);
                wd.check();
            });
        match started {
            Ok(_) => eprintln!(
                "fan watchdog running: releases the fan after {}s without a heartbeat",
                TIMEOUT.as_secs()
            ),
            // Refuse to pretend. Without this thread there is no protection against a
            // wedged daemon, and the operator should know that before relying on it.
            Err(e) => eprintln!("WARNING: could not start the fan watchdog ({e}); a wedged daemon would leave the fan held"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fw_helper_core::Sysfs;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU32;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn fixture(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helperd-wd-{}-{}-{}",
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
        root
    }

    fn enable(root: &Path) -> String {
        fs::read_to_string(root.join("sys/class/hwmon/hwmon7/pwm1_enable"))
            .unwrap()
            .trim()
            .into()
    }

    fn watchdog(root: &Path) -> (Arc<FanLease>, Arc<Watchdog>) {
        let lease = Arc::new(FanLease::new(Sysfs::new(root)));
        let wd = Watchdog::with_timeout(Arc::clone(&lease), Duration::from_millis(50));
        (lease, wd)
    }

    #[test]
    fn does_not_trip_while_the_heartbeat_is_fresh() {
        let root = fixture("fresh");
        let (lease, wd) = watchdog(&root);
        lease.set_duty(200, Some(40.0)).unwrap();

        wd.beat();
        assert!(!wd.check());
        assert_eq!(enable(&root), "1", "the fan should still be ours");
        assert!(!wd.tripped());
    }

    #[test]
    fn releases_the_fan_when_the_heartbeat_stops() {
        let root = fixture("stale");
        let (lease, wd) = watchdog(&root);
        lease.set_duty(200, Some(40.0)).unwrap();
        wd.beat();

        std::thread::sleep(Duration::from_millis(80));

        assert!(wd.check(), "should have intervened");
        assert_eq!(enable(&root), "2");
        assert!(wd.tripped());
    }

    #[test]
    fn ignores_a_stale_heartbeat_when_the_ec_already_owns_the_fan() {
        // A wedged daemon that never took the fan is not this module's problem, and
        // writing to hardware anyway would be noise on every stalled tick.
        let root = fixture("not-ours");
        let (_lease, wd) = watchdog(&root);
        wd.beat();
        std::thread::sleep(Duration::from_millis(80));

        assert!(!wd.check());
        assert!(!wd.tripped());
    }

    #[test]
    fn keeps_trying_while_the_fan_is_still_held() {
        // The trip condition is read from hardware, so a release that did not take
        // is retried rather than forgotten. Simulated by putting the fan back under
        // manual control behind the lease's back.
        let root = fixture("retry");
        let (lease, wd) = watchdog(&root);
        lease.set_duty(200, Some(40.0)).unwrap();
        wd.beat();
        std::thread::sleep(Duration::from_millis(80));

        assert!(wd.check());
        assert_eq!(enable(&root), "2");

        fs::write(root.join("sys/class/hwmon/hwmon7/pwm1_enable"), "1\n").unwrap();
        assert!(
            wd.check(),
            "a fan found manual again must be released again"
        );
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn a_beat_after_a_trip_stops_further_intervention() {
        let root = fixture("recover");
        let (lease, wd) = watchdog(&root);
        lease.set_duty(200, Some(40.0)).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert!(wd.check());

        // Daemon recovers and takes the fan again.
        lease.set_duty(200, Some(40.0)).unwrap();
        wd.beat();
        assert!(!wd.check());
        assert_eq!(enable(&root), "1");
        // The trip is still recorded: it happened, and that is worth knowing.
        assert!(wd.tripped());
    }
}
