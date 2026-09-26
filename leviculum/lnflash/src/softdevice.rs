//! The SoftDevice version, and the constraint our firmware places on it.
//!
//! This is the one precondition that exists today
//! (docs/src/concepts/lnode-flashing.md, "Four axes"). It is a precondition
//! rather than a board fact because it changes underneath a board: the same
//! T114 reads 6.1.1 from the factory and 7.3.0 after a remedy.
//!
//! There are two independent ways to read it, and a tool that can do both is
//! immune to a bootloader too old to report the line at all:
//!
//! 1. the `SoftDevice:` line in `INFO_UF2.TXT` (see [`crate::infouf2`]), and
//! 2. the version word the SoftDevice itself keeps at [`VERSION_WORD_ADDR`],
//!    which `CURRENT.UF2` dumps and [`installed_version`] reads back.

use std::fmt;
use std::str::FromStr;

use crate::uf2::Image;

/// Absolute flash address of the SoftDevice info-struct version word:
/// `nrf_sdm.h` puts the struct `0x2000` above the MBR and the word `0x14`
/// into it. Inside the range `CURRENT.UF2` covers, so it is readable
/// without writing anything.
pub const VERSION_WORD_ADDR: u32 = 0x3014;

/// A SoftDevice version. Nordic encodes it as one decimal-packed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub bugfix: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, bugfix: u32) -> Self {
        Self {
            major,
            minor,
            bugfix,
        }
    }

    /// Decode `major * 1_000_000 + minor * 1_000 + bugfix`.
    pub const fn from_word(word: u32) -> Self {
        Self {
            major: word / 1_000_000,
            minor: (word / 1_000) % 1_000,
            bugfix: word % 1_000,
        }
    }

    pub const fn to_word(self) -> u32 {
        self.major * 1_000_000 + self.minor * 1_000 + self.bugfix
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        s.parse()
    }
}

impl FromStr for Version {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split('.');
        let mut next = || -> Result<u32, Error> {
            parts
                .next()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| Error::BadVersion(s.to_string()))
        };
        let version = Self::new(next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(Error::BadVersion(s.to_string()));
        }
        Ok(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.bugfix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("{0:?} is not a major.minor.bugfix version")]
    BadVersion(String),
    #[error("{0:?} is not a version comparator")]
    BadComparator(String),
    #[error("an empty version constraint")]
    EmptyConstraint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Comparator {
    op: Op,
    version: Version,
}

impl Comparator {
    fn matches(&self, v: Version) -> bool {
        match self.op {
            Op::Lt => v < self.version,
            Op::Le => v <= self.version,
            Op::Eq => v == self.version,
            Op::Ge => v >= self.version,
            Op::Gt => v > self.version,
        }
    }
}

impl fmt::Display for Comparator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = match self.op {
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Eq => "=",
            Op::Ge => ">=",
            Op::Gt => ">",
        };
        write!(f, "{op}{}", self.version)
    }
}

/// A comma-separated conjunction of comparators, e.g. `">=7.0.1, <8.0.0"`.
///
/// Deliberately not a full semver grammar: the manifest is data, and the
/// smallest grammar that expresses the constraint we actually have is the
/// one least likely to grow into a language
/// (docs/src/concepts/lnode-flashing.md, "Deliberately not").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq {
    comparators: Vec<Comparator>,
    text: String,
}

impl VersionReq {
    pub fn parse(s: &str) -> Result<Self, Error> {
        s.parse()
    }

    pub fn matches(&self, version: Version) -> bool {
        self.comparators.iter().all(|c| c.matches(version))
    }

    /// The constraint as written in the manifest, for messages a user reads.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl FromStr for VersionReq {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut comparators = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (op, rest) = if let Some(rest) = part.strip_prefix(">=") {
                (Op::Ge, rest)
            } else if let Some(rest) = part.strip_prefix("<=") {
                (Op::Le, rest)
            } else if let Some(rest) = part.strip_prefix('>') {
                (Op::Gt, rest)
            } else if let Some(rest) = part.strip_prefix('<') {
                (Op::Lt, rest)
            } else if let Some(rest) = part.strip_prefix('=') {
                (Op::Eq, rest)
            } else {
                return Err(Error::BadComparator(part.to_string()));
            };
            comparators.push(Comparator {
                op,
                version: Version::parse(rest)?,
            });
        }
        if comparators.is_empty() {
            return Err(Error::EmptyConstraint);
        }
        Ok(Self {
            comparators,
            text: s.trim().to_string(),
        })
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Read the installed SoftDevice version out of a flash dump — a
/// `CURRENT.UF2` read off the bootloader drive — rather than out of the
/// bootloader's own claim about it.
pub fn installed_version(dump: &Image) -> Option<Version> {
    let bytes = dump.read_at(VERSION_WORD_ADDR, 4)?;
    let word = u32::from_le_bytes(bytes.try_into().ok()?);
    // An erased or absent SoftDevice reads as 0xFFFFFFFF or 0; neither
    // decodes to a version, and reporting 4294.967.295 would be worse than
    // reporting nothing.
    (word != 0 && word != u32::MAX).then(|| Version::from_word(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ihex::Span;
    use crate::uf2::FAMILY_NRF52840_APP;

    /// Both rig boards read this word on 2026-08-09.
    const RIG_WORD: u32 = 7_003_000;

    #[test]
    fn the_measured_word_decodes_to_the_version_the_bootloader_reports() {
        assert_eq!(Version::from_word(RIG_WORD), Version::new(7, 3, 0));
        assert_eq!(Version::new(7, 3, 0).to_word(), RIG_WORD);
    }

    #[test]
    fn the_bindings_version_is_seven_zero_one_not_seven_zero_zero() {
        // SD_VERSION = 7000001 in the nrf-softdevice-s140 bindings. Misread
        // as 7.0.0 in docs/ble5-broadcast-protocol3-spike.md:39.
        assert_eq!(Version::from_word(7_000_001), Version::new(7, 0, 1));
    }

    #[test]
    fn the_factory_version_decodes_too() {
        assert_eq!(Version::from_word(6_001_001), Version::new(6, 1, 1));
    }

    #[test]
    fn every_word_round_trips_through_the_decoding() {
        for word in [0, 1, 6_001_001, 7_000_001, 7_003_000, 8_000_000] {
            assert_eq!(Version::from_word(word).to_word(), word);
        }
    }

    fn ours() -> VersionReq {
        VersionReq::parse(">=7.0.1, <8.0.0").unwrap()
    }

    #[test]
    fn our_constraint_accepts_the_bindings_version_and_the_rig_version() {
        assert!(ours().matches(Version::new(7, 0, 1)));
        assert!(ours().matches(Version::new(7, 3, 0)));
    }

    #[test]
    fn our_constraint_rejects_the_factory_version() {
        // The soft brick: 6.1.1 puts the application boundary at 0x26000, a
        // page below where our image starts.
        assert!(!ours().matches(Version::new(6, 1, 1)));
    }

    #[test]
    fn our_constraint_rejects_the_next_major() {
        // The ABI is stable within major 7 and nothing is promised beyond it.
        assert!(!ours().matches(Version::new(8, 0, 0)));
        assert!(!ours().matches(Version::new(8, 1, 2)));
    }

    #[test]
    fn our_constraint_rejects_the_version_just_below_the_floor() {
        assert!(!ours().matches(Version::new(7, 0, 0)));
    }

    #[test]
    fn comparator_forms_all_parse() {
        assert!(VersionReq::parse("=7.3.0")
            .unwrap()
            .matches(Version::new(7, 3, 0)));
        assert!(!VersionReq::parse("=7.3.0")
            .unwrap()
            .matches(Version::new(7, 3, 1)));
        assert!(VersionReq::parse(">7.0.1")
            .unwrap()
            .matches(Version::new(7, 0, 2)));
        assert!(!VersionReq::parse(">7.0.1")
            .unwrap()
            .matches(Version::new(7, 0, 1)));
        assert!(VersionReq::parse("<=7.3.0")
            .unwrap()
            .matches(Version::new(7, 3, 0)));
    }

    #[test]
    fn a_constraint_keeps_the_text_it_was_written_as() {
        assert_eq!(ours().as_str(), ">=7.0.1, <8.0.0");
        assert_eq!(ours().to_string(), ">=7.0.1, <8.0.0");
    }

    #[test]
    fn nonsense_constraints_are_errors_rather_than_vacuous_truths() {
        assert!(matches!(
            VersionReq::parse("7.0.1"),
            Err(Error::BadComparator(_))
        ));
        assert_eq!(VersionReq::parse(""), Err(Error::EmptyConstraint));
        assert!(matches!(
            VersionReq::parse(">=seven"),
            Err(Error::BadVersion(_))
        ));
        assert!(matches!(Version::parse("7.3"), Err(Error::BadVersion(_))));
        assert!(matches!(
            Version::parse("7.3.0.1"),
            Err(Error::BadVersion(_))
        ));
    }

    /// A dump shaped like `CURRENT.UF2`: covers `0x1000` upward, with the
    /// version word where `nrf_sdm.h` puts it.
    fn dump_with_word(word: u32) -> Image {
        let mut data = vec![0u8; 0x4000];
        let at = (VERSION_WORD_ADDR - 0x1000) as usize;
        data[at..at + 4].copy_from_slice(&word.to_le_bytes());
        Image::from_spans(
            &[Span {
                start: 0x1000,
                data,
            }],
            FAMILY_NRF52840_APP,
        )
    }

    #[test]
    fn the_version_is_readable_out_of_a_flash_dump_without_the_bootloader() {
        assert_eq!(
            installed_version(&dump_with_word(RIG_WORD)),
            Some(Version::new(7, 3, 0))
        );
    }

    #[test]
    fn an_erased_or_absent_softdevice_reads_as_no_version() {
        assert_eq!(installed_version(&dump_with_word(0xFFFF_FFFF)), None);
        assert_eq!(installed_version(&dump_with_word(0)), None);
    }

    #[test]
    fn a_dump_that_does_not_reach_the_word_reads_as_no_version() {
        let short = Image::from_spans(
            &[Span {
                start: 0x1000,
                data: vec![0u8; 0x100],
            }],
            FAMILY_NRF52840_APP,
        );
        assert_eq!(installed_version(&short), None);
    }
}
