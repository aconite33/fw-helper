//! `fw-helperd` — privileged daemon for fw-helper.
//!
//! Owns all hardware access; the GUI and CLI hold none and reach it over D-Bus
//! (ADR 0003). Writes arrived with M2 (charge limit) and M3 (fan).
//!
//! Manual fan control is the one thing here that can damage the machine, so the
//! shutdown paths are not boilerplate: every route out of this process must return
//! `pwm1_enable=2` (ADR 0006). Clean exit, `SIGTERM`, `SIGINT` and panic are covered
//! below; `SIGKILL` and a hung process are not, and are covered by `ExecStopPost` and
//! the watchdog respectively.

mod fan;
mod iface;
mod logind;
mod polkit;
mod state;
mod wire;

use fw_helper_core::{Capabilities, Monitor, Sysfs};
use std::panic;
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

    // Before anything else: if a previous instance died holding manual fan control,
    // take it back. This runs even when fan control is unsupported, where it is a
    // no-op, because the cost of asking is one read.
    let lease = Arc::new(fan::FanLease::new(fs.clone()));
    lease.reclaim_at_startup();
    install_panic_hook(Arc::clone(&lease));

    let state = state::State::load();
    if let Some(limit) = state.charge_limit {
        eprintln!("persisted charge limit: {limit}%");
    }
    let daemon = iface::Daemon::new(fs.clone(), caps, state, Arc::clone(&lease));
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

    // Shut down cleanly on either signal, and hand the fan back before doing anything
    // else — the poll task is irrelevant if the fan is stuck (ADR 0006).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("SIGINT, shutting down"),
        _ = sigterm.recv()          => eprintln!("SIGTERM, shutting down"),
    }
    shut_down_fan(&lease);
    poll.abort();
    Ok(())
}

/// Release manual fan control on the way out, saying which case it was.
///
/// Reads before writing for the same reason M2's charge re-apply does: "we were not
/// holding it" and "we were, and gave it back" are different facts, and a log line
/// that cannot tell them apart cannot show that the restore paths work.
fn shut_down_fan(lease: &fan::FanLease) {
    let held = lease.held();
    if lease.release_now() {
        if held {
            eprintln!("released manual fan control; fan is back under EC control");
        }
    } else {
        eprintln!(
            "WARNING: could not return the fan to EC control. Run \
             fw-helper-restore-fan as root"
        );
    }
}

/// Release the fan on panic, then let the normal hook print the backtrace.
///
/// The hook must not lock anything: a panic can occur while another thread holds a
/// lock, and blocking here would leave the process alive with the fan held — the
/// exact failure ADR 0006 is written against. [`fan::FanLease`] is lock-free for this
/// reason.
fn install_panic_hook(lease: Arc<fan::FanLease>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let ok = lease.release_now();
        eprintln!(
            "fw-helperd panicked; fan {}",
            if ok {
                "returned to EC control"
            } else {
                "NOT returned to EC control - run fw-helper-restore-fan as root"
            }
        );
        previous(info);
    }));
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
