//! Availability levels (how quickly a file must come back) and account speed classes.
//! Same five levels and four speed classes as the Storage Distributor web app.

use serde::{Deserialize, Serialize};

/// How fast an account returns data, measured or set by the user. Ordered slowest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedClass {
    Archive,
    Slow,
    Standard,
    Fast,
}

impl SpeedClass {
    /// 1 (archive) to 4 (fast).
    pub const fn rank(self) -> u8 {
        match self {
            SpeedClass::Archive => 1,
            SpeedClass::Slow => 2,
            SpeedClass::Standard => 3,
            SpeedClass::Fast => 4,
        }
    }
}

/// How quickly a stored file must be readable again. Ordered slowest first, so
/// `Availability::Instant > Availability::Archive`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// Backups and cold data.
    Archive,
    /// Rarely opened.
    Slow,
    /// Everyday files.
    #[default]
    Standard,
    /// Quick downloads.
    Fast,
    /// Opens right away (catalog, smart previews).
    Instant,
}

impl Availability {
    /// Fastest first, as the levels are offered to the user.
    pub const ALL: [Availability; 5] = [Availability::Instant, Availability::Fast, Availability::Standard, Availability::Slow, Availability::Archive];

    /// 1 (archive) to 5 (instant), the numbers the shared format stores.
    pub const fn level(self) -> u8 {
        match self {
            Availability::Archive => 1,
            Availability::Slow => 2,
            Availability::Standard => 3,
            Availability::Fast => 4,
            Availability::Instant => 5,
        }
    }

    pub const fn from_level(level: u8) -> Option<Availability> {
        match level {
            1 => Some(Availability::Archive),
            2 => Some(Availability::Slow),
            3 => Some(Availability::Standard),
            4 => Some(Availability::Fast),
            5 => Some(Availability::Instant),
            _ => None,
        }
    }

    /// The next slower level, if any.
    pub const fn slower(self) -> Option<Availability> {
        Availability::from_level(self.level() - 1)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Availability::Archive => "Archive",
            Availability::Slow => "Slow is OK",
            Availability::Standard => "Standard",
            Availability::Fast => "Fast",
            Availability::Instant => "Instant",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Availability::Archive => "Backups and cold data",
            Availability::Slow => "Rarely opened",
            Availability::Standard => "Everyday files",
            Availability::Fast => "Quick downloads",
            Availability::Instant => "Opens right away",
        }
    }

    /// The slowest account allowed to hold a file at this level.
    pub const fn min_speed(self) -> SpeedClass {
        match self {
            Availability::Archive => SpeedClass::Archive,
            Availability::Slow => SpeedClass::Slow,
            Availability::Standard | Availability::Fast => SpeedClass::Standard,
            Availability::Instant => SpeedClass::Fast,
        }
    }

    /// The speed that fits best; faster accounts are "overkill" and score lower, so fast space
    /// stays free for files that need it.
    pub const fn ideal_speed(self) -> SpeedClass {
        match self {
            Availability::Archive => SpeedClass::Archive,
            Availability::Slow => SpeedClass::Slow,
            Availability::Standard => SpeedClass::Standard,
            Availability::Fast | Availability::Instant => SpeedClass::Fast,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_round_trip_and_order() {
        for a in Availability::ALL {
            assert_eq!(Availability::from_level(a.level()), Some(a));
            assert!(a.min_speed() <= a.ideal_speed());
        }
        assert_eq!(Availability::from_level(0), None);
        assert_eq!(Availability::from_level(6), None);
        assert_eq!(Availability::Archive.slower(), None);
        assert_eq!(Availability::Instant.slower(), Some(Availability::Fast));
        assert!(Availability::Instant > Availability::Archive);
    }

    #[test]
    fn serializes_as_lowercase_names() {
        assert_eq!(serde_json::to_string(&Availability::Instant).ok().as_deref(), Some("\"instant\""));
        assert_eq!(serde_json::to_string(&SpeedClass::Standard).ok().as_deref(), Some("\"standard\""));
    }
}
