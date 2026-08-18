//! `fw-helperctl` — command-line view of what this machine exposes.
//!
//! At M1a this reads sysfs directly, so it needs root for package power
//! (`energy_uj` is 0400 — the PLATYPUS mitigation, see ADR 0009). From M1b it
//! talks to `fw-helperd` over D-Bus instead and needs no privileges at all.

use fw_helper_core::{Monitor, Sysfs};
use std::thread::sleep;
use std::time::Duration;

const USAGE: &str = "\
fw-helperctl — Framework laptop firmware control

USAGE:
    fw-helperctl status          capabilities and one telemetry sample
    fw-helperctl watch [secs]    live telemetry, 1 Hz (default 10s)

NOTE:
    Package power needs root; everything else works unprivileged.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mon = Monitor::new(Sysfs::default());

    match args.first().map(String::as_str) {
        Some("status") | None => status(&mut mon),
        Some("watch") => {
            let secs = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            watch(&mut mon, secs);
        }
        Some("-h") | Some("--help") => print!("{USAGE}"),
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn status(mon: &mut Monitor) {
    println!("Capabilities");
    for (name, cap) in mon.capabilities().summary() {
        let mark = if cap.is_available() { "+" } else { "-" };
        println!("  {mark} {name:<18} {cap}");
    }

    // The first sample only establishes a reference point for the energy counter;
    // a second one a beat later is what actually yields watts.
    mon.sample();
    sleep(Duration::from_millis(1000));
    let t = mon.sample();

    println!("\nTelemetry");
    match t.package_watts {
        Some(w) => println!("  package power      {w:.1} W"),
        None => println!("  package power      <unavailable — run as root?>"),
    }
    match t.fan_rpm {
        Some(r) => println!("  fan                {r} rpm"),
        None => println!("  fan                <unavailable>"),
    }
    if let Some(p) = &t.platform_profile {
        println!("  platform profile   {p}");
    }
    if let (Some(pct), Some(st)) = (t.battery_percent, &t.battery_status) {
        println!("  battery            {pct}% ({st})");
    }

    if !t.temps.is_empty() {
        println!("\nTemperatures");
        for temp in &t.temps {
            let crit = temp
                .critical
                .map(|c| format!("crit {c:.1}"))
                .unwrap_or_else(|| "crit unknown".into());
            let marker = if t.control_temp().map(|c| c.label.as_str()) == Some(temp.label.as_str())
            {
                "*"
            } else {
                " "
            };
            println!("  {marker} {:<22} {:>6.1} C   ({crit})", temp.label, temp.celsius);
        }
        println!("\n  * = sensor a fan curve would follow");
    }
}

fn watch(mon: &mut Monitor, secs: u64) {
    println!("{:>6}  {:>9}  {:>8}  {:>9}", "t", "power", "fan", "cpu");
    mon.sample();
    for t in 1..=secs {
        sleep(Duration::from_secs(1));
        let s = mon.sample();
        let power = s
            .package_watts
            .map(|w| format!("{w:.1} W"))
            .unwrap_or_else(|| "-".into());
        let fan = s
            .fan_rpm
            .map(|r| format!("{r} rpm"))
            .unwrap_or_else(|| "-".into());
        let cpu = s
            .control_temp()
            .map(|c| format!("{:.1} C", c.celsius))
            .unwrap_or_else(|| "-".into());
        println!("{t:>5}s  {power:>9}  {fan:>8}  {cpu:>9}");
    }
}
