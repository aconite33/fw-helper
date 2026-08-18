//! `fw-helperd` — privileged daemon for fw-helper.
//!
//! Owns all hardware access; the GUI and CLI hold none and reach it over D-Bus
//! (ADR 0003). **M1b is entirely read-only** — it publishes telemetry and
//! capabilities and writes nothing. Hardware writes arrive with M2.

mod iface;
mod logind;
mod polkit;
mod state;
mod wire;

use fw_helper_core::{Capabilities, Monitor, Sysfs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const BUS_NAME: &str = "org.fwhelper.Daemon1";
const OBJECT_PATH: &str = "/org/fwhelper/Daemon1";

/// One second. This is the publication rate cap from ADR 0009, not a performance
/// tuning knob — do not raise it without superseding that ADR.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fs = Sysfs::default();
    let caps = Capabilities::probe(&fs);

    eprintln!("fw-helperd starting");
    for (name, cap) in caps.summary() {
        eprintln!("  {name:<18} {cap}");
    }
    if !caps.package_power.is_available() {
        eprintln!("note: package power needs root; running unprivileged?");
    }

    let state = state::State::load();
    if let Some(limit) = state.charge_limit {
        eprintln!("persisted charge limit: {limit}%");
    }
    let daemon = iface::Daemon::new(fs.clone(), caps, state);
    daemon.reapply_charge_limit();

    // The session bus is a development affordance: claiming a name on the system bus
    // needs both root and an installed policy file, which makes iterating painful.
    // Production always uses the system bus -- the systemd unit does not set this.
    let session = std::env::var_os("FW_HELPERD_SESSION_BUS").is_some();
    let builder = if session {
        eprintln!("FW_HELPERD_SESSION_BUS set -- using session bus (development only)");
        zbus::connection::Builder::session()?
    } else {
        zbus::connection::Builder::system()?
    };

    let conn = builder
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    eprintln!("listening on {BUS_NAME} at {OBJECT_PATH}");

    let resumed = Arc::new(AtomicBool::new(false));
    logind::watch_resume(&conn, Arc::clone(&resumed)).await;

    let poll = tokio::spawn(poll_loop(conn.clone(), Monitor::new(fs), resumed));

    // Shut down cleanly on either signal. From M3 this is also where manual fan
    // control gets released, so the structure matters more than it looks (ADR 0006).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("SIGINT, shutting down"),
        _ = sigterm.recv()          => eprintln!("SIGTERM, shutting down"),
    }
    poll.abort();
    Ok(())
}

async fn poll_loop(conn: zbus::Connection, mut mon: Monitor, resumed: Arc<AtomicBool>) {
    let iface_ref = match conn
        .object_server()
        .interface::<_, iface::Daemon>(OBJECT_PATH)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot obtain interface reference: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;

        // swap(false) so the resume is consumed exactly once.
        if resumed.swap(false, Ordering::SeqCst) {
            mon.on_resume();
            eprintln!("resumed from sleep; energy reference invalidated");
            // Firmware commonly resets the charge threshold across suspend.
            if let Ok(guard) = conn
                .object_server()
                .interface::<_, iface::Daemon>(OBJECT_PATH)
                .await
            {
                guard.get().await.reapply_charge_limit();
            }
        }

        let sample = mon.sample();
        let mut guard = iface_ref.get_mut().await;
        if guard.update(sample) {
            let emitter = iface_ref.signal_emitter();
            let _ = guard.telemetry_changed(emitter).await;
            let _ = guard.critical_temperatures_changed(emitter).await;
        }
    }
}
