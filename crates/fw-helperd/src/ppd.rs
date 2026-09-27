//! power-profiles-daemon client.
//!
//! ADR 0005: PPD owns `platform_profile` and EPP, and GNOME's power slider is wired to
//! it. We delegate that axis and layer our own knobs on top. Writing those paths
//! ourselves would be last-writer-wins against the desktop's own UI, which is the worst
//! bug class in this project — the slider silently overrides us, or we silently override
//! it, and the UI shows a state that is not real.
//!
//! Two details, both measured on the target machine rather than assumed:
//!
//! - **PPD owns both bus names.** `org.freedesktop.UPower.PowerProfiles` and the older
//!   `net.hadess.PowerProfiles` are both registered by the same process, and both serve
//!   the interface under the newer name at `/org/freedesktop/UPower/PowerProfiles`. We
//!   prefer the newer destination and fall back to the older.
//! - **`ActiveProfile` is a writable property that emits change signals.** So switching
//!   is a property write, and following the slider is a `PropertiesChanged` subscription
//!   rather than a poll.

use fw_helper_core::{Ppd, Sysfs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the startup probe may wait for PPD before carrying on without it.
///
/// PPD is D-Bus-activatable, so *our own probe* is what asks systemd to start it - and a
/// blocking call cannot be what unblocks it. Measured on the AMD board at boot,
/// 2026-09-27: the unbounded probe blocked 90 s, systemd killed the unit on its start
/// timeout, and PPD's own activation completed 45 ms after our process died. The restart
/// then connected in 1 s. So the wait was a deadlock of our own making, and the daemon
/// was absent for 95 s of every boot - which is what a user sees as "fw-helperd is not
/// available". Two seconds is far longer than the 4 ms a warm PPD answers in.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const PREFERRED: &str = "org.freedesktop.UPower.PowerProfiles";
const LEGACY: &str = "net.hadess.PowerProfiles";

#[zbus::proxy(
    interface = "org.freedesktop.UPower.PowerProfiles",
    assume_defaults = false
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, profile: &str) -> zbus::Result<()>;
}

/// How the PPD axis is being driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Delegating to PPD, as ADR 0005 requires.
    Ppd,
    /// PPD is absent, so we write `platform_profile` ourselves. Reported in
    /// capabilities, because it means the GNOME slider is not in the loop.
    DirectSysfs,
    /// Neither available.
    None,
}

/// Try both bus names, bounded, and return the first that answers.
///
/// Constructing a proxy contacts nothing, so each candidate is forced to make a real
/// round trip: without it an absent PPD looks identical to a present one until the first
/// call that matters fails.
async fn probe(conn: &zbus::Connection) -> Option<(PowerProfilesProxy<'static>, &'static str)> {
    for dest in [PREFERRED, LEGACY] {
        let built = PowerProfilesProxy::builder(conn)
            .destination(dest)
            .and_then(|b| b.path(PATH))
            .map(|b| b.build());
        let Ok(fut) = built else { continue };
        let Ok(proxy) = fut.await else { continue };
        match tokio::time::timeout(PROBE_TIMEOUT, proxy.active_profile()).await {
            Ok(Ok(_)) => return Some((proxy, dest)),
            // Timed out, or answered with an error. Either way, not usable now; the
            // name watcher will pick it up if it becomes usable later.
            _ => continue,
        }
    }
    None
}

pub struct ProfileAxis {
    /// Mutable state, because PPD can arrive after we do: at boot it is usually not on
    /// the bus yet, and a verdict reached in the first two seconds would otherwise
    /// stand for the whole session - writing `platform_profile` behind the desktop's
    /// back, the one thing ADR 0005 forbids. Behind a mutex, never held across an await
    /// (the `&self` rule): callers take a clone and release the lock first.
    proxy: Mutex<Option<PowerProfilesProxy<'static>>>,
    /// Kept so a late adoption can build its proxy on the same connection.
    conn: Option<zbus::Connection>,
    /// Woken when a proxy is adopted, so the follower waits rather than polls.
    adopted: tokio::sync::Notify,
    fs: Sysfs,
}

impl ProfileAxis {
    /// Connect to PPD, preferring the newer bus name.
    ///
    /// Bounded, and never fatal: an absent PPD leaves the axis on its sysfs fallback and
    /// [`Self::adopt_when_available`] takes over from there.
    pub async fn connect(conn: &zbus::Connection, fs: Sysfs) -> Self {
        let axis = Self {
            proxy: Mutex::new(None),
            conn: Some(conn.clone()),
            adopted: tokio::sync::Notify::new(),
            fs,
        };
        match probe(conn).await {
            Some((proxy, dest)) => {
                eprintln!("power profiles: delegating to PPD at {dest}");
                axis.store(Some(proxy));
            }
            None => eprintln!(
                "power profiles: PPD did not answer within {PROBE_TIMEOUT:?}; carrying on \
                 without it and watching for it to appear. Until it does, \
                 platform_profile is written directly and the desktop slider is not in \
                 the loop"
            ),
        }
        axis
    }

    /// Adopt PPD whenever it appears, and let it go when it leaves.
    ///
    /// The other half of the bounded probe. Without this, losing the startup race meant
    /// bypassing the desktop for the rest of the session.
    pub fn adopt_when_available(self: &Arc<Self>) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        let axis = Arc::clone(self);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let dbus = match zbus::fdo::DBusProxy::new(&conn).await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("power profiles: cannot watch for PPD appearing ({e})");
                    return;
                }
            };
            let mut stream = match dbus.receive_name_owner_changed().await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("power profiles: cannot watch for PPD appearing ({e})");
                    return;
                }
            };
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                let name = args.name().as_str();
                if name != PREFERRED && name != LEGACY {
                    continue;
                }
                if args.new_owner().is_some() {
                    if axis.snapshot().is_some() {
                        continue;
                    }
                    if let Some((proxy, dest)) = probe(&conn).await {
                        axis.store(Some(proxy));
                        eprintln!(
                            "power profiles: PPD appeared at {dest}; adopted it and handed \
                             the profile axis back to the desktop"
                        );
                        // notify_one, not notify_waiters: the permit is kept when the
                        // follower is not yet waiting, so adoption cannot be missed.
                        axis.adopted.notify_one();
                    }
                } else if axis.store(None).is_some() {
                    eprintln!(
                        "power profiles: PPD went away; writing platform_profile directly \
                         until it returns"
                    );
                }
            }
        });
    }

    /// No PPD at all: used when even the system bus is unreachable.
    pub fn disconnected(fs: Sysfs) -> Self {
        Self {
            proxy: Mutex::new(None),
            conn: None,
            adopted: tokio::sync::Notify::new(),
            fs,
        }
    }

    /// The proxy as it stands, cloned so no lock is held across an await.
    fn snapshot(&self) -> Option<PowerProfilesProxy<'static>> {
        self.proxy.lock().expect("ppd proxy mutex").clone()
    }

    /// Replace the proxy, returning what was there.
    fn store(
        &self,
        proxy: Option<PowerProfilesProxy<'static>>,
    ) -> Option<PowerProfilesProxy<'static>> {
        std::mem::replace(&mut *self.proxy.lock().expect("ppd proxy mutex"), proxy)
    }

    pub fn backend(&self) -> Backend {
        if self.snapshot().is_some() {
            Backend::Ppd
        } else if self.fs.exists(fw_helper_core::paths::PLATFORM_PROFILE) {
            Backend::DirectSysfs
        } else {
            Backend::None
        }
    }

    /// What PPD says is active right now.
    pub async fn active(&self) -> Option<Ppd> {
        match &self.snapshot() {
            Some(p) => Ppd::parse(&p.active_profile().await.ok()?),
            None => {
                // The fallback path: ACPI's names are not PPD's, so map what we can.
                let raw = self
                    .fs
                    .read_string(fw_helper_core::paths::PLATFORM_PROFILE)
                    .ok()?;
                match raw.as_str() {
                    "low-power" | "quiet" => Some(Ppd::PowerSaver),
                    "balanced" => Some(Ppd::Balanced),
                    "performance" => Some(Ppd::Performance),
                    _ => None,
                }
            }
        }
    }

    /// Ask for a PPD profile.
    pub async fn set(&self, ppd: Ppd) -> Result<(), String> {
        match &self.snapshot() {
            Some(p) => p
                .set_active_profile(ppd.as_str())
                .await
                .map_err(|e| format!("PPD refused {}: {e}", ppd.as_str())),
            None => {
                // ACPI accepts its own vocabulary, which is not PPD's.
                let value = match ppd {
                    Ppd::PowerSaver => "low-power",
                    Ppd::Balanced => "balanced",
                    Ppd::Performance => "performance",
                };
                self.fs
                    .write_string(fw_helper_core::paths::PLATFORM_PROFILE, value)
                    .map_err(|e| format!("cannot write platform_profile: {e}"))
            }
        }
    }

    /// Call `on_change` whenever PPD's active profile changes.
    ///
    /// This is the half of ADR 0005 that keeps the desktop authoritative: the user moves
    /// the GNOME slider, PPD tells us, and we apply the matching fan curve and power
    /// limit. Without it we would be a second, competing source of truth.
    pub fn watch<F>(self: &Arc<Self>, mut on_change: F)
    where
        F: FnMut(Ppd) + Send + 'static,
    {
        let axis = Arc::clone(self);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            // Adopted at startup, or only later? A late adoption has missed whatever PPD
            // was set to meanwhile, so it reconciles once before following.
            let mut reconcile = axis.snapshot().is_none();
            loop {
                if let Some(proxy) = axis.snapshot() {
                    if std::mem::take(&mut reconcile) {
                        if let Some(ppd) = proxy
                            .active_profile()
                            .await
                            .ok()
                            .as_deref()
                            .and_then(Ppd::parse)
                        {
                            on_change(ppd);
                        }
                    }
                    let mut stream = proxy.receive_active_profile_changed().await;
                    while let Some(change) = stream.next().await {
                        if let Ok(name) = change.get().await {
                            match Ppd::parse(&name) {
                                Some(ppd) => on_change(ppd),
                                None => {
                                    eprintln!(
                                        "power profiles: PPD reports unknown profile {name:?}"
                                    )
                                }
                            }
                        }
                    }
                    // The stream ended: PPD is gone. Wait to be handed a new proxy
                    // rather than spinning on a dead one.
                    reconcile = true;
                }
                axis.adopted.notified().await;
            }
        });
    }
}
