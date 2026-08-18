//! `fw-helper` — desktop UI.
//!
//! Holds **no** hardware access whatsoever. Everything comes from `fw-helperd` over
//! D-Bus, and this process runs entirely unprivileged (ADR 0003).
//!
//! Read-only for now: the daemon exposes no write methods until M2. Controls appear
//! here as those land, and anything the daemon reports as unavailable is shown
//! disabled with the reason rather than silently omitted.

mod ui;
mod worker;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "org.fwhelper.Gui";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| ui::load_css());
    app.connect_activate(ui::build);
    app.run()
}
