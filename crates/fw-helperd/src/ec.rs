//! Raw EC commands over `/dev/cros_ec`.
//!
//! ADR 0004 puts kernel sysfs first and raw EC commands second, and this is the first
//! thing to reach the second tier: the charge limit has no working sysfs interface on
//! this board (ADR 0012). Nothing else should follow it here without the same evidence.
//!
//! The wire format lives in `fw_helper_core::ec` so it can be tested without hardware.
//! What is here is only the syscall, which needs libc and so cannot live in core
//! (ADR 0010).

use fw_helper_core::charge::{MAX_LIMIT, MIN_LIMIT};
use fw_helper_core::ec::{self, ChargeLimits};
use fw_helper_core::fan::MIN_TAKEOVER_DUTY;
use fw_helper_core::{Cap, FanBackend, FanError, FanMode, Sysfs};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const DEVICE: &str = "/dev/cros_ec";

/// Bytes before the payload in `struct cros_ec_command_v2`: five `u32`s.
const HEADER_LEN: usize = 20;

/// `_IOWR(CROS_EC_DEV_IOC_V2, 0, struct cros_ec_command_v2)`.
///
/// Built rather than pasted so the derivation is visible, and pinned by a test to the
/// value observed on hardware.
const fn iowr(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    // _IOC(dir=3 for READ|WRITE, type, nr, size)
    (3 << 30)
        | ((size as libc::c_ulong) << 16)
        | ((ty as libc::c_ulong) << 8)
        | (nr as libc::c_ulong)
}
const CROS_EC_DEV_IOCXCMD_V2: libc::c_ulong = iowr(0xEC, 0, HEADER_LEN);

#[derive(Debug)]
pub enum EcError {
    /// The device node is missing, or we are not root.
    Open(io::Error),
    Ioctl(io::Error),
    /// The EC answered, and said no. Distinct from a transport failure: it means the
    /// command reached firmware that declined it, e.g. an unimplemented command.
    Rejected(u32),
    /// Fewer bytes than the command's response is defined to carry. Never guess at a
    /// value here — a wrong charge limit is the failure this whole module exists to fix.
    ShortResponse {
        want: usize,
        got: usize,
    },
}

impl fmt::Display for EcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                write!(f, "{DEVICE} needs root")
            }
            Self::Open(e) if e.kind() == io::ErrorKind::NotFound => {
                write!(f, "no {DEVICE}; is cros_ec_chardev loaded?")
            }
            Self::Open(e) => write!(f, "cannot open {DEVICE}: {e}"),
            Self::Ioctl(e) => write!(f, "EC command failed: {e}"),
            Self::Rejected(code) => {
                write!(f, "the EC rejected the command (result {code})")
            }
            Self::ShortResponse { want, got } => {
                write!(f, "the EC returned {got} bytes, expected {want}")
            }
        }
    }
}

impl std::error::Error for EcError {}

/// Something that can carry a command to the EC. A trait so the charge-limit logic
/// above it is testable without a device node — the fixture pattern of ADR 0004,
/// applied to an interface sysfs cannot represent.
pub trait EcTransport: Send + Sync {
    fn command(
        &self,
        command: u32,
        version: u32,
        out: &[u8],
        insize: usize,
    ) -> Result<Vec<u8>, EcError>;
}

pub struct CrosEc {
    path: PathBuf,
}

impl Default for CrosEc {
    fn default() -> Self {
        Self::new(DEVICE)
    }
}

impl CrosEc {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    fn open(&self) -> Result<File, EcError> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(EcError::Open)
    }
}

impl EcTransport for CrosEc {
    fn command(
        &self,
        command: u32,
        version: u32,
        out: &[u8],
        insize: usize,
    ) -> Result<Vec<u8>, EcError> {
        let file = self.open()?;

        // One buffer serves as both request and response: the kernel writes the reply
        // back over the payload area in place.
        let payload = out.len().max(insize);
        let mut buf = vec![0u8; HEADER_LEN + payload];
        buf[0..4].copy_from_slice(&version.to_ne_bytes());
        buf[4..8].copy_from_slice(&command.to_ne_bytes());
        buf[8..12].copy_from_slice(&(out.len() as u32).to_ne_bytes());
        buf[12..16].copy_from_slice(&(insize as u32).to_ne_bytes());
        // buf[16..20] is `result`, which the EC fills in.
        buf[HEADER_LEN..HEADER_LEN + out.len()].copy_from_slice(out);

        // SAFETY: buf is at least HEADER_LEN + max(outsize, insize) bytes, which is what
        // the ioctl reads and writes; the fd is open for read/write and outlives the call.
        let rc = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                CROS_EC_DEV_IOCXCMD_V2,
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if rc < 0 {
            return Err(EcError::Ioctl(io::Error::last_os_error()));
        }

        let result = u32::from_ne_bytes(buf[16..20].try_into().expect("4 bytes"));
        if result != 0 {
            return Err(EcError::Rejected(result));
        }

        let got = (rc as usize).min(payload);
        if got < insize {
            return Err(EcError::ShortResponse { want: insize, got });
        }
        Ok(buf[HEADER_LEN..HEADER_LEN + insize].to_vec())
    }
}

/// The battery charge limit, as Framework's EC actually governs it.
pub struct EcChargeLimit<'a, T: EcTransport + ?Sized> {
    ec: &'a T,
}

impl<'a, T: EcTransport + ?Sized> EcChargeLimit<'a, T> {
    pub fn new(ec: &'a T) -> Self {
        Self { ec }
    }

    pub fn get(&self) -> Result<ChargeLimits, EcError> {
        let resp = self.ec.command(
            ec::CHARGE_LIMIT_CONTROL,
            0,
            &ec::get_request(),
            ec::GET_RESPONSE_LEN,
        )?;
        ec::parse_limits(&resp).ok_or(EcError::ShortResponse {
            want: ec::GET_RESPONSE_LEN,
            got: resp.len(),
        })
    }

    /// Set the maximum, preserving whatever minimum the EC already holds.
    ///
    /// Read-modify-write rather than assuming a minimum of 0: the minimum is the EC's
    /// own discharge floor and is not ours to reset as a side effect of setting a
    /// ceiling. This mirrors what `framework_tool --charge-limit` does.
    ///
    /// The read-back at the end is a genuine check here, unlike the sysfs one it
    /// replaces: it reads the value out of the mechanism that governs charging, not out
    /// of a parallel one that firmware ignores. It still is not proof that charging
    /// *stops* — only watching `charge_now` across the threshold is that (ADR 0012).
    pub fn set_max(&self, percent: u8) -> Result<ChargeLimits, EcError> {
        let current = self.get()?;
        let want = ChargeLimits {
            min: current.min,
            max: percent,
        };
        // Set returns no payload; insize 0.
        self.ec
            .command(ec::CHARGE_LIMIT_CONTROL, 0, &ec::set_request(want), 0)?;
        self.get()
    }
}

/// What can go wrong applying a charge limit, in the order ADR 0008 established and
/// ADR 0012 keeps: range is checked before support, so a typo reports as a typo even
/// on a machine that could not apply it anyway.
#[derive(Debug)]
pub enum ChargeLimitError {
    OutOfRange(u8),
    Ec(EcError),
    /// The EC took the command and still reports something else. Unlike the sysfs
    /// read-back this replaces, this one can actually fire: it reads the value back
    /// out of the mechanism that governs charging.
    NotApplied {
        requested: u8,
        observed: u8,
    },
}

impl fmt::Display for ChargeLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange(v) => write!(
                f,
                "{v}% is outside the accepted range {MIN_LIMIT}\u{2013}{MAX_LIMIT}%"
            ),
            Self::Ec(e) => write!(f, "{e}"),
            Self::NotApplied {
                requested,
                observed,
            } => write!(
                f,
                "asked the EC for {requested}% and it reports {observed}%"
            ),
        }
    }
}

impl std::error::Error for ChargeLimitError {}

/// Read the limit that actually governs charging on this board.
pub fn read_charge_limit(ec: &dyn EcTransport) -> Result<u8, EcError> {
    EcChargeLimit::new(ec).get().map(|l| l.max)
}

/// Set the limit, verify it through the same mechanism, and mirror it into sysfs.
///
/// The mirror is deliberately last and deliberately best-effort. `charge_control_end_threshold`
/// does not govern anything here — that is the whole finding of ADR 0012 — but UPower
/// and GNOME read it, and leaving it disagreeing with the EC would put a second wrong
/// number in front of the user. It is written to match reality, never consulted as
/// evidence of it.
pub fn set_charge_limit(
    fs: &Sysfs,
    ec: &dyn EcTransport,
    percent: u8,
) -> Result<(), ChargeLimitError> {
    if !(MIN_LIMIT..=MAX_LIMIT).contains(&percent) {
        return Err(ChargeLimitError::OutOfRange(percent));
    }
    let observed = EcChargeLimit::new(ec)
        .set_max(percent)
        .map_err(ChargeLimitError::Ec)?;
    if observed.max != percent {
        return Err(ChargeLimitError::NotApplied {
            requested: percent,
            observed: observed.max,
        });
    }
    let _ = fs.write_string(
        &format!(
            "{}/charge_control_end_threshold",
            fw_helper_core::paths::BATTERY
        ),
        &percent.to_string(),
    );
    Ok(())
}

/// Whether this machine can have its charge limit set, asked of the EC itself.
///
/// Core's `Capabilities::probe` cannot answer this: it is sysfs-only by design
/// (ADR 0010), and on this board sysfs is exactly the interface that lies. The daemon
/// therefore replaces core's verdict with this one.
pub fn charge_capability(ec: &dyn EcTransport) -> Cap {
    match EcChargeLimit::new(ec).get() {
        Ok(_) => Cap::Yes,
        Err(e) => Cap::No(format!("{e}")),
    }
}

/// Fan control over raw EC commands, for boards whose `cros_ec` hwmon has no `pwm1`
/// (ADR 0013).
///
/// Built fresh for every operation, like its sysfs counterpart: the release paths have
/// to work when in-process state is exactly what cannot be trusted, so nothing here
/// caches a file handle or a verdict. The one piece of state - whether we believe we
/// hold the fan - lives in the caller's `AtomicBool` so it survives that rebuilding,
/// and is read without a lock so the panic path can use it.
pub struct EcFan<'a> {
    ec: &'a dyn EcTransport,
    fs: &'a Sysfs,
    manual: &'a AtomicBool,
}

impl<'a> EcFan<'a> {
    pub fn new(ec: &'a dyn EcTransport, fs: &'a Sysfs, manual: &'a AtomicBool) -> Self {
        Self { ec, fs, manual }
    }

    fn hwmon(&self) -> Option<String> {
        self.fs.find_hwmon(fw_helper_core::paths::EC_HWMON_NAME)
    }

    /// Send one duty, returning what the EC holds afterwards.
    fn send(&self, duty: u8) -> Result<u8, FanError> {
        if duty > 0 && duty < MIN_TAKEOVER_DUTY {
            return Err(FanError::DutyCannotTurnFan(duty));
        }
        let percent = ec::fan::duty_to_percent(duty);
        self.ec
            .command(
                ec::fan::SET_FAN_DUTY,
                0,
                &ec::fan::set_duty_request(percent),
                0,
            )
            .map_err(|e| FanError::Ec(e.to_string()))?;
        Ok(ec::fan::percent_to_duty(percent))
    }

    /// `send`, releasing the fan if it fails. The takeover has already happened by the
    /// time a duty write can fail, so an error must not leave us holding the fan.
    fn send_guarded(&self, duty: u8) -> Result<u8, FanError> {
        match self.send(duty) {
            Ok(settled) => Ok(settled),
            Err(e) => {
                if !FanBackend::release_best_effort(self) {
                    return Err(FanError::EcNotReleased(format!(
                        "after a failed duty write: {e}"
                    )));
                }
                Err(e)
            }
        }
    }
}

impl FanBackend for EcFan<'_> {
    fn is_supported(&self) -> bool {
        self.hwmon().is_some()
    }

    /// What we last did, not what the hardware says - there is nothing to read.
    fn mode(&self) -> Result<FanMode, FanError> {
        Ok(if self.manual.load(Ordering::SeqCst) {
            FanMode::Manual
        } else {
            FanMode::Auto
        })
    }

    /// False: see [`FanBackend::mode_is_observable`]. The callers that most need to
    /// know who holds the fan - the watchdog and the startup reclaim - release
    /// unconditionally on this backend instead of asking (ADR 0013, point 2).
    fn mode_is_observable(&self) -> bool {
        false
    }

    /// `None`: these boards report no duty at all. Not zero - a caller that reads this
    /// as an idle fan stops enforcing the floor.
    fn duty(&self) -> Result<Option<u8>, FanError> {
        Ok(None)
    }

    fn rpm(&self) -> Option<u64> {
        let hwmon = self.hwmon()?;
        self.fs.read_u64(&format!("{hwmon}/fan1_input")).ok()
    }

    /// There is no separate mode switch here: the duty command itself takes the fan
    /// from firmware. So the belief is recorded *before* sending, not after - if the
    /// process dies between the two, a release path that thinks it holds nothing still
    /// releases, because releasing is unconditional on this backend anyway, but the
    /// shutdown log will not claim the fan was never taken.
    fn take_manual(&self, duty: u8) -> Result<u8, FanError> {
        if duty > 0 && duty < MIN_TAKEOVER_DUTY {
            return Err(FanError::DutyCannotTurnFan(duty));
        }
        if !self.is_supported() {
            return Err(FanError::Unsupported);
        }
        self.manual.store(true, Ordering::SeqCst);
        self.send_guarded(duty)
    }

    fn set_duty(&self, duty: u8) -> Result<u8, FanError> {
        if !self.manual.load(Ordering::SeqCst) {
            // Same refusal as the sysfs path. On this EC the duty command would in fact
            // take the fan, so allowing it here would turn "change the duty" into an
            // unannounced takeover.
            return Err(FanError::NotUnderManualControl(FanMode::Auto));
        }
        self.send_guarded(duty)
    }

    /// Confirmed only as far as this board allows: the EC accepted the command. There is
    /// no mode register to read back, so a release the EC acknowledges but does not act
    /// on would go unnoticed - one of the three guarantees ADR 0013 records as weaker.
    fn release(&self) -> Result<(), FanError> {
        self.ec
            .command(ec::fan::AUTO_FAN_CTRL, 0, &ec::fan::auto_request(), 0)
            .map_err(|e| FanError::EcNotReleased(e.to_string()))?;
        self.manual.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn release_best_effort(&self) -> bool {
        FanBackend::release(self).is_ok()
    }
}

/// Whether fan control over EC commands can be offered, asked of the EC itself.
///
/// Called only for boards whose sysfs has no `pwm1`; core's sysfs-only probe has already
/// said no there, and this replaces that verdict the same way `charge_capability` does.
/// Two conditions, and the reasons say which failed, because they are fixed differently:
/// the EC has to advertise fan PWM, and the board's fan has to have been measured.
pub fn fan_capability(ec: &dyn EcTransport, board: fw_helper_core::board::Board) -> Cap {
    let answer = match ec.command(
        ec::features::GET_FEATURES,
        0,
        &[],
        ec::features::RESPONSE_LEN,
    ) {
        Ok(answer) => answer,
        Err(e) => return Cap::No(format!("cannot ask the EC for its features: {e}")),
    };
    match ec::features::has(&answer, ec::features::PWM_FAN) {
        Some(true) => {}
        Some(false) => return Cap::No("the EC does not advertise fan PWM control".into()),
        None => return Cap::No("the EC's feature answer was too short to read".into()),
    }
    if board.profile().is_none() {
        return Cap::No(format!(
            "{board}: the EC can drive the fan, but this board's fan is unmeasured, so \
             no firmware floor can be trusted; see scripts/probe-fan-amd.c"
        ));
    }
    Cap::Yes
}

/// A transport that answers from memory, for tests in this crate.
///
/// Exists because the charge limit no longer has a sysfs path a fixture tree can
/// stand in for — ADR 0004's rooted-filesystem trick cannot represent an ioctl, so the
/// seam moves to the trait.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    pub(crate) struct FakeEc {
        pub sent: Mutex<Vec<(u32, Vec<u8>, usize)>>,
        limits: Mutex<ChargeLimits>,
        alive: bool,
        /// The fan as the EC holds it: `None` while firmware owns it, otherwise the
        /// percent last commanded.
        fan: Mutex<Option<u8>>,
        /// Refuse releases, to exercise the path where nothing is managing the fan.
        pub refuse_release: std::sync::atomic::AtomicBool,
    }

    impl FakeEc {
        pub(crate) fn new(min: u8, max: u8) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                limits: Mutex::new(ChargeLimits { min, max }),
                alive: true,
                fan: Mutex::new(None),
                refuse_release: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// The percent the fan is held at, or `None` if firmware owns it.
        pub(crate) fn fan_percent(&self) -> Option<u8> {
            *self.fan.lock().unwrap()
        }

        /// Leave the fan held, as a process that died without releasing it would.
        pub(crate) fn hold_fan(&self, percent: u8) {
            *self.fan.lock().unwrap() = Some(percent);
        }

        /// An EC that refuses every command — a board without the custom command.
        pub(crate) fn dead() -> Self {
            Self {
                alive: false,
                ..Self::new(0, 100)
            }
        }

        pub(crate) fn max(&self) -> u8 {
            self.limits.lock().unwrap().max
        }
    }

    impl EcTransport for FakeEc {
        fn command(
            &self,
            command: u32,
            _version: u32,
            out: &[u8],
            insize: usize,
        ) -> Result<Vec<u8>, EcError> {
            if !self.alive {
                return Err(EcError::Rejected(1));
            }
            self.sent
                .lock()
                .unwrap()
                .push((command, out.to_vec(), insize));
            // Dispatch on the command first. The fan's release carries no payload at
            // all, so matching on a payload byte would index past the end of it.
            if command == ec::fan::SET_FAN_DUTY {
                let bytes: [u8; 4] = out.try_into().map_err(|_| EcError::Rejected(3))?;
                let percent = u32::from_le_bytes(bytes);
                if percent > u32::from(ec::fan::MAX_DUTY) {
                    return Err(EcError::Rejected(3)); // EC_RES_INVALID_PARAM
                }
                *self.fan.lock().unwrap() = Some(percent as u8);
                return Ok(Vec::new());
            }
            if command == ec::fan::AUTO_FAN_CTRL {
                if self
                    .refuse_release
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(EcError::Rejected(1));
                }
                *self.fan.lock().unwrap() = None;
                return Ok(Vec::new());
            }
            if command == ec::features::GET_FEATURES {
                // As measured on FRANMGCP05: PWM_FAN set, LIMITED clear.
                let mut answer = 0x0207_E6AE_u32.to_le_bytes().to_vec();
                answer.extend_from_slice(&0x0000_0207_u32.to_le_bytes());
                return Ok(answer);
            }
            if command != ec::CHARGE_LIMIT_CONTROL {
                return Err(EcError::Rejected(1)); // EC_RES_INVALID_COMMAND
            }
            match out[0] {
                x if x == ec::mode::GET => {
                    let l = *self.limits.lock().unwrap();
                    Ok(vec![l.max, l.min])
                }
                x if x == ec::mode::SET => {
                    *self.limits.lock().unwrap() = ChargeLimits {
                        max: out[1],
                        min: out[2],
                    };
                    Ok(Vec::new())
                }
                _ => Err(EcError::Rejected(1)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeEc;
    use super::*;

    /// A sysfs root with a cros_ec hwmon node, as the AMD boards present it: fan speed
    /// and target, and deliberately no pwm1 or pwm1_enable.
    fn amd_hwmon(tag: &str, rpm: u64) -> (std::path::PathBuf, Sysfs) {
        let root =
            std::env::temp_dir().join(format!("fw-helperd-ecfan-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let hw = root.join("sys/class/hwmon/hwmon8");
        std::fs::create_dir_all(&hw).unwrap();
        std::fs::write(hw.join("name"), "cros_ec\n").unwrap();
        std::fs::write(hw.join("fan1_input"), format!("{rpm}\n")).unwrap();
        std::fs::write(hw.join("fan1_target"), "0\n").unwrap();
        let fs = Sysfs::new(&root);
        (root, fs)
    }

    #[test]
    fn fan_control_is_offered_only_on_a_measured_board_whose_ec_can_do_it() {
        use fw_helper_core::board::{identify_name, Board};
        let ec = FakeEc::new(0, 100);
        assert_eq!(fan_capability(&ec, identify_name("FRANMGCP09")), Cap::Yes);

        match fan_capability(&ec, Board::Unknown) {
            Cap::No(reason) => assert!(reason.contains("unmeasured"), "{reason}"),
            Cap::Yes => panic!("an unmeasured board must not get fan control"),
        }
        match fan_capability(&FakeEc::dead(), identify_name("FRANMGCP09")) {
            Cap::No(reason) => assert!(reason.contains("features"), "{reason}"),
            Cap::Yes => panic!("an EC that will not answer must not get fan control"),
        }
    }

    #[test]
    fn taking_the_fan_sends_the_duty_as_a_percent() {
        let (_root, fs) = amd_hwmon("take", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        let settled = fan.take_manual(255).unwrap();
        assert_eq!(ec.fan_percent(), Some(100), "255 must reach the EC as 100%");
        assert_eq!(settled, 255);
        assert!(manual.load(Ordering::SeqCst));
        assert_eq!(fan.mode().unwrap(), FanMode::Manual);
    }

    #[test]
    fn a_duty_that_cannot_start_the_fan_never_reaches_the_ec() {
        // Measured on both AMD boards: 10% sustains rotation but will not start the
        // fan from rest. A command that the EC would accept and the fan would ignore is
        // a control that lies, so it is refused before anything is sent.
        let (_root, fs) = amd_hwmon("stiction", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        assert!(matches!(
            fan.take_manual(MIN_TAKEOVER_DUTY - 1),
            Err(FanError::DutyCannotTurnFan(_))
        ));
        assert!(ec.sent.lock().unwrap().is_empty());
        assert!(
            !manual.load(Ordering::SeqCst),
            "refusal must not record a takeover"
        );
    }

    #[test]
    fn changing_duty_without_holding_the_fan_is_refused() {
        // On this EC the duty command would take the fan itself, so allowing it would
        // turn "change the duty" into an unannounced takeover.
        let (_root, fs) = amd_hwmon("notheld", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        assert!(matches!(
            fan.set_duty(128),
            Err(FanError::NotUnderManualControl(_))
        ));
        assert_eq!(ec.fan_percent(), None);
    }

    #[test]
    fn release_hands_the_fan_back_and_clears_the_belief() {
        let (_root, fs) = amd_hwmon("release", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        fan.take_manual(128).unwrap();
        fan.release().unwrap();
        assert_eq!(ec.fan_percent(), None);
        assert_eq!(fan.mode().unwrap(), FanMode::Auto);
        // Releasing sends an empty payload - v0 of the command takes none.
        let sent = ec.sent.lock().unwrap();
        let last = sent.last().unwrap();
        assert_eq!(last.0, ec::fan::AUTO_FAN_CTRL);
        assert!(last.1.is_empty());
    }

    #[test]
    fn release_is_safe_to_repeat() {
        // Every release path releases unconditionally on this backend, so repeating it
        // must be harmless - including when we never held the fan at all.
        let (_root, fs) = amd_hwmon("repeat", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        assert!(fan.release_best_effort());
        assert!(fan.release_best_effort());
        assert_eq!(ec.fan_percent(), None);
    }

    #[test]
    fn a_refused_release_is_loud_and_keeps_the_belief() {
        // The one failure with no clean recovery. The error must say what to run, and
        // the belief must stay "manual" - nothing has shown the fan is safe.
        let (_root, fs) = amd_hwmon("refused", 0);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        fan.take_manual(128).unwrap();
        ec.refuse_release.store(true, Ordering::SeqCst);
        let err = fan.release().unwrap_err();
        assert!(matches!(err, FanError::EcNotReleased(_)));
        assert!(err.to_string().contains("fw-helper-restore-fan"), "{err}");
        assert!(manual.load(Ordering::SeqCst));
        assert!(!fan.release_best_effort());
    }

    #[test]
    fn this_backend_admits_what_it_cannot_see() {
        // ADR 0013: no duty register and no mode register. Both have to be visible in
        // the contract, or callers treat "unknown" as "zero" and "belief" as "fact".
        let (_root, fs) = amd_hwmon("admits", 3210);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);

        assert_eq!(fan.duty().unwrap(), None);
        assert!(!fan.mode_is_observable());
        assert_eq!(fan.rpm(), Some(3210), "RPM is the one feedback there is");
    }

    #[test]
    fn a_board_without_the_hwmon_node_is_unsupported() {
        let root =
            std::env::temp_dir().join(format!("fw-helperd-ecfan-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let fs = Sysfs::new(&root);
        let ec = FakeEc::new(0, 100);
        let manual = AtomicBool::new(false);
        let fan = EcFan::new(&ec, &fs, &manual);
        assert!(matches!(fan.take_manual(128), Err(FanError::Unsupported)));
        assert!(!manual.load(Ordering::SeqCst));
    }

    #[test]
    fn ioctl_number_matches_what_the_hardware_probe_used() {
        // 0xC014EC00 was printed by the C probe that returned max=100 min=0 on the
        // target machine, so this is pinned to a value known to work rather than to a
        // re-derivation of the same macro.
        assert_eq!(CROS_EC_DEV_IOCXCMD_V2, 0xC014_EC00);
    }

    #[test]
    fn reads_the_limits_the_ec_holds() {
        // The state found on hardware: a 100% ceiling, while sysfs claimed 80.
        let ec = FakeEc::new(0, 100);
        let limits = EcChargeLimit::new(&ec).get().unwrap();
        assert_eq!(limits, ChargeLimits { min: 0, max: 100 });
    }

    #[test]
    fn setting_a_maximum_preserves_the_existing_minimum() {
        // The EC's minimum is its discharge floor. Setting a ceiling must not quietly
        // reset it, which is why this is a read-modify-write and not a blind write.
        let ec = FakeEc::new(20, 100);
        let got = EcChargeLimit::new(&ec).set_max(80).unwrap();
        assert_eq!(got, ChargeLimits { min: 20, max: 80 });
    }

    #[test]
    fn set_sends_the_command_the_ec_expects() {
        let ec = FakeEc::new(0, 100);
        EcChargeLimit::new(&ec).set_max(80).unwrap();
        let sent = ec.sent.lock().unwrap();
        // get, set, get
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[1].0, 0x3E03);
        assert_eq!(sent[1].1, vec![ec::mode::SET, 80, 0]);
        // A Set returns nothing; asking for a response would leave us waiting on bytes
        // the EC never sends.
        assert_eq!(sent[1].2, 0);
    }

    #[test]
    fn a_rejected_command_is_an_error_not_a_limit() {
        struct Dead;
        impl EcTransport for Dead {
            fn command(&self, _: u32, _: u32, _: &[u8], _: usize) -> Result<Vec<u8>, EcError> {
                Err(EcError::Rejected(3))
            }
        }
        assert!(EcChargeLimit::new(&Dead).get().is_err());
    }
}
