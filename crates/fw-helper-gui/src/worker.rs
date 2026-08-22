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
    /// A control the user operated finished. `key` names the control, so the UI can
    /// stop holding it and let telemetry drive it again.
    CommandResult {
        key: &'static str,
        result: Result<String, String>,
    },
}

/// Something the user asked for. Each is one D-Bus call.
pub enum Command {
    Profile(String),
    PowerLimit(u32),
    ChargeLimit(u8),
    FanAuto,
    AutoProfiles(String, String),
}

impl Command {
    /// Which control this came from. The UI holds that control until the result
    /// arrives: a hardware write goes through polkit and can take seconds, and letting
    /// telemetry overwrite the widget meanwhile is what makes a setting appear to snap
    /// back to its old value the instant it is changed.
    pub fn key(&self) -> &'static str {
        match self {
            Self::Profile(_) => "profile",
            Self::PowerLimit(_) => "power",
            Self::ChargeLimit(_) => "charge",
            Self::FanAuto => "fan",
            Self::AutoProfiles(..) => "auto",
        }
    }
}

/// Commands run on their **own thread with its own connection**, not on the polling
/// one.
///
/// A hardware write goes through polkit, and polkit can legitimately take tens of
/// seconds — it may be showing a password dialog. Sharing the polling thread would
/// freeze the telemetry display for the whole prompt, which is the same mistake the
/// daemon avoids by never holding its interface lock across an await.
fn spawn_commands(tx: async_channel::Sender<Update>) -> std::sync::mpsc::Sender<Command> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
    thread::spawn(move || {
        while let Ok(cmd) = cmd_rx.recv() {
            let key = cmd.key();
            // Connect per command rather than holding one open: commands are rare, and
            // a stale proxy after a daemon restart is a failure mode not worth having.
            let result = match connect() {
                Err(e) => Err(format!("fw-helperd unavailable: {e}")),
                Ok((d, _)) => match cmd {
                    Command::Profile(name) => d
                        .set_profile(&name)
                        .map(|_| format!("profile {name} applied"))
                        .map_err(describe),
                    Command::PowerLimit(w) => d
                        .set_power_limit(w)
                        .map(|_| format!("power limit set to {w} W"))
                        .map_err(describe),
                    Command::ChargeLimit(v) => d
                        .set_charge_limit(v)
                        .map(|_| format!("charge limit set to {v}%"))
                        .map_err(describe),
                    Command::FanAuto => d
                        .set_fan_auto()
                        .map(|_| "fan returned to EC control".to_string())
                        .map_err(describe),
                    Command::AutoProfiles(ac, batt) => d
                        .set_auto_profiles(&ac, &batt)
                        .map(|_| {
                            if ac.is_empty() {
                                "automatic switching off".to_string()
                            } else {
                                format!("on AC: {ac}, on battery: {batt}")
                            }
                        })
                        .map_err(describe),
                },
            };
            if tx
                .send_blocking(Update::CommandResult { key, result })
                .is_err()
            {
                return; // UI gone
            }
        }
    });
    cmd_tx
}

/// D-Bus errors arrive wrapped; the daemon's own message is the useful part and it is
/// written to be read by a person.
fn describe(e: zbus::Error) -> String {
    let text = e.to_string();
    match text.split_once(": ") {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ => text,
    }
}

pub fn spawn() -> (
    async_channel::Receiver<Update>,
    std::sync::mpsc::Sender<Command>,
) {
    // Depth 1: if the UI falls behind there is no value in queueing stale telemetry.
    let (tx, rx) = async_channel::bounded(1);
    let commands = spawn_commands(tx.clone());

    thread::spawn(move || loop {
        let (proxy, _version) = match connect() {
            Ok(pair) => pair,
            Err(e) => {
                if tx
                    .send_blocking(Update::Disconnected(e.to_string()))
                    .is_err()
                {
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

    (rx, commands)
}
