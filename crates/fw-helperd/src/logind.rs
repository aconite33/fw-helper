//! logind integration.
//!
//! The energy counter cannot be trusted across suspend — it may reset while
//! wall-clock advances, so any delta spanning the gap is meaningless (ADR 0009).
//! Watch `PrepareForSleep` and invalidate the sampler's reference point on resume.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Login1Manager {
    /// `true` when about to sleep, `false` on resume.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Spawn a task that sets `flag` when the system resumes.
///
/// Failure to reach logind is not fatal: the sampler's max-gap check already
/// discards deltas spanning a long stall, so this is a refinement rather than the
/// only line of defence. Log and continue.
pub async fn watch_resume(conn: &zbus::Connection, flag: Arc<AtomicBool>) {
    let proxy = match Login1ManagerProxy::new(conn).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("logind unavailable ({e}); relying on sampler max-gap instead");
            return;
        }
    };
    let mut stream = match proxy.receive_prepare_for_sleep().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot subscribe to PrepareForSleep ({e}); relying on max-gap");
            return;
        }
    };

    tokio::spawn(async move {
        use futures_util::StreamExt;
        while let Some(signal) = stream.next().await {
            if let Ok(args) = signal.args() {
                if !args.start {
                    // Resuming. The next energy delta would span the sleep.
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }
    });
}
