//! Profiles: the thing a user actually picks.
//!
//! A profile is a PPD profile *plus* the knobs PPD does not manage (ADR 0005). The
//! daemon delegates the PPD axis over D-Bus rather than writing `platform_profile` or
//! EPP itself, because GNOME's power slider is wired to PPD and last-writer-wins
//! against it is the worst bug class in this project.
//!
//! **The layers compose rather than duplicate.** A profile picks a power budget and a
//! curve; the firmware floor, the ceiling and the battery guard still apply on top of
//! that curve exactly as they do to a hand-set duty. Nothing here can make the machine
//! unsafe, which is why the curve values below can be chosen for how they sound.
//!
//! The three defaults are grounded in measurement, not taste: 10 W of PL1 is worth
//! about 12 °C, so the power budget does most of the thermal work and the curve only
//! has to cover what is left. That is also why the quiet curve can afford to be silent
//! to 55 °C — at 15 W the machine sits around 62 °C under sustained load.

use crate::curve::{Curve, Point};

/// The PPD axis. These are PPD's own three profiles and the names it uses on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ppd {
    PowerSaver,
    Balanced,
    Performance,
}

impl Ppd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "power-saver" => Some(Self::PowerSaver),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }
}

/// Why a profile was rejected. Every variant names something the author can fix.
#[derive(Debug, PartialEq)]
pub enum ProfileError {
    EmptyName,
    NameNotSimple(String),
    UnknownPpd(String),
    PowerOutOfRange(u32),
    ChargeOutOfRange(u8),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "a profile needs a name"),
            Self::NameNotSimple(n) => write!(
                f,
                "profile name {n:?} must be lowercase letters, digits and dashes: it is \
                 what a user types and what appears on the D-Bus interface"
            ),
            Self::UnknownPpd(p) => write!(
                f,
                "{p:?} is not a power-profiles-daemon profile; expected power-saver, \
                 balanced or performance"
            ),
            Self::PowerOutOfRange(w) => write!(
                f,
                "{w} W is outside the range a power limit can sensibly take ({}-{} W)",
                crate::power::MIN_WATTS,
                crate::power::FALLBACK_MAX_WATTS
            ),
            Self::ChargeOutOfRange(v) => write!(f, "{v}% is not a usable charge limit"),
        }
    }
}

impl std::error::Error for ProfileError {}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Our name for it, as a user types it.
    pub name: String,
    /// What PPD is asked to switch to.
    pub ppd: Ppd,
    /// Sustained CPU power budget.
    pub pl1_watts: u32,
    /// The fan curve this profile runs.
    pub curve: Curve,
    /// Charge limit, if this profile should set one.
    ///
    /// **`None` in all three built-ins, deliberately.** ADR 0005's sketch included a
    /// charge limit in the profile, but battery longevity is a standing preference, not
    /// a performance choice — someone who caps at 80% to preserve the pack does not
    /// want that undone by asking for more speed for an hour. The field exists so a
    /// user-defined profile can opt in; the defaults leave the setting alone.
    pub charge_limit: Option<u8>,
}

fn curve(points: &[(f64, u8)]) -> Curve {
    Curve::new(
        points
            .iter()
            .map(|&(celsius, duty)| Point { celsius, duty })
            .collect(),
    )
    .expect("built-in curves are valid")
}

impl Profile {
    /// Silent as long as possible. At 15 W the machine settles around 62 °C under
    /// sustained load, so a curve that starts at 55 °C rarely has to do anything.
    pub fn quiet() -> Self {
        Self {
            name: "quiet".into(),
            ppd: Ppd::PowerSaver,
            pl1_watts: 15,
            curve: curve(&[
                (55.0, 0),
                (62.0, 40),
                (70.0, 65),
                (80.0, 92),
                (90.0, 130),
                (100.0, 255),
            ]),
            charge_limit: None,
        }
    }

    /// The default. 20 W lands about 6 °C above quiet, and the curve starts earlier to
    /// absorb it.
    pub fn balanced() -> Self {
        Self {
            name: "balanced".into(),
            ppd: Ppd::Balanced,
            pl1_watts: 20,
            curve: curve(&[
                (50.0, 0),
                (60.0, 45),
                (68.0, 72),
                (78.0, 100),
                (88.0, 145),
                (100.0, 255),
            ]),
            charge_limit: None,
        }
    }

    /// Stock power, and a curve that trades noise for headroom. Measured, the machine
    /// reaches 92.8 °C at 25 W under firmware's own curve; this one is working well
    /// before that.
    pub fn performance() -> Self {
        Self {
            name: "performance".into(),
            ppd: Ppd::Performance,
            pl1_watts: 25,
            curve: curve(&[
                (45.0, 0),
                (55.0, 50),
                (65.0, 85),
                (75.0, 120),
                (85.0, 170),
                (95.0, 255),
            ]),
            charge_limit: None,
        }
    }

    /// The three shipped defaults.
    pub fn built_ins() -> Vec<Self> {
        vec![Self::quiet(), Self::balanced(), Self::performance()]
    }

    /// Validate a profile assembled from somewhere less trustworthy than this file.
    ///
    /// Range-checking the power budget here is a courtesy, not the guarantee:
    /// [`crate::PowerLimit::set`] clamps against the zone's real maximum at apply time,
    /// and it is the one that matters. This exists so a typo in a config file is
    /// reported when the file is read rather than when the profile is first used.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ProfileError::NameNotSimple(self.name.clone()));
        }
        if self.pl1_watts < crate::power::MIN_WATTS
            || self.pl1_watts > crate::power::FALLBACK_MAX_WATTS
        {
            return Err(ProfileError::PowerOutOfRange(self.pl1_watts));
        }
        if let Some(limit) = self.charge_limit {
            if !(crate::charge::MIN_LIMIT..=crate::charge::MAX_LIMIT).contains(&limit) {
                return Err(ProfileError::ChargeOutOfRange(limit));
            }
        }
        Ok(())
    }

    /// The canonical name a PPD profile maps to.
    ///
    /// **User profiles never take part in this mapping**, even one that names the same
    /// PPD profile. When the GNOME slider moves, the machine must land somewhere
    /// predictable; picking between several user profiles that all claim `power-saver`
    /// would be a coin toss the user cannot see. A user profile that *replaces* a
    /// built-in by name is used here, because that is an explicit choice.
    pub fn canonical_name_for(ppd: Ppd) -> &'static str {
        match ppd {
            Ppd::PowerSaver => "quiet",
            Ppd::Balanced => "balanced",
            Ppd::Performance => "performance",
        }
    }

    /// The built-in matching a PPD profile. Callers holding a merged set should prefer
    /// looking up [`Self::canonical_name_for`] in that set.
    pub fn for_ppd(ppd: Ppd) -> Self {
        let name = Self::canonical_name_for(ppd);
        Self::built_ins()
            .into_iter()
            .find(|p| p.name == name)
            .expect("every PPD profile has a built-in")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ppd_profile_maps_to_one_of_ours() {
        // If the GNOME slider can reach a state we have no profile for, the desktop and
        // this daemon disagree about what the machine is doing - the exact failure
        // ADR 0005 exists to prevent.
        for ppd in [Ppd::PowerSaver, Ppd::Balanced, Ppd::Performance] {
            assert_eq!(Profile::for_ppd(ppd).ppd, ppd);
            assert_eq!(Profile::for_ppd(ppd).name, Profile::canonical_name_for(ppd));
        }
    }

    #[test]
    fn ppd_names_round_trip_on_the_wire() {
        for ppd in [Ppd::PowerSaver, Ppd::Balanced, Ppd::Performance] {
            assert_eq!(Ppd::parse(ppd.as_str()), Some(ppd));
        }
        assert_eq!(Ppd::parse("turbo"), None);
    }

    #[test]
    fn rejects_profiles_a_config_file_could_get_wrong() {
        let base = Profile::quiet();
        let with = |f: fn(&mut Profile)| {
            let mut p = base.clone();
            f(&mut p);
            p.validate().unwrap_err()
        };
        assert_eq!(with(|p| p.name.clear()), ProfileError::EmptyName);
        assert!(matches!(
            with(|p| p.name = "My Profile".into()),
            ProfileError::NameNotSimple(_)
        ));
        assert!(matches!(
            with(|p| p.pl1_watts = 2),
            ProfileError::PowerOutOfRange(2)
        ));
        assert!(matches!(
            with(|p| p.charge_limit = Some(5)),
            ProfileError::ChargeOutOfRange(5)
        ));
    }

    #[test]
    fn profiles_are_ordered_by_power_and_by_noise() {
        let (q, b, p) = (
            Profile::quiet(),
            Profile::balanced(),
            Profile::performance(),
        );
        assert!(q.pl1_watts < b.pl1_watts && b.pl1_watts < p.pl1_watts);
        // At any given temperature a hotter-running profile must not ask for less air.
        for t in [50.0, 60.0, 70.0, 80.0, 90.0] {
            assert!(
                q.curve.duty_at(t) <= b.curve.duty_at(t),
                "quiet louder than balanced at {t} C"
            );
            assert!(
                b.curve.duty_at(t) <= p.curve.duty_at(t),
                "balanced louder than performance at {t} C"
            );
        }
    }

    #[test]
    fn the_quiet_profile_is_actually_silent_where_it_matters() {
        // At 15 W the machine sits around 62 C under sustained load. If the quiet curve
        // is already working there, the profile is not quiet.
        let q = Profile::quiet();
        assert_eq!(q.curve.duty_at(50.0), 0);
        assert_eq!(q.curve.duty_at(55.0), 0);
        assert!(q.curve.duty_at(62.0) <= 45);
    }

    #[test]
    fn no_built_in_profile_touches_the_charge_limit() {
        // Battery longevity is a standing preference, not a performance choice.
        for p in Profile::built_ins() {
            assert_eq!(p.charge_limit, None, "{} sets a charge limit", p.name);
        }
    }

    #[test]
    fn power_budgets_stay_inside_the_measured_envelope() {
        for p in Profile::built_ins() {
            assert!(
                p.validate().is_ok(),
                "built-in {} does not validate",
                p.name
            );
        }
    }
}
