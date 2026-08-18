//! `fw-helperctl` — command-line view of what this machine exposes.
//!
//! Prefers `fw-helperd` over D-Bus, which needs no privileges. Falls back to reading
//! sysfs directly when the daemon is not running — useful for debugging and before
//! the daemon is installed, but then package power needs root, because `energy_uj`
//! is 0400 (the PLATYPUS mitigation, ADR 0009).

mod proxy;

use fw_helper_core::{Monitor, Sysfs};
use std::collections::HashMap;
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
    match proxy::connect() {
        Ok((d, version)) => status_via_dbus(&d, version),
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e});\nreading sysfs directly\n");
            status_direct();
        }
    }
}

fn status_via_dbus(d: &proxy::DaemonProxyBlocking<'_>, version: u32) {
    if version != proxy::SUPPORTED_VERSION {
        eprintln!(
            "warning: daemon speaks interface v{version}, this client understands \
             v{}; output may be incomplete\n",
            proxy::SUPPORTED_VERSION
        );
    }

    println!("Capabilities                                    (via fw-helperd)");
    match d.capabilities() {
        Ok(caps) => {
            let mut rows: Vec<_> = caps.into_iter().collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, (ok, reason)) in rows {
                let mark = if ok { "+" } else { "-" };
                let detail = if ok {
                    "available".to_string()
                } else {
                    format!("unavailable — {reason}")
                };
                println!("  {mark} {name:<18} {detail}");
            }
        }
        Err(e) => println!("  <unreadable: {e}>"),
    }

    let t = d.telemetry().unwrap_or_default();
    let crit = d.critical_temperatures().unwrap_or_default();
    render(&t, &crit);
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
        for temp in &s.temps {
            let mark = if control.as_deref() == Some(temp.label.as_str()) {
                "*"
            } else {
                " "
            };
            let crit = temp
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!(
                "  {mark} {:<22} {:>6.1} C   ({crit})",
                temp.label, temp.celsius
            );
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn render(t: &HashMap<String, zbus::zvariant::OwnedValue>, crit: &HashMap<String, f64>) {
    println!("\nTelemetry");
    match t.get("package_watts").and_then(proxy::as_f64) {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable>"),
    }
    if let Some(r) = t.get("fan_rpm").and_then(proxy::as_u64) {
        println!("  fan                {r} rpm");
    }
    if let Some(p) = t.get("platform_profile").and_then(proxy::as_string) {
        println!("  platform profile   {p}");
    }
    if let Some(pct) = t.get("battery_percent").and_then(proxy::as_u64) {
        let st = t
            .get("battery_status")
            .and_then(proxy::as_string)
            .unwrap_or_default();
        println!("  battery            {pct}% ({st})");
    }

    let control = t.get("control_sensor").and_then(proxy::as_string);
    if let Some(temps) = t.get("temps").and_then(proxy::as_temp_map) {
        println!("\nTemperatures");
        let mut rows: Vec<_> = temps.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (label, celsius) in rows {
            let mark = if control.as_deref() == Some(label.as_str()) {
                "*"
            } else {
                " "
            };
            let c = crit
                .get(&label)
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            println!("  {mark} {label:<22} {celsius:>6.1} C   ({c})");
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn watch(secs: u64) {
    let (d, _version) = match proxy::connect() {
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
        let t = d.telemetry().unwrap_or_default();
        let power = t
            .get("package_watts")
            .and_then(proxy::as_f64)
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "-".into());
        let fan = t
            .get("fan_rpm")
            .and_then(proxy::as_u64)
            .map(|r| format!("{r} rpm"))
            .unwrap_or_else(|| "-".into());
        let cpu = t
            .get("control_sensor")
            .and_then(proxy::as_string)
            .and_then(|label| {
                t.get("temps")
                    .and_then(proxy::as_temp_map)
                    .and_then(|m| m.get(&label).copied())
            })
            .map(|c| format!("{c:.1} C"))
            .unwrap_or_else(|| "-".into());
        println!("{i:>5}s  {power:>9}  {fan:>8}  {cpu:>9}");
    }
}
