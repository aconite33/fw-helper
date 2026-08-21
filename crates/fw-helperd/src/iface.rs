//! The `org.fwhelper.Daemon1` interface.
//!
//! Properties are read-only and unauthenticated; every method that touches hardware
//! goes through `authorize` first, per action, failing closed (ADR 0003).

use crate::fan::FanLease;
use crate::state::State;
use crate::watchdog::Watchdog;
use crate::{polkit, wire};
use fw_helper_core::{Capabilities, ChargeControl, Sysfs, Telemetry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use zbus::zvariant::OwnedValue;

/// Outcome of [`Daemon::reapply_charge_limit`].
///
/// `AlreadyCorrect` versus `Corrected` is the distinction worth having: it says whether
/// firmware actually reset the threshold, which no amount of unconditional writing can
/// reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reapply {
    /// No limit persisted; nothing to do.
    NothingPersisted,
    /// Hardware already held the persisted value. Firmware left it alone.
    AlreadyCorrect,
    /// Hardware disagreed and has been corrected. Firmware reset it, or something else did.
    Corrected,
    /// The re-apply was attempted and failed. Reason already logged.
    Failed,
}

pub struct Daemon {
    fs: Sysfs,
    caps: Capabilities,
    latest: Telemetry,
    /// Behind a mutex so write methods can take `&self`. A `&mut self` method makes
    /// zbus hold the interface **write** lock for the whole call, and a polkit prompt
    /// can legitimately take tens of seconds — which would stall telemetry for every
    /// client while a password dialog sits on screen.
    state: Mutex<State>,
    /// Shared with `main`, which releases it on every shutdown path. Lock-free by
    /// design so the panic hook can use it (ADR 0006).
    fan: Arc<FanLease>,
    /// Consulted before granting manual fan control, so the fan is never handed to a
    /// daemon that has stopped minding it.
    watchdog: Arc<Watchdog>,
}

impl Daemon {
    pub fn new(
        fs: Sysfs,
        caps: Capabilities,
        state: State,
        fan: Arc<FanLease>,
        watchdog: Arc<Watchdog>,
    ) -> Self {
        Self {
            fs,
            caps,
            latest: Telemetry::default(),
            state: Mutex::new(state),
            fan,
            watchdog,
        }
    }

    /// Authorize a caller for one action, or say why not.
    ///
    /// Every hardware-touching method starts here. Factored out because the sequence
    /// — identify the caller, then check the action, failing closed — must be
    /// identical for each one, and a method that forgets a step is a method that
    /// writes to hardware unauthenticated.
    async fn authorize(
        header: &zbus::message::Header<'_>,
        conn: &zbus::Connection,
        action: &str,
    ) -> zbus::fdo::Result<String> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| zbus::fdo::Error::AuthFailed("no caller identity".into()))?;
        polkit::check(conn, &sender, action)
            .await
            .map_err(zbus::fdo::Error::AuthFailed)?;
        Ok(sender)
    }

    /// Re-apply the persisted charge limit. Called at startup, because
    /// `charge_control_end_threshold` does not survive a reboot, and on resume,
    /// because firmware may reset it.
    ///
    /// Reads before writing, and this is not an optimisation — the write is cheap and
    /// idempotent. It is about what the log can tell us afterwards. Writing
    /// unconditionally emits the same line whether or not firmware disturbed anything,
    /// which is precisely the question the resume hook exists to answer. Separating the
    /// two cases makes every resume a free measurement of whether this hook is
    /// load-bearing on this hardware, rather than an assumption we keep re-asserting.
    ///
    /// Returns what happened, so callers and tests can distinguish the cases without
    /// scraping stderr.
    pub fn reapply_charge_limit(&self) -> Reapply {
        let Some(limit) = self.state.lock().ok().and_then(|s| s.charge_limit) else {
            return Reapply::NothingPersisted;
        };
        let cc = ChargeControl::new(&self.fs);

        match cc.read() {
            Ok(observed) if observed == limit => {
                eprintln!("charge limit still {limit}%; nothing to re-apply");
                return Reapply::AlreadyCorrect;
            }
            Ok(observed) => {
                eprintln!("charge limit is {observed}%, expected {limit}%; re-applying");
            }
            // Fall through to the write deliberately. The usual cause is the attribute
            // being absent, and `set` reports that with the actionable message (ADR 0008)
            // rather than the bare io error we would have to invent here.
            Err(e) => {
                eprintln!("cannot read charge limit ({e}); re-applying {limit}% anyway");
            }
        }

        match cc.set(limit) {
            Ok(()) => {
                eprintln!("re-applied charge limit {limit}%");
                Reapply::Corrected
            }
            Err(e) => {
                eprintln!("could not re-apply charge limit {limit}%: {e}");
                Reapply::Failed
            }
        }
    }

    /// Called by the poll task. Returns true when the published view actually changed,
    /// so we only emit a PropertiesChanged signal when there is something to say.
    pub fn update(&mut self, t: Telemetry) -> bool {
        let changed = self.latest != t;
        self.latest = t;
        changed
    }
}

#[zbus::interface(name = "org.fwhelper.Daemon1")]
impl Daemon {
    /// Per-knob availability. Clients disable controls this reports as unavailable
    /// rather than offering something that silently does nothing.
    #[zbus(property)]
    async fn capabilities(&self) -> HashMap<String, (bool, String)> {
        wire::capabilities_dict(&self.caps)
    }

    /// Live readings. Updated at most once per second and quantized to 0.1 W —
    /// see ADR 0009. There is deliberately no on-demand sampling method: a client
    /// must not be able to drive sampling cadence.
    #[zbus(property)]
    async fn telemetry(&self) -> HashMap<String, OwnedValue> {
        wire::telemetry_dict(&self.latest)
    }

    /// Validated critical thresholds per sensor. Sensors reporting implausible
    /// values are omitted entirely rather than published as-is.
    #[zbus(property)]
    async fn critical_temperatures(&self) -> HashMap<String, f64> {
        wire::critical_temps(&self.latest)
    }

    /// Interface version, so a client can refuse to talk to a daemon it does not
    /// understand. Bumped on breaking changes only.
    #[zbus(property)]
    async fn version(&self) -> u32 {
        1
    }

    /// Current charge limit as the EC reports it, or 0 when unsupported.
    #[zbus(property)]
    async fn charge_limit(&self) -> u8 {
        ChargeControl::new(&self.fs).read().unwrap_or(0)
    }

    /// Set the battery charge limit.
    ///
    /// The first method in this interface that writes to hardware. Two things are
    /// non-negotiable and set the pattern for every write that follows: polkit is
    /// checked before touching anything, and the value is read back afterwards so a
    /// silent override surfaces as an error rather than as success (ADR 0008).
    async fn set_charge_limit(
        &self,
        percent: u8,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender = Self::authorize(&header, conn, polkit::actions::SET_CHARGE_LIMIT).await?;

        ChargeControl::new(&self.fs)
            .set(percent)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        // Lock only to record the result. Never held across an await.
        if let Ok(mut state) = self.state.lock() {
            state.charge_limit = Some(percent);
            state.save();
        }
        eprintln!("charge limit set to {percent}% by {sender}");
        Ok(())
    }

    /// How the fan is currently driven: `auto`, `manual`, or `unavailable`.
    #[zbus(property)]
    async fn fan_mode(&self) -> String {
        match self.fan.mode() {
            Some(m) => m.to_string(),
            None => "unavailable".into(),
        }
    }

    /// Current duty 0-255 as the EC reports it, or 0 when unavailable. Under EC
    /// control this reads 0 regardless of how fast the fan is actually turning, so it
    /// is only meaningful alongside `FanMode` - read `Telemetry`'s fan rpm for speed.
    #[zbus(property)]
    async fn fan_duty(&self) -> u8 {
        self.fan.duty().unwrap_or(0)
    }

    /// Take manual fan control and hold `duty` (0-255).
    ///
    /// Returns the duty the EC actually settled on, which may differ by a count or
    /// two: the EC stores whole percent, so 180 comes back as 181.
    ///
    /// **This is not a fan curve.** It pins one duty until something changes it, with
    /// no temperature feedback whatsoever, and the safety layers that make a curve
    /// safe to expose (firmware-floor clamp, critical-temperature override, watchdog)
    /// are not built yet. The flat `MIN_DUTY` floor is what stands in for them, and it
    /// is a poor substitute - see `fan::MIN_DUTY`.
    async fn set_fan_duty(
        &self,
        duty: u8,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<u8> {
        // Capability before authorization: there is no point prompting for a password
        // to do something this machine cannot do, and the reason is more useful than
        // an auth failure would be.
        if let fw_helper_core::Cap::No(reason) = &self.caps.fan_control {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        // Refuse if our own heartbeat is stale.
        //
        // This is not hypothetical: zbus serves this interface from its own executor,
        // not from the tokio runtime (measured 2026-08-21 — blocking every tokio
        // worker left D-Bus answering normally). So the poll loop can be dead while
        // this method still runs, and without this check the fan would be handed to a
        // daemon that is not minding it, taken back by the watchdog five seconds
        // later, and handed over again on the next call.
        let stale = self.watchdog.since_beat();
        if stale > crate::watchdog::TIMEOUT {
            return Err(zbus::fdo::Error::Failed(format!(
                "refusing manual fan control: this daemon's telemetry loop has not run \
                 for {:.0}s, so nothing would be watching the fan. Restart fw-helperd",
                stale.as_secs_f64()
            )));
        }

        let sender = Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;

        let settled = self
            .fan
            .set_duty(duty)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        eprintln!("fan set to duty {settled}/255 by {sender}");
        Ok(settled)
    }

    /// Hand the fan back to the EC.
    ///
    /// Deliberately requires the same authorization as taking control. It is the safe
    /// direction, but it is still a change to how the machine behaves, and a caller
    /// permitted to undo another user's setting without authenticating would be a
    /// surprise.
    async fn set_fan_auto(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        if let fw_helper_core::Cap::No(reason) = &self.caps.fan_control {
            return Err(zbus::fdo::Error::NotSupported(reason.clone()));
        }
        let sender = Self::authorize(&header, conn, polkit::actions::SET_FAN).await?;

        self.fan
            .release()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        eprintln!("fan returned to EC control by {sender}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    const ATTR: &str = "sys/class/power_supply/BAT1/charge_control_end_threshold";

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Same rooted-sysfs trick as the core fixtures (ADR 0004): no hardware, no root.
    fn fixture(tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helperd-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn daemon_with(root: &Path, persisted: Option<u8>) -> Daemon {
        let fs_ = Sysfs::new(root);
        let caps = Capabilities::probe(&fs_);
        let state = State {
            charge_limit: persisted,
        };
        let lease = Arc::new(crate::fan::FanLease::new(fs_.clone()));
        let wd = crate::watchdog::Watchdog::new(Arc::clone(&lease));
        wd.beat();
        Daemon::new(fs_, caps, state, lease, wd)
    }

    fn write_attr(root: &Path, value: &str) {
        let p = root.join(ATTR);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, value).unwrap();
    }

    fn read_attr(root: &Path) -> String {
        fs::read_to_string(root.join(ATTR)).unwrap().trim().into()
    }

    #[test]
    fn reapply_reports_when_firmware_left_the_limit_alone() {
        let root = fixture("already");
        write_attr(&root, "80\n");
        let d = daemon_with(&root, Some(80));

        assert_eq!(d.reapply_charge_limit(), Reapply::AlreadyCorrect);
        assert_eq!(read_attr(&root), "80");
    }

    #[test]
    fn reapply_corrects_when_firmware_reset_the_limit() {
        let root = fixture("reset");
        // What a firmware reset across suspend would look like.
        write_attr(&root, "100\n");
        let d = daemon_with(&root, Some(80));

        assert_eq!(d.reapply_charge_limit(), Reapply::Corrected);
        assert_eq!(read_attr(&root), "80");
    }

    #[test]
    fn reapply_does_nothing_without_a_persisted_limit() {
        let root = fixture("none");
        write_attr(&root, "100\n");
        let d = daemon_with(&root, None);

        assert_eq!(d.reapply_charge_limit(), Reapply::NothingPersisted);
        // Untouched — an absent limit is not a request to set 100%.
        assert_eq!(read_attr(&root), "100");
    }

    #[test]
    fn reapply_fails_loudly_when_charge_control_is_unavailable() {
        // No attribute at all: the module parameter is unset (ADR 0008).
        let root = fixture("unsupported");
        fs::create_dir_all(&root).unwrap();
        let d = daemon_with(&root, Some(80));

        assert_eq!(d.reapply_charge_limit(), Reapply::Failed);
    }
}
