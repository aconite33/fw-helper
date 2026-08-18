//! Background polling.
//!
//! The GUI must never block its main loop on IPC. A worker thread owns the D-Bus
//! connection and pushes snapshots down a channel; the UI only ever receives.
//! This also gives reconnection for free — if the daemon is restarted, the worker
//! notices and picks it up without the user doing anything.

use fw_helper_client::{connect, Snapshot};
use std::thread;
use std::time::Duration;

/// Matches the daemon's own publication rate (ADR 0009). Polling faster would
/// return identical values.
const POLL: Duration = Duration::from_secs(1);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub enum Update {
    Data(Box<Snapshot>),
    Disconnected(String),
}

pub fn spawn() -> async_channel::Receiver<Update> {
    // Depth 1: if the UI falls behind there is no value in queueing stale telemetry.
    let (tx, rx) = async_channel::bounded(1);

    thread::spawn(move || loop {
        let (proxy, _version) = match connect() {
            Ok(pair) => pair,
            Err(e) => {
                if tx.send_blocking(Update::Disconnected(e.to_string())).is_err() {
                    return; // UI gone
                }
                thread::sleep(RECONNECT_DELAY);
                continue;
            }
        };

        loop {
            match Snapshot::fetch(&proxy) {
                Ok(s) => {
                    if tx.send_blocking(Update::Data(Box::new(s))).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    // Daemon went away mid-session; drop out to reconnect.
                    let _ = tx.send_blocking(Update::Disconnected(e.to_string()));
                    break;
                }
            }
            thread::sleep(POLL);
        }
        thread::sleep(RECONNECT_DELAY);
    });

    rx
}
