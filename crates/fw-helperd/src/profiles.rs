//! User profiles from `/etc/fw-helper/profiles.d/`.
//!
//! Parsing lives in the daemon rather than in `fw-helper-core`, which stays free of
//! config handling (ADR 0010). The format is the same deliberately trivial `key=value`
//! the state file uses: this describes four fields and a list of points, not a
//! configuration language.
//!
//! ```text
//! # /etc/fw-helper/profiles.d/silent.conf
//! name         = silent
//! ppd          = power-saver
//! pl1_watts    = 12
//! curve        = 55:0,65:40,75:70,85:110,95:255
//! charge_limit = 80          # optional; omitted means "leave it alone"
//! ```
//!
//! **A file naming an existing profile replaces it.** That is how the shipped defaults
//! are customised — write a `quiet.conf` with `name = quiet` and it becomes the quiet
//! profile, including for the GNOME slider, because replacing a built-in by name is an
//! explicit choice. A profile under any other name is selectable by hand but never
//! auto-applied when the slider moves: choosing between several user profiles that all
//! claim `power-saver` would be a coin toss the user cannot see.
//!
//! One bad file does not sink the rest. Each is reported by name and skipped, because
//! losing every profile over one typo is a worse failure than running without one.

use fw_helper_core::{Curve, Point, Ppd, Profile};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROFILE_DIR: &str = "/etc/fw-helper/profiles.d";

/// Load the built-ins, then let user files replace or extend them.
pub fn load() -> Vec<Profile> {
    load_from(Path::new(PROFILE_DIR))
}

fn load_from(dir: &Path) -> Vec<Profile> {
    let mut profiles = Profile::built_ins();
    let Ok(entries) = fs::read_dir(dir) else {
        return profiles; // No directory is the normal case, not an error.
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "conf"))
        .collect();
    // Deterministic order, so two files defining the same name resolve the same way on
    // every boot rather than by directory iteration order.
    files.sort();

    for path in files {
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("profile {}: cannot read ({e})", path.display());
                continue;
            }
        };
        match parse(&text) {
            Ok(profile) => {
                let replacing = profiles.iter().position(|p| p.name == profile.name);
                match replacing {
                    Some(i) => {
                        eprintln!("profile {}: replaces the built-in", profile.name);
                        profiles[i] = profile;
                    }
                    None => {
                        eprintln!("profile {}: loaded", profile.name);
                        profiles.push(profile);
                    }
                }
            }
            Err(e) => eprintln!("profile {}: ignored, {e}", path.display()),
        }
    }
    profiles
}

/// Parse one profile file.
pub fn parse(text: &str) -> Result<Profile, String> {
    let mut name = None;
    let mut ppd = None;
    let mut watts = None;
    let mut curve = None;
    let mut charge = None;

    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", n + 1));
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "name" => name = Some(value.to_string()),
            "ppd" => {
                ppd = Some(
                    Ppd::parse(value)
                        .ok_or_else(|| format!("line {}: unknown ppd {value:?}", n + 1))?,
                )
            }
            "pl1_watts" => {
                watts = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("line {}: {value:?} is not a number", n + 1))?,
                )
            }
            "curve" => {
                curve = Some(parse_curve(value).map_err(|e| format!("line {}: {e}", n + 1))?)
            }
            "charge_limit" => {
                charge = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("line {}: {value:?} is not a percentage", n + 1))?,
                )
            }
            other => return Err(format!("line {}: unknown key {other:?}", n + 1)),
        }
    }

    let profile = Profile {
        name: name.ok_or("no name")?,
        ppd: ppd.ok_or("no ppd (power-saver, balanced or performance)")?,
        pl1_watts: watts.ok_or("no pl1_watts")?,
        curve: curve.ok_or("no curve")?,
        charge_limit: charge,
    };
    profile.validate().map_err(|e| e.to_string())?;
    Ok(profile)
}

fn parse_curve(spec: &str) -> Result<Curve, String> {
    let points = spec
        .split(',')
        .map(|pair| {
            let (t, d) = pair
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("{pair:?} is not temperature:duty"))?;
            Ok(Point {
                celsius: t
                    .trim()
                    .parse()
                    .map_err(|_| format!("{t:?} is not a temperature"))?,
                duty: d
                    .trim()
                    .parse()
                    .map_err(|_| format!("{d:?} is not a duty"))?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Curve::new(points).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "
# a comment
name         = silent
ppd          = power-saver
pl1_watts    = 12
curve        = 55:0,65:40,75:70,85:110,95:255
charge_limit = 80   # trailing comment
";

    #[test]
    fn parses_a_complete_profile() {
        let p = parse(GOOD).unwrap();
        assert_eq!(p.name, "silent");
        assert_eq!(p.ppd, Ppd::PowerSaver);
        assert_eq!(p.pl1_watts, 12);
        assert_eq!(p.charge_limit, Some(80));
        assert_eq!(p.curve.duty_at(55.0), 0);
        assert_eq!(p.curve.duty_at(95.0), 255);
    }

    #[test]
    fn a_missing_charge_limit_means_leave_it_alone() {
        let text = GOOD.replace("charge_limit = 80   # trailing comment", "");
        assert_eq!(parse(&text).unwrap().charge_limit, None);
    }

    #[test]
    fn every_rejection_says_which_line_and_why() {
        for (text, want) in [
            ("name = x\nppd = turbo\n", "unknown ppd"),
            (
                "name = x\nppd = balanced\npl1_watts = lots\n",
                "not a number",
            ),
            ("name = x\nnonsense\n", "expected key = value"),
            ("name = x\ngovernor = ondemand\n", "unknown key"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(want), "{e:?} should mention {want:?}");
            assert!(e.starts_with("line "), "{e:?} should name the line");
        }
    }

    #[test]
    fn an_incomplete_profile_says_what_is_missing() {
        assert!(parse("name = x\n").unwrap_err().contains("no ppd"));
        assert!(parse("ppd = balanced\n").unwrap_err().contains("no name"));
    }

    #[test]
    fn a_curve_that_falls_as_it_heats_is_rejected_with_the_curve_rules() {
        // The curve's own validation, surfaced rather than duplicated.
        let text = "name = x\nppd = balanced\npl1_watts = 15\ncurve = 50:200,60:40\n";
        let e = parse(text).unwrap_err();
        assert!(e.contains("less airflow"), "got {e:?}");
    }

    #[test]
    fn a_name_that_is_not_typeable_is_rejected() {
        let text = "name = My Profile\nppd = balanced\npl1_watts = 15\ncurve = 50:0,60:40\n";
        assert!(parse(text).unwrap_err().contains("lowercase"));
    }

    fn write(dir: &Path, file: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), body).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fw-profiles-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn no_directory_leaves_the_built_ins_alone() {
        let profiles = load_from(&tmpdir("absent"));
        assert_eq!(profiles.len(), Profile::built_ins().len());
    }

    #[test]
    fn a_user_file_can_add_a_profile() {
        let d = tmpdir("add");
        write(&d, "silent.conf", GOOD);
        let profiles = load_from(&d);
        assert_eq!(profiles.len(), Profile::built_ins().len() + 1);
        assert!(profiles.iter().any(|p| p.name == "silent"));
    }

    #[test]
    fn a_user_file_replaces_a_built_in_of_the_same_name() {
        let d = tmpdir("replace");
        write(
            &d,
            "quiet.conf",
            "name = quiet\nppd = power-saver\npl1_watts = 10\ncurve = 60:0,70:50\n",
        );
        let profiles = load_from(&d);
        assert_eq!(profiles.len(), Profile::built_ins().len());
        let quiet = profiles.iter().find(|p| p.name == "quiet").unwrap();
        assert_eq!(quiet.pl1_watts, 10, "the user's version should win");
    }

    #[test]
    fn one_bad_file_does_not_take_the_others_with_it() {
        // Losing every profile over one typo is worse than running without one.
        let d = tmpdir("mixed");
        write(&d, "broken.conf", "name = x\nppd = turbo\n");
        write(&d, "silent.conf", GOOD);
        let profiles = load_from(&d);
        assert!(profiles.iter().any(|p| p.name == "silent"));
        assert!(!profiles.iter().any(|p| p.name == "x"));
    }

    #[test]
    fn the_shipped_example_actually_parses() {
        // It is documentation, and documentation rots. If the format changes and this
        // file is not updated, the first person to follow it hits an error the project
        // told them to expect to work.
        let text = include_str!("../../../data/example-profile.conf");
        let p = parse(text).expect("data/example-profile.conf must parse");
        assert_eq!(p.name, "silent");
        assert_eq!(
            p.charge_limit, None,
            "the example's charge_limit is commented out and must stay optional"
        );
    }

    #[test]
    fn files_without_the_conf_extension_are_ignored() {
        // Editor backups and .rpmnew files are not profiles.
        let d = tmpdir("ext");
        write(&d, "silent.conf.bak", GOOD);
        assert_eq!(load_from(&d).len(), Profile::built_ins().len());
    }
}
