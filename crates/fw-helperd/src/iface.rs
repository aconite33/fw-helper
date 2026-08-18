//! The `org.fwhelper.Daemon1` interface.
//!
//! M1b is read-only: properties only, no methods that touch hardware. Writes arrive
//! with M2 onward, each behind its own polkit action (ADR 0003).

use crate::wire;
use fw_helper_core::{Capabilities, Telemetry};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

pub struct Daemon {
    caps: Capabilities,
    latest: Telemetry,
}

impl Daemon {
    pub fn new(caps: Capabilities) -> Self {
        Self {
            caps,
            latest: Telemetry::default(),
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
}
