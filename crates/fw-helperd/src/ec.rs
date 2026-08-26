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
use fw_helper_core::{Cap, Sysfs};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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
    }

    impl FakeEc {
        pub(crate) fn new(min: u8, max: u8) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                limits: Mutex::new(ChargeLimits { min, max }),
                alive: true,
            }
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
