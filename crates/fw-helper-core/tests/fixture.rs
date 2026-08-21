//! End-to-end tests against a synthetic sysfs tree.
//!
//! This is the payoff from ADR 0004's rooted-path design: capability probing and
//! telemetry are exercised with no hardware, no root, and no network — so they run
//! in CI. The fixture mirrors the real values captured in docs/hardware-baseline.md.

use fw_helper_core::fan::MIN_TAKEOVER_DUTY;
use fw_helper_core::{Capabilities, FanControl, FanError, FanMode, Monitor, Sysfs};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "fw-helper-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        let _ = fs::remove_dir_all(&root);
        Self { root }
    }

    fn write(&self, rel: &str, contents: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    fn sysfs(&self) -> Sysfs {
        Sysfs::new(&self.root)
    }

    /// A Framework 13 Pro as actually observed: EC hwmon present, RAPL present,
    /// charge control declined to bind but the override parameter exists.
    fn framework_13(tag: &str) -> Self {
        let f = Fixture::new(tag);
        // hwmon3 is the battery, hwmon7 the EC — deliberately not the real indices,
        // to prove lookup is by name and not by number.
        f.write("sys/class/hwmon/hwmon3/name", "BAT1\n");
        f.write("sys/class/hwmon/hwmon7/name", "cros_ec\n");
        f.write("sys/class/hwmon/hwmon7/pwm1_enable", "2\n");
        f.write("sys/class/hwmon/hwmon7/pwm1", "0\n");
        f.write("sys/class/hwmon/hwmon7/fan1_input", "2925\n");
        f.write("sys/class/hwmon/hwmon7/temp1_input", "36850\n");
        f.write("sys/class/hwmon/hwmon7/temp1_label", "local_f75397@4c\n");
        f.write("sys/class/hwmon/hwmon7/temp1_crit", "87850\n");
        f.write("sys/class/hwmon/hwmon7/temp5_input", "64800\n");
        f.write("sys/class/hwmon/hwmon7/temp5_label", "peci-temp\n");
        f.write("sys/class/hwmon/hwmon7/temp5_crit", "119850\n");
        // the unset threshold that would otherwise poison fan safety
        f.write("sys/class/hwmon/hwmon7/temp5_max", "-273150\n");

        let rapl = "sys/class/powercap/intel-rapl-mmio:0";
        f.write(&format!("{rapl}/name"), "package-0\n");
        f.write(&format!("{rapl}/constraint_0_power_limit_uw"), "25000000\n");
        f.write(&format!("{rapl}/constraint_0_max_power_uw"), "25000000\n");
        f.write(&format!("{rapl}/energy_uj"), "1000000\n");
        f.write(&format!("{rapl}/max_energy_range_uj"), "262143328850\n");

        f.write("sys/firmware/acpi/platform_profile", "balanced\n");
        f.write("sys/class/power_supply/BAT1/capacity", "100\n");
        f.write("sys/class/power_supply/BAT1/status", "Not charging\n");
        f.write(
            "sys/module/cros_charge_control/parameters/probe_with_fwk_charge_control",
            "N\n",
        );
        f
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn probes_a_framework_13_correctly() {
    let f = Fixture::framework_13("probe");
    let caps = Capabilities::probe(&f.sysfs());

    assert!(caps.fan_control.is_available());
    assert!(caps.power_limit.is_available());
    assert!(caps.platform_profile.is_available());
    assert!(caps.package_power.is_available());

    // Charge control must be reported unavailable *with the actionable reason*,
    // not merely absent — this is the ADR 0008 case.
    assert!(!caps.charge_limit.is_available());
    assert!(
        format!("{}", caps.charge_limit).contains("probe_with_fwk_charge_control"),
        "reason should tell the user how to fix it, got: {}",
        caps.charge_limit
    );
}

#[test]
fn finds_hwmon_by_name_not_index() {
    let f = Fixture::framework_13("hwmon");
    let caps = Capabilities::probe(&f.sysfs());
    assert_eq!(caps.ec_hwmon.as_deref(), Some("sys/class/hwmon/hwmon7"));
}

#[test]
fn charge_limit_available_when_threshold_exists() {
    let f = Fixture::framework_13("charge-ok");
    f.write(
        "sys/class/power_supply/BAT1/charge_control_end_threshold",
        "80\n",
    );
    assert!(Capabilities::probe(&f.sysfs()).charge_limit.is_available());
}

#[test]
fn degrades_gracefully_with_no_hardware() {
    let f = Fixture::new("empty");
    f.write("sys/placeholder", "\n"); // root exists but is otherwise bare
    let caps = Capabilities::probe(&f.sysfs());

    // Nothing panics, and every knob explains itself.
    for (name, cap) in caps.summary() {
        assert!(!cap.is_available(), "{name} should be unavailable");
        assert!(!format!("{cap}").is_empty(), "{name} must carry a reason");
    }
}

#[test]
fn samples_telemetry_and_picks_the_control_sensor() {
    let f = Fixture::framework_13("telemetry");
    let mut mon = Monitor::new(f.sysfs());
    let t = mon.sample();

    assert_eq!(t.fan_rpm, Some(2925));
    assert_eq!(t.battery_percent, Some(100));
    assert_eq!(t.platform_profile.as_deref(), Some("balanced"));
    assert_eq!(t.temps.len(), 2);

    let ctrl = t.control_temp().expect("a control sensor");
    assert_eq!(ctrl.label, "peci-temp");
    assert_eq!(ctrl.celsius, 64.8);
    assert_eq!(ctrl.critical, Some(119.85));

    // First sample has no prior reference, so power is not yet derivable.
    assert_eq!(t.package_watts, None);
}

/// Read a fixture file back as a trimmed string — asserts on what actually reached
/// "hardware", rather than on what the API claims it did.
fn raw(f: &Fixture, rel: &str) -> String {
    fs::read_to_string(f.root.join(rel))
        .unwrap()
        .trim()
        .to_string()
}

const EC: &str = "sys/class/hwmon/hwmon7";

#[test]
fn takes_manual_control_and_hands_it_back() {
    let f = Fixture::framework_13("fan-lease");
    let fs = f.sysfs();
    let fan = FanControl::probe(&fs).expect("fixture has a cros_ec hwmon");

    assert_eq!(fan.mode().unwrap(), FanMode::Auto);

    fan.take_manual(200).unwrap();
    assert_eq!(fan.mode().unwrap(), FanMode::Manual);
    assert_eq!(fan.duty().unwrap(), 200);
    // The mode switch and the duty both landed in sysfs, not just in our own state.
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "1");
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "200");

    fan.set_duty(120).unwrap();
    assert_eq!(fan.duty().unwrap(), 120);

    fan.release().unwrap();
    assert_eq!(fan.mode().unwrap(), FanMode::Auto);
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "2");
}

#[test]
fn duty_is_attempted_before_the_mode_switch() {
    // Real hardware refuses this pre-write with EOPNOTSUPP (measured 2026-08-21), so
    // on the reference machine it changes nothing and the takeover window stays open.
    // The fixture is writable in either mode, so what this pins down is that we still
    // *try* — the ordering is a genuine safety gain on any EC that permits it, and a
    // later refactor must not quietly drop it.
    let f = Fixture::framework_13("fan-order");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "0");
    fan.take_manual(180).unwrap();
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "180");
}

#[test]
fn refuses_to_set_duty_while_the_ec_owns_the_fan() {
    let f = Fixture::framework_13("fan-auto");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    // Would be silently ignored by real hardware, so it must be an error here.
    assert!(matches!(
        fan.set_duty(200),
        Err(FanError::NotUnderManualControl(FanMode::Auto))
    ));
    assert_eq!(
        raw(&f, &format!("{EC}/pwm1")),
        "0",
        "nothing should be written"
    );
}

#[test]
fn release_is_idempotent() {
    // Every exit path calls this, including ones that run after another already did.
    let f = Fixture::framework_13("fan-idempotent");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    fan.release().unwrap();
    fan.release().unwrap();
    assert!(fan.release_best_effort());
    assert_eq!(fan.mode().unwrap(), FanMode::Auto);
}

#[test]
fn a_duty_that_cannot_turn_the_fan_never_reaches_hardware() {
    let f = Fixture::framework_13("fan-unsafe");
    let fs = f.sysfs();
    let fan = FanControl::new(&fs, EC);

    assert!(matches!(
        fan.take_manual(MIN_TAKEOVER_DUTY - 1),
        Err(FanError::DutyCannotTurnFan(_))
    ));
    // The refusal must happen before any write: the fan is still the EC's.
    assert_eq!(raw(&f, &format!("{EC}/pwm1_enable")), "2");
    assert_eq!(raw(&f, &format!("{EC}/pwm1")), "0");
}
