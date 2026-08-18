//! Window construction and updates.

use crate::worker::{self, Update};
use adw::prelude::*;
use gtk::glib;
use fw_helper_client::Snapshot;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const CSS: &str = "
.stat-value  { font-size: 2.1rem; font-weight: 300; }
.stat-label  { font-size: 0.85rem; opacity: 0.6; }
.stat-card   { padding: 14px 18px; border-radius: 12px; }
.sensor-crit { opacity: 0.5; font-size: 0.8rem; }
";

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Widgets that get updated on every tick, held so we mutate rather than rebuild.
struct Widgets {
    title: adw::WindowTitle,
    banner: adw::Banner,
    power: gtk::Label,
    fan: gtk::Label,
    profile: gtk::Label,
    battery: adw::ActionRow,
    sensors_group: adw::PreferencesGroup,
    /// Sensor rows are created on first sight and updated thereafter — rebuilding
    /// the list every second would make it flicker and lose scroll position.
    sensor_rows: HashMap<String, (adw::ActionRow, gtk::LevelBar)>,
    caps_group: adw::PreferencesGroup,
    cap_rows: HashMap<String, adw::ActionRow>,
}

pub fn build(app: &adw::Application) {
    let title = adw::WindowTitle::new("fw-helper", "connecting…");
    let header = adw::HeaderBar::builder().title_widget(&title).build();

    let banner = adw::Banner::builder().revealed(false).build();

    let power = stat_label();
    let fan = stat_label();
    let stats = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build();
    stats.append(&stat_card(&power, "package power"));
    stats.append(&stat_card(&fan, "fan"));

    let profile = gtk::Label::builder().xalign(0.0).label("—").build();
    let profile_row = adw::ActionRow::builder().title("Performance profile").build();
    profile_row.add_suffix(&profile);

    let battery = adw::ActionRow::builder().title("Battery").build();

    let system_group = adw::PreferencesGroup::new();
    system_group.add(&profile_row);
    system_group.add(&battery);

    let sensors_group = adw::PreferencesGroup::builder().title("Temperatures").build();
    let caps_group = adw::PreferencesGroup::builder()
        .title("Capabilities")
        .description("What this machine exposes. Unavailable items say why.")
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&stats);
    content.append(&system_group);
    content.append(&sensors_group);
    content.append(&caps_group);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&banner);
    outer.append(&scroll);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(460)
        .default_height(680)
        .content(&toolbar)
        .build();

    let widgets = Rc::new(RefCell::new(Widgets {
        title,
        banner,
        power,
        fan,
        profile,
        battery,
        sensors_group,
        sensor_rows: HashMap::new(),
        caps_group,
        cap_rows: HashMap::new(),
    }));

    let rx = worker::spawn();
    glib::spawn_future_local(async move {
        while let Ok(update) = rx.recv().await {
            let mut w = widgets.borrow_mut();
            match update {
                Update::Data(s) => apply(&mut w, &s),
                Update::Disconnected(why) => disconnected(&mut w, &why),
            }
        }
    });

    window.present();
}

fn stat_label() -> gtk::Label {
    let l = gtk::Label::builder().label("—").build();
    l.add_css_class("stat-value");
    l
}

fn stat_card(value: &gtk::Label, caption: &str) -> gtk::Widget {
    let label = gtk::Label::builder().label(caption).build();
    label.add_css_class("stat-label");

    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    b.append(value);
    b.append(&label);
    b.add_css_class("stat-card");
    b.add_css_class("card");
    b.upcast()
}

fn disconnected(w: &mut Widgets, why: &str) {
    w.title.set_subtitle("daemon not running");
    w.banner.set_title(&format!("fw-helperd is not available — {why}"));
    w.banner.set_revealed(true);
    w.power.set_label("—");
    w.fan.set_label("—");
}

fn apply(w: &mut Widgets, s: &Snapshot) {
    w.banner.set_revealed(false);
    w.title.set_subtitle("Framework Laptop 13");

    w.power.set_label(
        &s.package_watts
            .map(|v| format!("{v:.1} W"))
            .unwrap_or_else(|| "—".into()),
    );
    w.fan.set_label(&match s.fan_rpm {
        // The EC keeps the fan stopped below roughly 45 C, so zero is a state
        // worth naming rather than showing as "0 rpm".
        Some(0) => "off".to_string(),
        Some(r) => format!("{r} rpm"),
        None => "—".to_string(),
    });

    w.profile
        .set_label(s.platform_profile.as_deref().unwrap_or("—"));

    match (s.battery_percent, s.battery_status.as_deref()) {
        (Some(p), Some(st)) => {
            w.battery.set_subtitle(&format!("{p}% · {st}"));
        }
        (Some(p), None) => w.battery.set_subtitle(&format!("{p}%")),
        _ => w.battery.set_subtitle("—"),
    }

    for sensor in &s.temps {
        let entry = w.sensor_rows.get(&sensor.label).cloned();
        let (row, bar) = match entry {
            Some(pair) => pair,
            None => {
                let bar = gtk::LevelBar::builder()
                    .min_value(0.0)
                    .max_value(sensor.critical.unwrap_or(100.0))
                    .valign(gtk::Align::Center)
                    .width_request(90)
                    .build();
                let row = adw::ActionRow::builder().title(&sensor.label).build();
                row.add_suffix(&bar);
                w.sensors_group.add(&row);
                w.sensor_rows
                    .insert(sensor.label.clone(), (row.clone(), bar.clone()));
                (row, bar)
            }
        };

        let is_control = s.control_sensor.as_deref() == Some(sensor.label.as_str());
        let suffix = if is_control { "  · fan curve input" } else { "" };
        let crit = sensor
            .critical
            .map(|c| format!(" of {c:.0} °C"))
            .unwrap_or_default();
        row.set_subtitle(&format!("{:.1} °C{crit}{suffix}", sensor.celsius));
        bar.set_value(sensor.celsius.clamp(0.0, bar.max_value()));
    }

    for (name, ok, why) in &s.capabilities {
        let row = w.cap_rows.entry(name.clone()).or_insert_with(|| {
            let row = adw::ActionRow::builder().title(name).build();
            w.caps_group.add(&row);
            row
        });
        // Unavailable knobs keep their reason visible; a disabled control with no
        // explanation is the thing ADR 0003 exists to prevent.
        row.set_subtitle(if *ok { "available" } else { why });
        row.set_sensitive(*ok);
    }
}
