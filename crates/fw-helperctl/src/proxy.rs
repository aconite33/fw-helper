//! Client side of `org.fwhelper.Daemon1`.
//!
//! Uses zbus's blocking API deliberately: a CLI has no use for an async runtime,
//! and this keeps tokio out of the dependency graph entirely (ADR 0010).

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

/// Highest interface version this client understands. The daemon reports its own;
/// a mismatch is reported rather than guessed at.
pub const SUPPORTED_VERSION: u32 = 1;

/// Connect and confirm the daemon is actually answering.
///
/// Constructing a proxy does **not** contact the service, so it succeeds even when
/// nothing owns the name. Without a forced round-trip the caller cannot distinguish
/// "daemon absent" from "daemon present", and every subsequent property read fails
/// individually instead of falling back cleanly. Returns the daemon's interface
/// version alongside the proxy.
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

/// Read an `f64` out of an `a{sv}` entry, tolerating either a double or an integer.
pub fn as_f64(v: &OwnedValue) -> Option<f64> {
    f64::try_from(v)
        .ok()
        .or_else(|| u64::try_from(v).ok().map(|n| n as f64))
}

pub fn as_u64(v: &OwnedValue) -> Option<u64> {
    u64::try_from(v).ok()
}

pub fn as_string(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

pub fn as_temp_map(v: &OwnedValue) -> Option<HashMap<String, f64>> {
    HashMap::<String, f64>::try_from(v.try_clone().ok()?).ok()
}
