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

pub struct ProfileAxis {
    proxy: Option<PowerProfilesProxy<'static>>,
    fs: Sysfs,
}

impl ProfileAxis {
    /// Connect to PPD, preferring the newer bus name.
    pub async fn connect(conn: &zbus::Connection, fs: Sysfs) -> Self {
        for dest in [PREFERRED, LEGACY] {
            let built = PowerProfilesProxy::builder(conn)
                .destination(dest)
                .and_then(|b| b.path(PATH))
                .map(|b| b.build());
            let Ok(fut) = built else { continue };
            if let Ok(proxy) = fut.await {
                // Constructing a proxy contacts nothing, so force a round trip: without
                // it an absent PPD looks identical to a present one until the first
                // real call fails.
                if proxy.active_profile().await.is_ok() {
                    eprintln!("power profiles: delegating to PPD at {dest}");
                    return Self {
                        proxy: Some(proxy),
                        fs,
                    };
                }
            }
        }
        eprintln!(
            "power profiles: PPD unavailable; falling back to writing platform_profile \
             directly. The GNOME power slider will not reflect changes made here"
        );
        Self { proxy: None, fs }
    }

    /// No PPD at all: used when even the system bus is unreachable.
    pub fn disconnected(fs: Sysfs) -> Self {
        Self { proxy: None, fs }
    }

    pub fn backend(&self) -> Backend {
        if self.proxy.is_some() {
            Backend::Ppd
        } else if self.fs.exists(fw_helper_core::paths::PLATFORM_PROFILE) {
            Backend::DirectSysfs
        } else {
            Backend::None
        }
    }

    /// What PPD says is active right now.
    pub async fn active(&self) -> Option<Ppd> {
        match &self.proxy {
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
        match &self.proxy {
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
    pub async fn watch<F>(&self, mut on_change: F)
    where
        F: FnMut(Ppd) + Send + 'static,
    {
        let Some(proxy) = self.proxy.clone() else {
            eprintln!("power profiles: no PPD to follow; the desktop slider is not wired in");
            return;
        };
        let mut stream = proxy.receive_active_profile_changed().await;
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(change) = stream.next().await {
                if let Ok(name) = change.get().await {
                    match Ppd::parse(&name) {
                        Some(ppd) => on_change(ppd),
                        None => eprintln!("power profiles: PPD reports unknown profile {name:?}"),
                    }
                }
            }
        });
    }
}
