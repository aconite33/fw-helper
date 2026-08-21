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
    fw-helperctl charge-limit N  set the battery charge limit (20-100)
    fw-helperctl fan N           pin the fan at duty N (0, or 30-255)
    fw-helperctl fan auto        hand the fan back to the EC
    fw-helperctl fan curve        follow the built-in quiet curve
    fw-helperctl fan curve T:D,.. follow a custom curve, e.g. 55:0,70:65,85:120
    fw-helperctl power-limit N   set the sustained CPU power limit, in watts

Talks to fw-helperd when it is running; otherwise reads sysfs directly, in which
case package power needs root.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("status") | None => status(),
        Some("watch") => watch(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10)),
        Some("charge-limit") => charge_limit(args.get(1).map(String::as_str)),
        Some("fan") => fan(args.get(1).map(String::as_str)),
        Some("power-limit") => power_limit(args.get(1).map(String::as_str)),
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
    if let (Some(pl), Some(max)) = (s.power_limit, s.power_limit_max) {
        let note = if pl >= max { " (stock)" } else { "" };
        println!("  power limit        {pl} W of {max} W max{note}");
    }
    if let Some(r) = s.fan_rpm {
        // Say who is driving. Under manual control the EC's curve is not running, and
        // an RPM with no owner shown is indistinguishable from a stuck fan (ADR 0006).
        match s.fan_mode.as_deref() {
            Some("curve") => {
                let duty = s.fan_duty.unwrap_or(0);
                println!("  fan                {r} rpm  (CURVE, currently duty {duty}/255)");
                let spec: Vec<String> = s
                    .fan_curve
                    .iter()
                    .map(|(c, d)| format!("{c:.0}C:{d}"))
                    .collect();
                println!("                     {}", spec.join("  "));
            }
            Some("manual") => {
                let duty = s.fan_duty.unwrap_or(0);
                let percent = (f64::from(duty) * 100.0 / 255.0).round();
                println!("  fan                {r} rpm  (MANUAL, duty {duty}/255 = {percent:.0}%)");
                println!("                     EC curve is not running; 'fw-helperctl fan auto' restores it");
            }
            _ => println!("  fan                {r} rpm  (EC automatic)"),
        }
        // The floor moves with temperature, so show it: it is the reason a request
        // may be raised, and a number the user can act on.
        match s.fan_floor {
            Some(0) => println!("  fan floor          0/255 (firmware would have the fan off)"),
            Some(255) => println!("  fan floor          255/255 (no temperature readable)"),
            Some(f) => {
                println!("  fan floor          {f}/255 (firmware would be at least this fast)")
            }
            None => {}
        }
    }
    if let Some(p) = &s.platform_profile {
        println!("  platform profile   {p}");
    }
    if let Some(pct) = s.battery_percent {
        let st = s.battery_status.clone().unwrap_or_default();
        println!("  battery            {pct}% ({st})");
    }
    if let Some(limit) = s.charge_limit {
        println!("  charge limit       {limit}%");
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

/// Setting a limit requires the daemon: this is a hardware write, and it goes
/// through polkit inside the daemon rather than being attempted directly.
fn charge_limit(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl charge-limit <20-100>");
        std::process::exit(2);
    };
    let Ok(percent) = arg.parse::<u8>() else {
        eprintln!("not a number: {arg}");
        std::process::exit(2);
    };

    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); setting a charge limit requires it");
            std::process::exit(1);
        }
    };
    match d.set_charge_limit(percent) {
        Ok(()) => println!("charge limit set to {percent}%"),
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Manual fan control. Requires the daemon for the same reason the charge limit does.
///
/// There is no curve behind this: it pins one duty with no temperature feedback at
/// all. `auto` is the way back, and is what should be used to make the fan quieter at
/// idle - the EC can run it slower than this command is allowed to.
fn fan(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl fan <77-255|auto>");
        std::process::exit(2);
    };

    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); fan control requires it");
            std::process::exit(1);
        }
    };

    if arg == "curve" {
        set_curve(&d, None);
    }
    if let Some(spec) = arg.strip_prefix("curve ") {
        set_curve(&d, Some(spec));
    }
    if arg == "auto" {
        match d.set_fan_auto() {
            Ok(()) => println!("fan returned to EC control"),
            Err(e) => {
                eprintln!("failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let Ok(duty) = arg.parse::<u8>() else {
        eprintln!("not a duty or 'auto': {arg}");
        std::process::exit(2);
    };
    match d.set_fan_duty(duty) {
        // Report what the EC took, not what was asked for: it quantizes to whole
        // percent, so these differ by a count or two and the difference is real.
        Ok(settled) => {
            let percent = (f64::from(settled) * 100.0 / 255.0).round();
            println!("fan pinned at duty {settled}/255 ({percent:.0}%)");
            // Two quite different reasons the number can come back changed, and the
            // user needs to be able to tell them apart.
            if settled > duty.saturating_add(3) {
                println!("  raised from {duty}: the firmware floor at the current");
                println!("  temperature is higher, and we never run slower than the EC would");
            } else if settled != duty {
                println!("  (asked for {duty}; the EC stores whole percent)");
            }
            println!("  'fw-helperctl fan auto' returns control to the EC");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Send a curve to the daemon. `None` means the built-in one.
///
/// The daemon validates: points must ascend in temperature, must not fall in duty, and
/// must not ask for a duty that cannot turn the fan. Its message says which rule was
/// broken, so there is nothing to duplicate here.
fn set_curve(d: &DaemonProxyBlocking<'_>, spec: Option<&str>) -> ! {
    let points: Vec<(f64, u8)> = match spec {
        None => Vec::new(),
        Some(s) => match parse_curve(s) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                eprintln!("expected temperature:duty pairs, e.g. 55:0,70:65,85:120");
                std::process::exit(2);
            }
        },
    };
    // An empty list means "use the built-in", resolved here rather than in the daemon
    // so the wire stays a plain list of points.
    let points = if points.is_empty() {
        vec![
            (55.0, 0u8),
            (62.0, 40),
            (70.0, 65),
            (80.0, 92),
            (90.0, 130),
            (100.0, 255),
        ]
    } else {
        points
    };

    match d.set_fan_curve(points.clone()) {
        Ok(settled) => {
            println!("following a fan curve; currently duty {settled}/255");
            for (c, duty) in &points {
                println!("  {c:>5.1} C -> {duty:>3}/255");
            }
            println!("  the firmware floor still applies on top of this");
            println!("  'fw-helperctl fan auto' returns control to the EC");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_curve(spec: &str) -> Result<Vec<(f64, u8)>, String> {
    spec.split(',')
        .map(|pair| {
            let (t, d) = pair
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("'{pair}' is not temperature:duty"))?;
            let celsius: f64 = t
                .trim()
                .parse()
                .map_err(|_| format!("'{t}' is not a temperature"))?;
            let duty: u8 = d
                .trim()
                .parse()
                .map_err(|_| format!("'{d}' is not a duty"))?;
            Ok((celsius, duty))
        })
        .collect()
}

/// Set the sustained CPU power limit. Requires the daemon, like every hardware write.
fn power_limit(arg: Option<&str>) {
    let Some(arg) = arg else {
        eprintln!("usage: fw-helperctl power-limit <watts>");
        std::process::exit(2);
    };
    let Ok(watts) = arg.parse::<u32>() else {
        eprintln!("not a number: {arg}");
        std::process::exit(2);
    };
    let (d, _) = match connect() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fw-helperd unavailable ({e}); setting a power limit requires it");
            std::process::exit(1);
        }
    };
    match d.set_power_limit(watts) {
        Ok(()) => {
            println!("power limit set to {watts} W");
            // Measured, and the reason a user who checks immediately will think it did
            // not work: the limit averages over ~32 s.
            println!("  the limit averages over ~32 s, so power readings take that long to settle");
            println!("  roughly 10 W is worth 12 C of sustained CPU temperature");
        }
        Err(e) => {
            eprintln!("failed: {e}");
            std::process::exit(1);
        }
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
