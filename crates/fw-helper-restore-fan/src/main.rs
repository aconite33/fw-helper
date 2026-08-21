//! `fw-helper-restore-fan` — give the fan back to the EC, and say whether it took.
//!
//! This exists for the case the daemon cannot handle itself: it was `SIGKILL`ed, it
//! deadlocked, it panicked inside its own panic hook. systemd runs it as
//! `ExecStopPost=`, so it fires on *every* stop of the unit including the crash
//! paths, and it is safe to run by hand at any time (ADR 0006 point 1).
//!
//! Writing `2` to `pwm1_enable` when the fan is already in EC control is a no-op, so
//! there is nothing to detect and no reason to be clever. Being boring is the point:
//! the machine may be hot and unattended when this runs.

use fw_helper_core::{FanControl, FanMode, Sysfs};
use std::process::ExitCode;

fn main() -> ExitCode {
    let fs = Sysfs::default();

    let fan = match FanControl::probe(&fs) {
        Ok(f) => f,
        Err(e) => {
            // No fan control on this machine means nothing to restore. Say so and
            // succeed — as ExecStopPost, failing here would mark every clean stop
            // of the unit as failed on hardware that never had a fan to take.
            eprintln!("fw-helper-restore-fan: nothing to restore ({e})");
            return ExitCode::SUCCESS;
        }
    };

    let was = fan.mode().unwrap_or(FanMode::Other(0));

    if fan.release_best_effort() {
        match was {
            FanMode::Auto => eprintln!("fw-helper-restore-fan: fan was already EC automatic"),
            other => eprintln!("fw-helper-restore-fan: fan was {other}, now EC automatic"),
        }
        ExitCode::SUCCESS
    } else {
        // The one genuinely bad outcome, and the reason this prints to stderr rather
        // than exiting quietly: the fan may be held at a fixed duty with nothing
        // refreshing it, and nobody is going to notice a silent failure here.
        eprintln!(
            "fw-helper-restore-fan: FAILED to restore EC fan control (fan reports {}). \
             Are you root? Write 2 to the cros_ec hwmon's pwm1_enable by hand.",
            fan.mode().unwrap_or(FanMode::Other(0))
        );
        ExitCode::FAILURE
    }
}
