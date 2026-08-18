//! Client side of `org.fwhelper.Daemon1`, shared by the CLI and the GUI.
//!
//! Uses zbus's blocking API deliberately: neither consumer wants an async runtime.
//! The GUI drives this from a worker thread so its main loop never blocks on IPC.
//!
//! This crate exists so the proxy is defined once. It sits outside `fw-helper-core`,
//! which stays dependency-free (ADR 0010).

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

#[zbus::proxy(
    interface = "org.fwhelper.Daemon1",
    default_service = "org.fwhelper.Daemon1",
    default_path = "/org/fwhelper/Daemon1"
)]
pub trait Daemon {
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<HashMap<String, (bool, String)>>;
    #[zbus(property)]
    fn telemetry(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn critical_temperatures(&self) -> zbus::Result<HashMap<String, f64>>;
    #[zbus(property)]
    fn version(&self) -> zbus::Result<u32>;
}

/// Highest interface version this client understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// Connect and confirm the daemon is actually answering.
///
/// Constructing a proxy does **not** contact the service, so it succeeds even when
/// nothing owns the name. Without a forced round-trip a caller cannot distinguish
/// "daemon absent" from "daemon present", and every later property read fails
/// separately instead of falling back cleanly.
pub fn connect() -> zbus::Result<(DaemonProxyBlocking<'static>, u32)> {
    let conn = if std::env::var_os("FW_HELPERD_SESSION_BUS").is_some() {
        zbus::blocking::Connection::session()?
    } else {
        zbus::blocking::Connection::system()?
    };
    let proxy = DaemonProxyBlocking::new(&conn)?;
    let version = proxy.version()?;
    Ok((proxy, version))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub label: String,
    pub celsius: f64,
    pub critical: Option<f64>,
}

/// One decoded reading of everything the daemon publishes.
///
/// Decoding happens here so consumers never touch `zvariant` types directly, and so
/// an interface change lands in one place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub package_watts: Option<f64>,
    pub fan_rpm: Option<u64>,
    pub battery_percent: Option<u64>,
    pub battery_status: Option<String>,
    pub platform_profile: Option<String>,
    pub control_sensor: Option<String>,
    pub temps: Vec<Sensor>,
    /// (knob, available, reason-if-not)
    pub capabilities: Vec<(String, bool, String)>,
}

impl Snapshot {
    pub fn fetch(d: &DaemonProxyBlocking<'_>) -> zbus::Result<Self> {
        let t = d.telemetry()?;
        let crit = d.critical_temperatures().unwrap_or_default();

        let mut caps: Vec<(String, bool, String)> = d
            .capabilities()
            .unwrap_or_default()
            .into_iter()
            .map(|(k, (ok, why))| (k, ok, why))
            .collect();
        caps.sort_by(|a, b| a.0.cmp(&b.0));

        let mut temps: Vec<Sensor> = as_temp_map(t.get("temps"))
            .into_iter()
            .map(|(label, celsius)| {
                let critical = crit.get(&label).copied();
                Sensor {
                    label,
                    celsius,
                    critical,
                }
            })
            .collect();
        // Hottest first: that is the one a fan curve cares about and the one a
        // reader scans for.
        temps.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));

        Ok(Self {
            package_watts: t.get("package_watts").and_then(as_f64),
            fan_rpm: t.get("fan_rpm").and_then(as_u64),
            battery_percent: t.get("battery_percent").and_then(as_u64),
            battery_status: t.get("battery_status").and_then(as_string),
            platform_profile: t.get("platform_profile").and_then(as_string),
            control_sensor: t.get("control_sensor").and_then(as_string),
            temps,
            capabilities: caps,
        })
    }

    pub fn capability(&self, name: &str) -> Option<(bool, &str)> {
        self.capabilities
            .iter()
            .find(|(k, _, _)| k == name)
            .map(|(_, ok, why)| (*ok, why.as_str()))
    }
}

fn as_f64(v: &OwnedValue) -> Option<f64> {
    f64::try_from(v)
        .ok()
        .or_else(|| u64::try_from(v).ok().map(|n| n as f64))
}

fn as_u64(v: &OwnedValue) -> Option<u64> {
    u64::try_from(v).ok()
}

fn as_string(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

fn as_temp_map(v: Option<&OwnedValue>) -> HashMap<String, f64> {
    v.and_then(|v| v.try_clone().ok())
        .and_then(|v| HashMap::<String, f64>::try_from(v).ok())
        .unwrap_or_default()
}
