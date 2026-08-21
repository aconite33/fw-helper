//! `fw-helperd` — privileged daemon for fw-helper.
//!
//! Owns all hardware access; the GUI and CLI hold none and reach it over D-Bus
//! (ADR 0003). Writes arrived with M2 (charge limit) and M3 (fan).
//!
//! Manual fan control is the one thing here that can damage the machine, so the
//! shutdown paths are not boilerplate: every route out of this process must return
//! `pwm1_enable=2` (ADR 0006). Clean exit, `SIGTERM`, `SIGINT` and panic are covered
//! below; `SIGKILL` and a hung process are not, and are covered by `ExecStopPost` and
//! the watchdog respectively. The watchdog's heartbeat is the telemetry poll loop, so
//! the thing that proves this daemon is alive is the same thing that proves it is
//! doing its job.

mod fan;
mod iface;
mod logind;
mod polkit;
mod state;
mod watchdog;
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
    // Started before the bus name is claimed: from the moment a client can ask for
    // manual fan control, the thing that takes it back must already be running.
    let watchdog = watchdog::Watchdog::new(Arc::clone(&lease));
    watchdog.spawn();

    // Beat once before serving: the interface refuses fan control on a stale
    // heartbeat, and at startup the poll loop has not ticked yet.
    watchdog.beat();
    let daemon = iface::Daemon::new(
        fs.clone(),
        caps,
        state,
        Arc::clone(&lease),
        Arc::clone(&watchdog),
    );
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
    logind::watch_sleep(&conn, Arc::clone(&resumed), Arc::clone(&lease)).await;

    let poll = tokio::spawn(poll_loop(
        conn.clone(),
        Monitor::new(fs),
        resumed,
        Arc::clone(&watchdog),
        Arc::clone(&lease),
    ));

    // Shut down cleanly on either signal, and hand the fan back before doing anything
    // else — the poll task is irrelevant if the fan is stuck (ADR 0006).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("SIGINT, shutting down"),
        _ = sigterm.recv()          => eprintln!("SIGTERM, shutting down"),
    }
    shut_down_fan(&lease, &watchdog);
    poll.abort();
    Ok(())
}

/// Take the fan back after a resume, if it was ours before the sleep.
///
/// Runs on the first tick after waking, with telemetry sampled *after* the wake, so
/// the floor and ceiling reflect the machine as it is now rather than as it was when
/// it went down.
fn restore_fan(lease: &fan::FanLease, sample: &fw_helper_core::Telemetry) {
    let celsius = sample.control_temp().map(|t| t.celsius);
    let ceiling = fan::ceiling_for(sample.control_temp().and_then(|t| t.critical));

    match lease.restore_after_resume(celsius, ceiling) {
        None => {}
        Some(Ok(applied)) if applied.clamped() => eprintln!(
            "fan: restored after resume at duty {}/255, raised from the requested {} \
             by the firmware floor",
            applied.settled, applied.requested
        ),
        Some(Ok(applied)) => eprintln!(
            "fan: restored manual control after resume at duty {}/255",
            applied.settled
        ),
        // Refusing to restore is a legitimate outcome, not a failure: waking too hot,
        // or with no readable sensor, are both reasons the fan should stay firmware's.
        Some(Err(e)) => eprintln!("fan: not restoring manual control after resume: {e}"),
    }
}

/// Is the machine heating? Carries the previous answer through unchanged readings.
///
/// Split out and pure so the plateau case can be tested: the die sensor is quantized
/// to ~1 C, so a slow cooldown produces long runs of identical readings. Deriving
/// direction from each pair alone treats those as "steady", and steady was counted as
/// rising, so most of the descending branch got recorded. That is exactly what the
/// ascending-only rule exists to exclude. Hardware caught it; the unit tests were happy.
fn next_direction(celsius: Option<f64>, previous: Option<f64>, was_rising: bool) -> bool {
    match (celsius, previous) {
        (Some(now), Some(before)) if now > before => true,
        (Some(now), Some(before)) if now < before => false,
        // Unchanged reading: keep going the way we were. A plateau while heating is
        // sustained load, and the most valuable observation available - it is firmware
        // stating what this temperature actually needs.
        (Some(_), Some(_)) => was_rising,
        // No history: assume rising so a cold start learns something.
        (Some(_), None) => true,
        _ => was_rising,
    }
}

/// What `govern_fan` has to remember between ticks.
#[derive(Default)]
struct Govern {
    celsius: Option<f64>,
    duty: Option<u8>,
    /// Which way the temperature was last seen moving. Persists through plateaus.
    rising: bool,
}

/// One tick of fan governance: learn what firmware does, and hold ourselves to it.
///
/// Two halves, and which one runs depends on who owns the fan:
///
/// - **EC owns it:** record what firmware is actually doing at this temperature. The
///   static floor table has only four points and a large gap right across the knee,
///   so this is what turns a model into a measurement.
/// - **We own it:** re-enforce the floor. This is the half that matters. Clamping only
///   when a duty is requested protects nothing — a duty chosen at idle is safe when it
///   is chosen and becomes stuck-low as soon as the machine is loaded.
fn govern_fan(lease: &fan::FanLease, sample: &fw_helper_core::Telemetry, state: &mut Govern) {
    let previous_duty = state.duty;
    let celsius = sample.control_temp().map(|t| t.celsius);
    let ceiling = fan::ceiling_for(sample.control_temp().and_then(|t| t.critical));

    // Rising or steady. Firmware's descending branch is hysteresis rather than a
    // requirement - measured, it runs duty 0 at 61.9 C climbing and duty 92 at the
    // same temperature falling - so only the ascending branch says anything about what
    // a temperature actually needs.
    let previous = state.celsius;
    // Direction has to PERSIST across plateaus rather than be re-derived from each
    // pair of samples. The sensor is quantized to ~1 C, so a slow cooldown produces
    // long runs of identical readings; an earlier version treated "steady" as rising
    // and therefore recorded most of the descending branch, which is precisely what
    // the ascending-only rule exists to exclude. Measured on hardware: it learned
    // duty 61 at 52.9 C while cooling, where firmware climbing is silent.
    //
    // Steady still counts as rising once we are rising, because a plateau under
    // sustained load is the single most valuable observation there is: it is firmware
    // stating what this temperature actually needs.
    state.rising = next_direction(celsius, previous, state.rising);
    let rising = celsius.is_some() && state.rising;
    state.celsius = celsius;

    if !lease.held() {
        // pwm1 reports firmware's OWN duty while the EC owns the fan, so this reads
        // the real curve rather than inferring it from RPM through two tables.
        if let (Some(c), Some(duty)) = (celsius, lease.duty()) {
            // Credit the whole span since the last sample, not just this point: the die
            // sensor climbs ~4 C/s under load, so consecutive 1 Hz samples skip whole
            // buckets and endpoint-only recording learns almost nothing per event.
            //
            // Only when firmware's duty was the SAME at both ends, though. If it
            // changed, we do not know where in the interval it changed, and crediting
            // the new duty to the whole span would attribute a spun-up fan to every
            // cooler bucket the ramp passed through - erring loud, but erasing the
            // record that firmware runs the fan off down there, which is the finding
            // this whole mechanism exists to capture.
            let from = match (previous, previous_duty) {
                (Some(p), Some(pd)) if pd == duty => p,
                _ => c,
            };
            lease.observe(from, c, duty, rising);
            state.duty = Some(duty);
        } else {
            state.duty = None;
        }
        return;
    }

    match lease.enforce_floor(celsius, ceiling) {
        None => {}
        // Re-asserting the same duty is a real write but not news. It happens because
        // the EC quantizes: a target of 88 settles at 89, so next tick's target of 89
        // is a genuine change of decision that moves nothing.
        Some(fan::Enforced::Corrected { from, to, .. }) if from == to => {}
        Some(fan::Enforced::Corrected {
            from,
            to,
            floor,
            celsius,
        }) => eprintln!(
            "fan: {celsius:.1} C puts the firmware floor at {floor}/255; moved {from} -> {to}"
        ),
        Some(fan::Enforced::ReleasedTooHot {
            celsius,
            ceiling,
            released,
        }) => eprintln!(
            "fan: {celsius:.1} C reached the {ceiling} ceiling; the fan belongs to \
             firmware from here (ADR 0006 point 5). Returned to EC control: {released}"
        ),
        Some(fan::Enforced::ReleasedNoSensor { released }) => eprintln!(
            "fan: temperature became unreadable while under manual control; \
             returned to EC control: {released}"
        ),
        Some(fan::Enforced::Failed(e)) => {
            eprintln!("fan: could not enforce the firmware floor: {e}")
        }
    }
}

/// Fault injection for the watchdog, off unless `FW_HELPERD_DEBUG_WEDGE_AFTER` is set
/// to a number of seconds.
///
/// This exists because the watchdog's entire claim is about a failure that cannot be
/// produced on demand: a daemon that is alive, holding the fan, and no longer doing
/// any work. Unit tests with a 50 ms timeout show the logic is right; they cannot show
/// that a real wedged runtime on real hardware ends with the fan back under EC
/// control. Blocking a runtime worker thread forever reproduces exactly that, and
/// `SIGSTOP` cannot substitute — it freezes the watchdog thread too, which is a
/// failure nothing in this process could cover anyway.
///
/// Never set in production. The variable is read every tick rather than cached so the
/// cost when unset is one `getenv`, and so it cannot be armed after start.
fn maybe_wedge(started: &std::time::Instant) {
    let Some(after) = std::env::var_os("FW_HELPERD_DEBUG_WEDGE_AFTER") else {
        return;
    };
    let Some(secs) = after.to_str().and_then(|s| s.parse::<u64>().ok()) else {
        return;
    };
    if started.elapsed() < Duration::from_secs(secs) {
        return;
    }
    // Block *every* worker, not just this one. Blocking a single thread of a
    // multi-threaded runtime leaves D-Bus answering normally, which is a much weaker
    // fault than the one the watchdog claims to cover — measured on 2026-08-21, where
    // the first version of this injection left the daemon perfectly responsive.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    eprintln!(
        "DEBUG: wedging the runtime deliberately (FW_HELPERD_DEBUG_WEDGE_AFTER={secs}), \
         blocking {} worker slots. The watchdog should take the fan back.",
        workers * 2
    );
    for _ in 0..workers * 2 {
        tokio::spawn(async {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
    // And this one last, so the queued blockers get picked up first.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Release manual fan control on the way out, saying which case it was.
///
/// Reads before writing for the same reason M2's charge re-apply does: "we were not
/// holding it" and "we were, and gave it back" are different facts, and a log line
/// that cannot tell them apart cannot show that the restore paths work.
fn shut_down_fan(lease: &fan::FanLease, watchdog: &watchdog::Watchdog) {
    // A trip means this daemon stopped working while holding the fan. The machine was
    // protected, but that is a bug and it should not vanish quietly at shutdown.
    if watchdog.tripped() {
        eprintln!("note: the fan watchdog intervened at least once during this run");
    }
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

async fn poll_loop(
    conn: zbus::Connection,
    mut mon: Monitor,
    resumed: Arc<AtomicBool>,
    watchdog: Arc<watchdog::Watchdog>,
    lease: Arc<fan::FanLease>,
) {
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

    let started = std::time::Instant::now();
    let mut restore_fan_after_resume = false;
    let mut govern = Govern::default();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;

        // The heartbeat. Deliberately the first thing after the tick and before any
        // work that could block: it must mean "the runtime is turning", not "the last
        // iteration happened to finish".
        watchdog.beat();

        maybe_wedge(&started);

        // swap(false) so the resume is consumed exactly once.
        if resumed.swap(false, Ordering::SeqCst) {
            mon.on_resume();
            eprintln!("resumed from sleep; energy reference invalidated");
            // The fan is restored below rather than here, on the freshly sampled
            // telemetry: the machine may have woken warmer than it slept, and the
            // floor must be computed from temperatures read after the wake.
            restore_fan_after_resume = true;
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
        if std::mem::take(&mut restore_fan_after_resume) {
            restore_fan(&lease, &sample);
        }
        govern_fan(&lease, &sample, &mut govern);

        let mut guard = iface_ref.get_mut().await;
        if guard.update(sample) {
            let emitter = iface_ref.signal_emitter();
            let _ = guard.telemetry_changed(emitter).await;
            let _ = guard.critical_temperatures_changed(emitter).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::next_direction;

    #[test]
    fn a_plateau_keeps_the_direction_it_arrived_with() {
        // The bug hardware caught. Cooling from 60 C, the sensor reports
        // 53.9, 53.9, 53.9 ... and each identical pair looks "steady". Treating that
        // as rising recorded firmware's descending duty - measured, duty 61 at 52.9 C,
        // where firmware climbing through the same temperature is silent.
        let mut rising = true;
        for t in [60.0, 58.0, 56.0] {
            rising = next_direction(Some(t), Some(t + 2.0), rising);
        }
        assert!(!rising, "should be falling after a descent");

        for _ in 0..10 {
            rising = next_direction(Some(53.9), Some(53.9), rising);
            assert!(
                !rising,
                "a plateau during a cooldown must not read as heating"
            );
        }
    }

    #[test]
    fn a_plateau_under_sustained_load_still_counts_as_heating() {
        // The other half: holding at temperature under load is firmware telling us
        // what that temperature needs, and must be recorded.
        let mut rising = next_direction(Some(70.0), Some(65.0), false);
        assert!(rising);
        for _ in 0..10 {
            rising = next_direction(Some(70.0), Some(70.0), rising);
            assert!(rising, "a plateau while heating is sustained load");
        }
    }

    #[test]
    fn direction_flips_on_a_real_change() {
        assert!(next_direction(Some(50.0), Some(49.0), false));
        assert!(!next_direction(Some(49.0), Some(50.0), true));
    }

    #[test]
    fn a_cold_start_assumes_heating() {
        assert!(next_direction(Some(40.0), None, false));
    }
}
