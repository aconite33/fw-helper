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

    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    let theme = settings
        .gtk_theme_name()
        .map(|t| t.to_lowercase())
        .unwrap_or_default();
    if theme.contains("dark") {
        manager.set_color_scheme(adw::ColorScheme::ForceDark);
    }
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
