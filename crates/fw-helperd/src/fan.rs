//! Daemon-side ownership of manual fan control.
//!
//! `fw-helper-core` provides the mechanism; this decides *when* the daemon holds the
//! lease and guarantees it lets go. ADR 0006 point 1: `pwm1_enable=2` on clean
//! shutdown, on `SIGTERM`/`SIGINT`, and on panic.
//!
//! It also enforces ADR 0006 point 4: the fan is never run slower than firmware would
//! run it, re-checked on every poll tick rather than only when a duty is requested.
//!
//! Three design choices are about the failure paths rather than the happy one:
//!
//! - **The release path takes no locks.** The panic hook calls `release_now`, and a
//!   panic can happen while another thread holds a lock — a hook that blocked on one
//!   would hang the process with the fan still held. There *is* a mutex here, around
//!   the learned floor, and the rule is that nothing on a shutdown path may touch it.
//!   Everything `release_now` needs is an atomic or a question for the hardware.
//! - **Releases are unconditional.** `release_now` writes `2` whether or not we think
//!   we hold the lease. The flag can be wrong in exactly the situation that matters —
//!   a previous instance killed while holding it — and the write is idempotent and
//!   cheap, so believing our own bookkeeping buys nothing and risks everything.
//! - **Enforcement compares decisions, not observations.** See [`FanLease::applied`]:
//!   judging "are we below the floor" against the hardware's quantized read-back let a
//!   real deficit hide inside the tolerance, which hardware caught and unit tests did
//!   not.

use fw_helper_core::fan::DUTY_TOLERANCE;
use fw_helper_core::{Ceiling, FanControl, FanError, FanMode, FirmwareFloor, Sysfs};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

#[derive(Debug)]
pub enum LeaseError {
    /// No usable temperature. ADR 0006 point 6: no sensor, no manual fan.
    NoTemperature,
    /// Too hot for user configuration to have a say. ADR 0006 point 5.
    AboveCeiling {
        celsius: f64,
        ceiling: Ceiling,
    },
    Fan(FanError),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTemperature => write!(
                f,
                "no readable temperature sensor, so the firmware floor cannot be \
                 computed and manual fan control would be unbounded; leaving the fan \
                 to the EC (ADR 0006 point 6)"
            ),
            Self::AboveCeiling { celsius, ceiling } => write!(
                f,
                "{celsius:.1} C is at or above the {ceiling} ceiling, where the fan \
                 belongs to firmware and configuration does not get a vote \
                 (ADR 0006 point 5). Let the machine cool, then ask again"
            ),
            Self::Fan(e) => write!(f, "{e}"),
        }
    }
}

/// Build the ceiling from a sensor's critical point.
///
/// `FW_HELPERD_DEBUG_CEILING_C` overrides it, and exists for the same reason the
/// watchdog's wedge injection does: the real ceiling sits near 100 °C, which this
/// machine does not reach under any load it is safe to apply deliberately. Without an
/// override the release path could only ever be argued for, not demonstrated. Never
/// set in production; it can only ever *lower* the ceiling, so a stray value makes the
/// daemon more cautious rather than less.
pub fn ceiling_for(crit: Option<f64>) -> Ceiling {
    let real = Ceiling::from_crit(crit);
    let Some(raw) = std::env::var_os("FW_HELPERD_DEBUG_CEILING_C") else {
        return real;
    };
    match raw.to_str().and_then(|s| s.parse::<f64>().ok()) {
        Some(c) if c < real.celsius() => {
            Ceiling::from_crit(Some(c + fw_helper_core::ceiling::CEILING_MARGIN_C))
        }
        _ => real,
    }
}

/// What actually happened when a duty was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// What the caller asked for.
    pub requested: u8,
    /// What we wrote after clamping to the firmware floor.
    pub target: u8,
    /// What the EC settled on. Differs from `target` by a count or two.
    pub settled: u8,
    /// The floor at the temperature used. Surfaced so a clamp can explain itself.
    pub floor: u8,
}

impl Applied {
    pub fn clamped(&self) -> bool {
        self.target > self.requested
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
    /// The duty the user actually asked for, before clamping. Kept because the floor
    /// moves with temperature: when the machine cools, the fan must be allowed back
    /// down to what was requested rather than staying at whatever the hottest moment
    /// demanded.
    requested: AtomicU8,
    /// The duty we last decided to write. Compared exactly against the newly computed
    /// target, so a floor deficit of one or two counts is still a deficit.
    ///
    /// The obvious alternative — compare the *observed* duty against the target and
    /// allow `DUTY_TOLERANCE` of slack — was measured on hardware doing the wrong
    /// thing: that tolerance exists to absorb the EC's percent quantization when
    /// verifying a write, and reusing it here silently permitted running up to three
    /// counts below firmware. On 2026-08-21 that left the fan at duty 84 while the
    /// floor was 87, about 4% slower than the EC would have run, for 35 seconds.
    applied: AtomicU8,
    /// The firmware floor, learned as it goes.
    ///
    /// **The release paths never lock this.** `release_now` runs from the panic hook,
    /// and a lock there could be held by the panicking thread. Everything on the
    /// shutdown path stays lock-free; only the deliberate control paths take this.
    floor: Mutex<FirmwareFloor>,
}

impl FanLease {
    pub fn new(fs: Sysfs) -> Self {
        Self {
            fs,
            held: AtomicBool::new(false),
            requested: AtomicU8::new(0),
            applied: AtomicU8::new(0),
            floor: Mutex::new(FirmwareFloor::new()),
        }
    }

    /// Record what the EC does at a temperature. Call only while the EC owns the fan:
    /// under manual control the RPM is ours, and feeding it back would ratchet the
    /// floor up to whatever we last chose.
    pub fn observe(&self, celsius: f64, rpm: u64) {
        if let Ok(mut floor) = self.floor.lock() {
            floor.observe(celsius, rpm);
        }
    }

    /// The lowest duty permitted at this temperature.
    pub fn floor_duty(&self, celsius: f64) -> u8 {
        self.floor
            .lock()
            .map(|f| f.floor_duty(celsius))
            // A poisoned lock means a thread panicked holding it. Refusing to guess
            // quietly, the answer is the loud one.
            .unwrap_or(u8::MAX)
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
    pub fn set_duty(
        &self,
        duty: u8,
        celsius: Option<f64>,
        ceiling: Ceiling,
    ) -> Result<Applied, LeaseError> {
        let Some(celsius) = celsius.filter(|c| c.is_finite()) else {
            return Err(LeaseError::NoTemperature);
        };
        // Checked before anything else touches hardware: above the ceiling the answer
        // is no, whatever the duty and whoever is asking.
        if ceiling.exceeded_by(celsius) {
            return Err(LeaseError::AboveCeiling { celsius, ceiling });
        }
        let floor = self.floor_duty(celsius);
        let target = duty.max(floor);

        let settled = self.write(target)?;
        // Store the *unclamped* request: it is what the user wants, and it is what the
        // fan should return to once the machine cools.
        self.requested.store(duty, Ordering::SeqCst);
        Ok(Applied {
            requested: duty,
            target,
            settled,
            floor,
        })
    }

    /// Re-evaluate the floor and correct the fan if it has drifted from where it
    /// should be. Called every poll tick while we hold the lease.
    ///
    /// **This is what makes the clamp mean anything.** Clamping only at the moment a
    /// duty is requested protects nothing: a duty chosen at idle is perfectly safe
    /// when it is chosen and becomes stuck-low the moment the machine is put under
    /// load. The floor has to be enforced continuously or not at all.
    ///
    /// Corrects downward too. When the machine cools the floor drops, and the fan is
    /// allowed back to what was actually asked for.
    pub fn enforce_floor(&self, celsius: Option<f64>, ceiling: Ceiling) -> Option<Enforced> {
        if !self.held() {
            return None;
        }
        // Losing the sensor while holding the fan is not a reason to keep holding it
        // blind. Firmware has its own sensors; give it back.
        let Some(celsius) = celsius.filter(|c| c.is_finite()) else {
            let released = self.release_now();
            return Some(Enforced::ReleasedNoSensor { released });
        };

        // ADR 0006 point 5. Note this is reached only after the floor has already been
        // demanding full duty for several degrees: releasing hands the fan to a curve
        // that tops out slower than we can drive it, so it is the last resort rather
        // than the next step up.
        if ceiling.exceeded_by(celsius) {
            let released = self.release_now();
            return Some(Enforced::ReleasedTooHot {
                celsius,
                ceiling,
                released,
            });
        }

        let requested = self.requested.load(Ordering::SeqCst);
        let floor = self.floor_duty(celsius);
        let target = requested.max(floor);

        let current = self.duty()?;
        // Two separate questions, and conflating them is what caused the defect
        // described on `applied`:
        //
        // 1. Has the *decision* changed? Compared exactly — one count below the floor
        //    is still below the floor.
        // 2. Has something moved the fan out from under us? That comparison is against
        //    what the hardware reports, and only there does quantization slack belong.
        let decision_changed = target != self.applied.load(Ordering::SeqCst);
        let drifted = current.abs_diff(target) > DUTY_TOLERANCE;
        if !decision_changed && !drifted {
            return None;
        }
        match self.write(target) {
            Ok(settled) => Some(Enforced::Corrected {
                from: current,
                to: settled,
                floor,
                celsius,
            }),
            Err(e) => Some(Enforced::Failed(e)),
        }
    }

    /// Take the fan if we do not have it, then write `duty`.
    fn write(&self, duty: u8) -> Result<u8, LeaseError> {
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
        self.applied.store(duty, Ordering::SeqCst);
        Ok(settled)
    }

    /// Hand the fan back and confirm the EC took it.
    pub fn release(&self) -> Result<(), LeaseError> {
        self.requested.store(0, Ordering::SeqCst);
        self.applied.store(0, Ordering::SeqCst);
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

/// Outcome of one floor enforcement pass.
#[derive(Debug)]
pub enum Enforced {
    /// The fan was moved to keep up with (or come back down from) the floor.
    Corrected {
        from: u8,
        to: u8,
        floor: u8,
        celsius: f64,
    },
    /// The temperature became unreadable, so the fan went back to firmware.
    ReleasedNoSensor {
        released: bool,
    },
    /// Too hot for us to be holding the fan at all (ADR 0006 point 5).
    ReleasedTooHot {
        celsius: f64,
        ceiling: Ceiling,
        released: bool,
    },
    Failed(LeaseError),
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

    /// Idle on the reference machine. The EC runs the fan at 0 rpm here, so the floor
    /// is 0 and the user may ask for silence.
    const IDLE_C: f64 = 40.0;
    /// Loaded. Measured EC behaviour is 2925 rpm, which needs duty ~85.
    const HOT_C: f64 = 64.8;
    /// The ceiling this machine actually derives, from peci-temp's 119.85 C crit.
    fn real_ceiling() -> Ceiling {
        Ceiling::from_crit(Some(119.85))
    }

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

    fn lease(root: &Path) -> FanLease {
        FanLease::new(Sysfs::new(root))
    }

    #[test]
    fn refuses_manual_control_without_a_temperature() {
        // ADR 0006 point 6: no sensor, no manual fan. Without a temperature there is
        // no floor, and without a floor the duty is unbounded.
        let root = fixture("no-temp");
        let l = lease(&root);

        assert!(matches!(
            l.set_duty(200, None, real_ceiling()),
            Err(LeaseError::NoTemperature)
        ));
        assert!(matches!(
            l.set_duty(200, Some(f64::NAN), real_ceiling()),
            Err(LeaseError::NoTemperature)
        ));
        assert_eq!(enable(&root), "2", "the EC must still own the fan");
        assert!(!l.held());
    }

    #[test]
    fn a_silent_fan_is_allowed_at_idle() {
        // The whole point of the feature. If this ever fails, the floor has stopped
        // tracking the EC and started inventing policy.
        let root = fixture("silent");
        let l = lease(&root);

        let applied = l.set_duty(0, Some(IDLE_C), real_ceiling()).unwrap();
        assert_eq!(applied.floor, 0);
        assert_eq!(applied.target, 0);
        assert!(!applied.clamped());
    }

    #[test]
    fn a_low_duty_is_raised_when_the_machine_is_hot() {
        let root = fixture("clamp");
        let l = lease(&root);

        let applied = l.set_duty(0, Some(HOT_C), real_ceiling()).unwrap();
        assert!(applied.clamped(), "{applied:?}");
        assert!(applied.target >= 78, "{applied:?}");
        assert_eq!(applied.requested, 0, "the request itself is remembered");
    }

    #[test]
    fn a_duty_safe_at_idle_is_corrected_when_the_machine_heats_up() {
        // The failure the clamp exists to prevent, and the reason enforcement cannot
        // happen only at request time: duty 0 is perfectly safe when it is chosen and
        // is stuck-low a minute later.
        let root = fixture("heats-up");
        let l = lease(&root);

        l.set_duty(0, Some(IDLE_C), real_ceiling()).unwrap();
        assert_eq!(l.duty().unwrap(), 0);

        let outcome = l.enforce_floor(Some(HOT_C), real_ceiling());
        match outcome {
            Some(Enforced::Corrected { from, to, .. }) => {
                assert_eq!(from, 0);
                assert!(to >= 78, "raised only to {to}");
            }
            other => panic!("expected a correction, got {other:?}"),
        }
    }

    #[test]
    fn the_fan_comes_back_down_when_the_machine_cools() {
        // Clamping upward without ever relaxing would leave the fan stuck at whatever
        // the hottest moment demanded, which is a different kind of broken.
        let root = fixture("cools");
        let l = lease(&root);

        l.set_duty(0, Some(HOT_C), real_ceiling()).unwrap();
        let hot_duty = l.duty().unwrap();
        assert!(hot_duty >= 78);

        match l.enforce_floor(Some(IDLE_C), real_ceiling()) {
            Some(Enforced::Corrected { to, .. }) => {
                assert_eq!(to, 0, "should return to the duty actually requested")
            }
            other => panic!("expected a correction, got {other:?}"),
        }
    }

    #[test]
    fn a_steady_temperature_provokes_no_writes() {
        let root = fixture("steady");
        let l = lease(&root);
        l.set_duty(120, Some(IDLE_C), real_ceiling()).unwrap();

        assert!(
            l.enforce_floor(Some(IDLE_C), real_ceiling()).is_none(),
            "nothing changed, so nothing should be written"
        );
    }

    #[test]
    fn losing_the_sensor_hands_the_fan_back() {
        // Firmware has its own sensors. Holding the fan blind is the worst option.
        let root = fixture("blind");
        let l = lease(&root);
        l.set_duty(200, Some(IDLE_C), real_ceiling()).unwrap();

        match l.enforce_floor(None, real_ceiling()) {
            Some(Enforced::ReleasedNoSensor { released }) => assert!(released),
            other => panic!("expected a release, got {other:?}"),
        }
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn enforcement_does_nothing_when_the_ec_owns_the_fan() {
        let root = fixture("not-held");
        let l = lease(&root);
        assert!(l.enforce_floor(Some(HOT_C), real_ceiling()).is_none());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn observation_raises_the_floor_the_daemon_enforces() {
        // The static table has a large gap across the EC's knee. Watching firmware is
        // what closes it.
        let root = fixture("observe");
        let l = lease(&root);
        let before = l.floor_duty(48.0);

        l.observe(48.0, 2500);
        assert!(l.floor_duty(48.0) > before);
    }

    #[test]
    fn refuses_to_take_the_fan_above_the_ceiling() {
        // ADR 0006 point 5: user configuration does not get a vote up here.
        let root = fixture("ceiling-refuse");
        let l = lease(&root);
        let ceiling = real_ceiling();

        let too_hot = ceiling.celsius() + 1.0;
        assert!(matches!(
            l.set_duty(255, Some(too_hot), ceiling),
            Err(LeaseError::AboveCeiling { .. })
        ));
        assert_eq!(enable(&root), "2", "the fan must stay with firmware");
        assert!(!l.held());
    }

    #[test]
    fn gives_the_fan_back_when_the_ceiling_is_reached() {
        let root = fixture("ceiling-release");
        let l = lease(&root);
        let ceiling = real_ceiling();
        l.set_duty(255, Some(IDLE_C), ceiling).unwrap();
        assert_eq!(enable(&root), "1");

        match l.enforce_floor(Some(ceiling.celsius() + 5.0), ceiling) {
            Some(Enforced::ReleasedTooHot { released, .. }) => assert!(released),
            other => panic!("expected a ceiling release, got {other:?}"),
        }
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn the_ceiling_error_tells_the_user_what_to_do() {
        let msg = LeaseError::AboveCeiling {
            celsius: 105.0,
            ceiling: real_ceiling(),
        }
        .to_string();
        assert!(msg.contains("cool"), "got: {msg}");
    }

    #[test]
    fn full_load_temperatures_do_not_trip_the_ceiling() {
        // 76.8 C is the measured steady state under sustained full load. If the
        // ceiling fires there, manual fan control is useless exactly when wanted.
        let root = fixture("ceiling-load");
        let l = lease(&root);
        assert!(l.set_duty(200, Some(76.8), real_ceiling()).is_ok());
    }

    #[test]
    fn takes_the_lease_then_gives_it_back() {
        let root = fixture("lease");
        let l = lease(&root);

        assert_eq!(
            l.set_duty(200, Some(IDLE_C), real_ceiling())
                .unwrap()
                .settled,
            200
        );
        assert!(l.held());
        assert_eq!(enable(&root), "1");

        l.release().unwrap();
        assert!(!l.held());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn release_now_works_whatever_we_believed() {
        let root = fixture("unconditional");
        let l = lease(&root);
        l.set_duty(200, Some(IDLE_C), real_ceiling()).unwrap();

        // Bookkeeping that has gone wrong: the flag says we hold nothing, the
        // hardware says otherwise. The hardware wins.
        l.held.store(false, Ordering::SeqCst);
        assert!(l.release_now());
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn reclaims_a_fan_left_manual_by_a_previous_instance() {
        let root = fixture("reclaim");
        fs::write(root.join("sys/class/hwmon/hwmon7/pwm1_enable"), "1\n").unwrap();

        lease(&root).reclaim_at_startup();
        assert_eq!(enable(&root), "2");
    }

    #[test]
    fn startup_reclaim_leaves_an_untouched_fan_alone() {
        let root = fixture("no-reclaim");
        let l = lease(&root);
        l.reclaim_at_startup();
        assert_eq!(enable(&root), "2");
        assert!(!l.held());
    }

    #[test]
    fn a_dropped_lease_releases() {
        let root = fixture("drop");
        {
            let l = lease(&root);
            l.set_duty(200, Some(IDLE_C), real_ceiling()).unwrap();
            assert_eq!(enable(&root), "1");
        }
        assert_eq!(enable(&root), "2", "Drop must have handed the fan back");
    }
}
