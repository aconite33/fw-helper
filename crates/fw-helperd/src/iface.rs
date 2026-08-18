//! The `org.fwhelper.Daemon1` interface.
//!
//! M1b is read-only: properties only, no methods that touch hardware. Writes arrive
//! with M2 onward, each behind its own polkit action (ADR 0003).

use crate::state::State;
use crate::{polkit, wire};
use fw_helper_core::{Capabilities, ChargeControl, Sysfs, Telemetry};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

pub struct Daemon {
    fs: Sysfs,
    caps: Capabilities,
    latest: Telemetry,
    state: State,
}

impl Daemon {
    pub fn new(fs: Sysfs, caps: Capabilities, state: State) -> Self {
        Self {
            fs,
            caps,
            latest: Telemetry::default(),
            state,
        }
    }

    /// Re-apply the persisted charge limit. Called at startup, because
    /// `charge_control_end_threshold` does not survive a reboot, and on resume,
    /// because firmware may reset it.
    pub fn reapply_charge_limit(&self) {
        let Some(limit) = self.state.charge_limit else {
            return;
        };
        match ChargeControl::new(&self.fs).set(limit) {
            Ok(()) => eprintln!("re-applied charge limit {limit}%"),
            Err(e) => eprintln!("could not re-apply charge limit {limit}%: {e}"),
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
        &mut self,
        percent: u8,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| zbus::fdo::Error::AuthFailed("no caller identity".into()))?;

        polkit::check(conn, &sender, polkit::actions::SET_CHARGE_LIMIT)
            .await
            .map_err(zbus::fdo::Error::AuthFailed)?;

        ChargeControl::new(&self.fs)
            .set(percent)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        self.state.charge_limit = Some(percent);
        self.state.save();
        eprintln!("charge limit set to {percent}% by {sender}");
        Ok(())
    }
}
