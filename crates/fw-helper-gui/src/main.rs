//! `fw-helper` — desktop UI.
//!
//! Holds **no** hardware access whatsoever. Everything comes from `fw-helperd` over
//! D-Bus, and this process runs entirely unprivileged (ADR 0003).
//!
//! Controls mirror what the daemon exposes; anything it reports as unavailable is
//! shown disabled with the reason rather than silently omitted.

mod curve;
mod ui;
mod worker;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "org.fwhelper.Gui";

/// `SIGINT` and `SIGTERM`. Spelled out rather than pulling in libc for two integers.
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// Follow the desktop's light/dark preference, including on desktops that do not
/// publish one the way libadwaita expects.
///
/// libadwaita decides this from the XDG settings portal's
/// `org.freedesktop.appearance color-scheme`. On GNOME that works. **On Cinnamon the
/// portal answers `0` — "no preference" — even when the session is unambiguously dark**,
/// so the app renders light inside a dark desktop. Measured on Cinnamon 6.6.9:
///
/// ```text
/// org.cinnamon.desktop.interface gtk-theme    'Adwaita-dark'
/// org.gnome.desktop.interface    color-scheme 'prefer-dark'
/// portal org.freedesktop.appearance color-scheme -> uint32 0
/// ```
///
/// So: when the portal expresses a real preference, leave it alone and let libadwaita
/// do its job. Otherwise fall back to the GTK theme name, which Cinnamon *does* set.
/// The fallback only ever forces dark — a light theme name is indistinguishable from
/// "no opinion" here, and `Default` already means light.
fn follow_desktop_color_scheme() {
    // An explicit override, mostly so this is testable on a desktop whose preference
    // cannot easily be changed. Never set in normal use.
    match std::env::var("FW_HELPER_COLOR_SCHEME").as_deref() {
        Ok("dark") => {
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
            return;
        }
        Ok("light") => {
            adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
            return;
        }
        _ => {}
    }

    let manager = adw::StyleManager::default();
    if manager.is_dark() {
        // The portal already said dark; nothing to correct.
        return;
    }

    // Ask GSettings directly rather than going through GTK.
    //
    // GTK4 does not read XSettings — it asks the same settings portal libadwaita just
    // failed to get an answer from, so on Cinnamon `gtk_theme_name()` reports a bare
    // "Adwaita" rather than the "Adwaita-dark" the session is actually using. Reading
    // the schema itself is the only place the truth is available.
    let prefers_dark = gsetting("org.gnome.desktop.interface", "color-scheme")
        .is_some_and(|v| v.contains("dark"))
        || [
            "org.cinnamon.desktop.interface",
            "org.gnome.desktop.interface",
            "org.mate.interface",
        ]
        .iter()
        .filter_map(|schema| gsetting(schema, "gtk-theme"))
        .any(|theme| theme.to_lowercase().contains("dark"))
        || gtk::Settings::default()
            .and_then(|s| s.gtk_theme_name())
            .is_some_and(|t| t.to_lowercase().contains("dark"));

    if prefers_dark {
        manager.set_color_scheme(adw::ColorScheme::ForceDark);
    }
}

/// Read one GSettings key, or `None` if this desktop does not have it.
///
/// Both guards are load-bearing: constructing a `Settings` for a schema that is not
/// installed **aborts the process**, and so does reading a key the schema does not
/// define. Neither is a catchable error, and a GUI that dies on a desktop for having
/// the wrong schemas installed would be a worse bug than the one this fixes.
fn gsetting(schema_id: &str, key: &str) -> Option<String> {
    let schema = gtk::gio::SettingsSchemaSource::default()?.lookup(schema_id, true)?;
    if !schema.has_key(key) {
        return None;
    }
    Some(
        gtk::gio::Settings::new(schema_id)
            .string(key)
            .trim_matches('\'')
            .to_string(),
    )
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        follow_desktop_color_scheme();
        ui::load_css();
    });
    app.connect_activate(ui::build);

    // Quit cleanly on Ctrl-C and on SIGTERM.
    //
    // GTK installs no handler for either, and this app is normally started from a
    // terminal, so the obvious way to stop it is Ctrl-C. Exiting cleanly also releases
    // the application ID: GTK applications are single-instance, so a lingering process
    // makes the *next* launch silently activate the old window instead of starting the
    // new build — which looks exactly like a build that did nothing.
    for signal in [SIGINT, SIGTERM] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            eprintln!("signal {signal}, closing");
            app.quit();
            glib::ControlFlow::Break
        });
    }

    app.run()
}
