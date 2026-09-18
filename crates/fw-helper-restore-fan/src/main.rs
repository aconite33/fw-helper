//! `fw-helper-restore-fan` — give the fan back to the EC, and say whether it took.
//!
//! This exists for the case the daemon cannot handle itself: it was `SIGKILL`ed, it
//! deadlocked, it panicked inside its own panic hook. systemd runs it as
//! `ExecStopPost=`, so it fires on *every* stop of the unit including the crash
//! paths, and it is safe to run by hand at any time (ADR 0006 point 1).
//!
//! Two boards, two mechanisms. Where `cros_ec` hwmon has `pwm1_enable` (the Intel
//! board), writing `2` hands the fan back and can be read back to confirm it. Where it
//! does not (the AMD boards, ADR 0013), the only way is an EC command - and there is no
//! mode register either, so there is nothing to ask first. Releasing a fan the EC
//! already owns is a no-op on both, so this releases unconditionally and is never
//! clever. The machine may be hot and unattended when it runs.

use fw_helper_core::ec::fan;
use fw_helper_core::{FanControl, FanMode, Sysfs};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::process::ExitCode;

const DEVICE: &str = "/dev/cros_ec";

/// Header of `struct cros_ec_command_v2`: `version, command, outsize, insize, result`,
/// five `u32`s. The release carries no payload and expects none back, so the header is
/// the whole command.
const HEADER_LEN: usize = 20;

/// `_IOWR(0xEC, 0, struct cros_ec_command_v2)`, with a 20-byte header.
///
/// The same value the daemon pins in its own test against the one the hardware probe
/// used, derived again here rather than shared: sharing it would mean depending on the
/// daemon crate.
const CROS_EC_DEV_IOCXCMD_V2: libc::c_ulong =
    (3 << 30) | ((HEADER_LEN as libc::c_ulong) << 16) | (0xEC << 8);

/// `EC_RES_INVALID_COMMAND`: this EC does not implement the command.
const EC_RES_INVALID_COMMAND: u32 = 1;

/// The whole release command, on the stack.
fn release_command() -> [u8; HEADER_LEN] {
    let mut buf = [0u8; HEADER_LEN];
    // version 0 at [0..4], outsize 0 at [8..12], insize 0 at [12..16], result at [16..20]
    buf[4..8].copy_from_slice(&fan::AUTO_FAN_CTRL.to_ne_bytes());
    buf
}

enum EcRelease {
    Released,
    /// The EC does not have the command, so it cannot have had a fan taken with it.
    NotSupported,
    Failed(String),
}

fn ec_release() -> EcRelease {
    let file = match OpenOptions::new().read(true).write(true).open(DEVICE) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return EcRelease::Failed(format!("{DEVICE} needs root"))
        }
        Err(e) => return EcRelease::Failed(format!("cannot open {DEVICE}: {e}")),
    };
    let mut buf = release_command();
    // SAFETY: buf is HEADER_LEN bytes, which is all the kernel reads and writes for a
    // command with no payload in either direction; the fd is open read/write and
    // outlives the call.
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            CROS_EC_DEV_IOCXCMD_V2,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return EcRelease::Failed(format!("{}", io::Error::last_os_error()));
    }
    match u32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]) {
        0 => EcRelease::Released,
        EC_RES_INVALID_COMMAND => EcRelease::NotSupported,
        code => EcRelease::Failed(format!("the EC refused it (result {code})")),
    }
}

fn main() -> ExitCode {
    let fs = Sysfs::default();

    let fan = match FanControl::probe(&fs) {
        Ok(f) => f,
        Err(_) => return release_over_ec(&fs),
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

/// The AMD path: no `pwm1_enable`, so the EC is asked directly.
fn release_over_ec(fs: &Sysfs) -> ExitCode {
    // No cros_ec at all means no fan control of any kind. As ExecStopPost, failing here
    // would mark every clean stop of the unit as failed on hardware that never had a fan
    // to take.
    if fs
        .find_hwmon(fw_helper_core::paths::EC_HWMON_NAME)
        .is_none()
        || !std::path::Path::new(DEVICE).exists()
    {
        eprintln!("fw-helper-restore-fan: nothing to restore (no cros_ec fan interface)");
        return ExitCode::SUCCESS;
    }
    match ec_release() {
        // There is no register to say what the fan was doing before, so this cannot
        // report "was manual" the way the sysfs path does. It says what it did.
        EcRelease::Released => {
            eprintln!("fw-helper-restore-fan: fan handed to the EC (no mode register to check)");
            ExitCode::SUCCESS
        }
        EcRelease::NotSupported => {
            eprintln!(
                "fw-helper-restore-fan: nothing to restore (this EC has no fan-control command)"
            );
            ExitCode::SUCCESS
        }
        EcRelease::Failed(e) => {
            eprintln!(
                "fw-helper-restore-fan: FAILED to return the fan to the EC ({e}). The fan \
                 may be held with nothing managing it. Run this again as root."
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_number_matches_the_one_the_hardware_probe_used() {
        // 0xC014EC00: printed by the C probe that worked on hardware, and pinned in the
        // daemon too. Re-derived here, so this is where a disagreement would show.
        assert_eq!(CROS_EC_DEV_IOCXCMD_V2, 0xC014_EC00);
    }

    #[test]
    fn the_release_is_the_auto_fan_command_with_no_payload() {
        let cmd = release_command();
        assert_eq!(
            u32::from_ne_bytes(cmd[0..4].try_into().unwrap()),
            0,
            "version"
        );
        assert_eq!(
            u32::from_ne_bytes(cmd[4..8].try_into().unwrap()),
            fan::AUTO_FAN_CTRL
        );
        assert_eq!(
            u32::from_ne_bytes(cmd[8..12].try_into().unwrap()),
            0,
            "outsize"
        );
        assert_eq!(
            u32::from_ne_bytes(cmd[12..16].try_into().unwrap()),
            0,
            "insize"
        );
    }
}
