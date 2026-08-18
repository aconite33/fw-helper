//! `fw-helperctl` — command-line view of what this machine exposes.
//!
//! Prefers `fw-helperd` over D-Bus, which needs no privileges. Falls back to reading
//! sysfs directly when the daemon is absent — useful for debugging and before the
//! daemon is installed, but then package power needs root, because `energy_uj` is
//! 0400 (the PLATYPUS mitigation, ADR 0009).

use fw_helper_client::{connect, DaemonProxyBlocking, Snapshot, SUPPORTED_VERSION};
use fw_helper_core::{Monitor, Sysfs};
use std::thread::sleep;
use std::time::Duration;

const USAGE: &str = "\
fw-helperctl — Framework laptop firmware control

USAGE:
    fw-helperctl status          capabilities and one telemetry sample
    fw-helperctl watch [secs]    live telemetry, 1 Hz (default 10s)

Talks to fw-helperd when it is running; otherwise reads sysfs directly, in which
case package power needs root.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("status") | None => status(),
        Some("watch") => watch(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10)),
        Some("-h") | Some("--help") => print!("{USAGE}"),
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn status() {
    match connect() {
        Ok((d, version)) => status_via_dbus(&d, version),
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e});\nreading sysfs directly\n");
            status_direct();
        }
    }
}

fn status_via_dbus(d: &DaemonProxyBlocking<'_>, version: u32) {
    if version != SUPPORTED_VERSION {
        eprintln!(
            "warning: daemon speaks interface v{version}, this client understands \
             v{SUPPORTED_VERSION}; output may be incomplete\n"
        );
    }
    let s = match Snapshot::fetch(d) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read telemetry: {e}");
            std::process::exit(1);
        }
    };

    println!("Capabilities                                    (via fw-helperd)");
    for (name, ok, why) in &s.capabilities {
        let mark = if *ok { "+" } else { "-" };
        let detail = if *ok {
            "available".to_string()
        } else {
            format!("unavailable — {why}")
        };
        println!("  {mark} {name:<18} {detail}");
    }

    println!("\nTelemetry");
    match s.package_watts {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable>"),
    }
    if let Some(r) = s.fan_rpm {
        println!("  fan                {r} rpm");
    }
    if let Some(p) = &s.platform_profile {
        println!("  platform profile   {p}");
    }
    if let Some(pct) = s.battery_percent {
        let st = s.battery_status.clone().unwrap_or_default();
        println!("  battery            {pct}% ({st})");
    }

    if !s.temps.is_empty() {
        println!("\nTemperatures");
        for t in &s.temps {
            let mark = if s.control_sensor.as_deref() == Some(t.label.as_str()) {
                "*"
            } else {
                " "
            };
            let crit = t
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!("  {mark} {:<22} {:>6.1} C   ({crit})", t.label, t.celsius);
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn status_direct() {
    let mut mon = Monitor::new(Sysfs::default());
    println!("Capabilities                                    (direct sysfs)");
    for (name, cap) in mon.capabilities().summary() {
        let mark = if cap.is_available() { "+" } else { "-" };
        println!("  {mark} {name:<18} {cap}");
    }
    // The first sample only establishes the energy reference point.
    mon.sample();
    sleep(Duration::from_secs(1));
    let s = mon.sample();

    println!("\nTelemetry");
    match s.package_watts {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable — run as root?>"),
    }
    if let Some(r) = s.fan_rpm {
        println!("  fan                {r} rpm");
    }
    if let Some(p) = &s.platform_profile {
        println!("  platform profile   {p}");
    }
    if let (Some(pct), Some(st)) = (s.battery_percent, &s.battery_status) {
        println!("  battery            {pct}% ({st})");
    }
    if !s.temps.is_empty() {
        println!("\nTemperatures");
        let control = s.control_temp().map(|c| c.label.clone());
        for t in &s.temps {
            let mark = if control.as_deref() == Some(t.label.as_str()) {
                "*"
            } else {
                " "
            };
            let crit = t
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!("  {mark} {:<22} {:>6.1} C   ({crit})", t.label, t.celsius);
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn watch(secs: u64) {
    let (d, _version) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); watch requires the daemon");
            eprintln!("hint: `fw-helperctl status` still works by reading sysfs directly");
            std::process::exit(1);
        }
    };
    println!("{:>6}  {:>9}  {:>8}  {:>9}", "t", "power", "fan", "cpu");
    for i in 1..=secs {
        sleep(Duration::from_secs(1));
        let Ok(s) = Snapshot::fetch(&d) else { continue };
        let power = s
            .package_watts
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "-".into());
        let fan = s
            .fan_rpm
            .map(|r| format!("{r} rpm"))
            .unwrap_or_else(|| "-".into());
        let cpu = s
            .control_sensor
            .as_ref()
            .and_then(|label| s.temps.iter().find(|t| &t.label == label))
            .map(|t| format!("{:.1} C", t.celsius))
            .unwrap_or_else(|| "-".into());
        println!("{i:>5}s  {power:>9}  {fan:>8}  {cpu:>9}");
    }
}
