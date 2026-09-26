//! Two tables, split by what they describe.
//!
//! * The **catalogue** ([`Catalogue`], `lnflash/catalogue.toml`) holds every
//!   board fact: which USB IDs an LNode answers on, what its bootloader
//!   publishes, where its flash window is, what SoftDevice it needs. These
//!   are properties of the hardware, so the catalogue is compiled into the
//!   binary and is always available.
//! * The **manifest** ([`Manifest`], a bundle's `manifest.toml`) holds what
//!   one firmware release carries: the version, and one image per board with
//!   its checksum.
//!
//! The split is Codeberg #342. The sessions that only configure a board that
//! is already running — `--set-time`, `--set-telemetry` — need the catalogue
//! and no image at all; before the split they loaded the whole bundle
//! manifest to reach the USB IDs and refused to start without one.
//!
//! The binary is board-agnostic. An `if board == "t114"` anywhere outside
//! this module would mean the split has failed — a new nRF or RP2040 board
//! is meant to be data entry, and a new chip family exactly one new
//! transport (docs/src/concepts/lnode-flashing.md, "Four axes").
//!
//! Two properties are enforced here rather than documented:
//!
//! * **A payload cannot be read without its checksum being verified.**
//!   [`Payload::read`] is the only way to get the bytes and it always hashes
//!   them, so "verify the image checksum" is not a step anyone can forget.
//! * **A third-party blob cannot ship without its licence.** [`Remedy`] has a
//!   mandatory `license` field and loading fails if that file is missing.
//!   Nordic's clause 2 requires the notice to travel with the distribution;
//!   Meshtastic vendors the same blob without one.
//!
//! Nothing here is a programming language, and it must not become one. When
//! declarative data is not enough, the answer is a new transport in Rust
//! ("Deliberately not").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::softdevice::VersionReq;
use crate::usb::UsbId;

/// The board catalogue, compiled in. See [`Catalogue::builtin`].
const CATALOGUE_TOML: &str = include_str!("../catalogue.toml");
/// What the compiled-in catalogue is called in an error message, so a
/// malformed one points at the file to edit rather than at a path that does
/// not exist on the user's disk.
const CATALOGUE_NAME: &str = "lnflash/catalogue.toml";

/// Where the manifest lives inside a bundle.
pub const MANIFEST_NAME: &str = "manifest.toml";
/// The bundle subdirectory holding the manifest and the images.
pub const FIRMWARE_DIR: &str = "firmware";
/// Last resort in the resolution order, for a distro-packaged install.
pub const SYSTEM_BUNDLE: &str = "/usr/share/lnflash";
/// Environment override, second in the resolution order.
pub const BUNDLE_ENV: &str = "LNFLASH_BUNDLE";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no bundle found; looked in {}", .0.join(", "))]
    NoBundle(Vec<String>),
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("{file}: sha256 is {actual}, manifest says {expected}")]
    Checksum {
        file: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("lnflash knows no board {wanted:?}; it knows {}", available.join(", "))]
    UnknownBoard {
        wanted: String,
        available: Vec<String>,
    },
    #[error("this bundle carries no image for {wanted:?}; it carries {}", available.join(", "))]
    NoImage {
        wanted: String,
        available: Vec<String>,
    },
    #[error("board {board}: {field} is not a version constraint: {source}")]
    BadConstraint {
        board: String,
        field: String,
        #[source]
        source: crate::softdevice::Error,
    },
    #[error("board {board}: {field} is not a vid:pid: {value:?}")]
    BadUsbId {
        board: String,
        field: String,
        value: String,
    },
}

/// How the bytes get in. One variant today; a new chip family adds one here
/// and one module, and touches nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Copy a UF2 onto the bootloader's mass-storage drive.
    Uf2Msc,
}

/// How a board reaches a programmable state, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Entry {
    /// Open a CDC port at 1200 baud. Only works if the running firmware
    /// implements it; ours does, stock Meshtastic does not.
    // Spelled out because kebab-case renaming does not split before a digit.
    #[serde(rename = "touch-1200")]
    Touch1200,
    /// The bootloader's own mechanism. Works regardless of what is running,
    /// and needs a human — the load-bearing limit on any automatic tool.
    DoubleTap,
}

/// What has to happen to a payload file before it can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Convert {
    /// Already a UF2; copy it as it is.
    None,
    /// Intel HEX, converted to UF2 at run time. Converting alters no byte,
    /// only the container, but distributing the untouched hex avoids even
    /// the appearance of the modification Nordic's clause 5 prohibits.
    HexToUf2,
}

/// Every board this binary knows, independent of any firmware release.
#[derive(Debug, Clone, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub board: BTreeMap<String, Board>,
}

/// What one bundle carries: the release it is, and one image per board.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub bundle: BundleInfo,
    /// Keyed by the catalogue's board name. A bundle need not carry every
    /// board the catalogue knows, but every board it names must be one.
    #[serde(default)]
    pub board: BTreeMap<String, Payloads>,
    /// Directory the payload paths are relative to. Filled in by [`load`].
    #[serde(skip)]
    pub root: PathBuf,
}

/// The images one bundle carries for one board.
#[derive(Debug, Clone, Deserialize)]
pub struct Payloads {
    pub app: Payload,
    #[serde(default)]
    pub remedy: Remedy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleInfo {
    /// The firmware release this bundle carries.
    pub version: String,
    /// Free-form provenance for a user staring at a tarball of unknown age.
    #[serde(default)]
    pub built: Option<String>,
}

/// Everything this tool knows about one board — the four axes and the
/// preconditions crossing them. All of it hardware fact, which is why it
/// lives in the catalogue and not in a bundle: none of it changes when a new
/// firmware release is cut.
#[derive(Debug, Clone, Deserialize)]
pub struct Board {
    /// Chip family, informational. `transport` is what decides behaviour.
    pub family: String,
    pub transport: Transport,
    pub entry: Vec<Entry>,
    pub identify: Identify,
    pub flash: Flash,
    #[serde(default)]
    pub requires: Requires,
    /// What to tell a person who has to reach for this board. Absent where
    /// the ordinary wording fits, which is every board whose RESET is a
    /// button on the outside of the case.
    #[serde(default)]
    pub double_tap: DoubleTap,
}

/// Per-board wording for the one instruction in this tool a human has to act
/// on. It is data rather than a branch for the reason the module header
/// gives: an `if board == "rak4631"` around a prompt is the same failure as
/// one around a flash address.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DoubleTap {
    /// The press itself, replacing "press RESET twice, quickly — the second
    /// press within about half a second of the first."
    #[serde(default)]
    pub press: Option<String>,
    /// Where the longer story is, for a board whose recovery has one.
    #[serde(default)]
    pub docs: Option<String>,
}

/// The **identify** axis, in the two stages the order of work demands.
#[derive(Debug, Clone, Deserialize)]
pub struct Identify {
    /// The truth, read from `INFO_UF2.TXT` after entering the bootloader.
    /// The only thing a write may rest on.
    pub info_uf2_board_id: String,
    /// USB IDs the bootloader answers on. Stage one only: it says a device
    /// is worth mounting, never what board it is.
    pub bootloader_usb: Vec<String>,
    /// USB IDs worth trying a touch on. Stage one only, and weaker still —
    /// these belong to whatever firmware is installed, not to the board.
    #[serde(default)]
    pub candidate_usb: Vec<String>,
    /// The mass-storage label the bootloader publishes. Reported to the
    /// user; never used to decide anything.
    #[serde(default)]
    pub msc_label: Option<String>,
}

/// Flash geometry, as measured from the bootloader's own `CURRENT.UF2`.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Flash {
    /// UF2 family ID to write and to convert into. Never a bootloader family.
    pub family_id: u32,
    /// First address the bootloader accepts. Below it, blocks are skipped
    /// silently while still reporting success.
    pub writable_start: u32,
    /// One past the last address it accepts. At or above, blocks are rejected.
    pub writable_end: u32,
    /// Where the application lives, above the SoftDevice.
    pub app_base: u32,
}

/// One file in the bundle, and the checksum that says it arrived intact.
#[derive(Debug, Clone, Deserialize)]
pub struct Payload {
    /// Path relative to the manifest's directory.
    pub file: PathBuf,
    pub sha256: String,
    #[serde(default)]
    pub convert: Option<Convert>,
    /// The git SHA the image was built from, for the `[FW_BUILD]` banner
    /// check. Absent for third-party images, which emit no such banner.
    #[serde(default)]
    pub git_sha: Option<String>,
}

/// Preconditions. They cross all four axes, and each one names its own
/// remedy rather than becoming a special case in code.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub softdevice: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Remedy {
    #[serde(default)]
    pub softdevice: Option<RemedyPayload>,
}

/// A payload that repairs an unmet precondition, plus the licence that has
/// to travel with it.
#[derive(Debug, Clone, Deserialize)]
pub struct RemedyPayload {
    #[serde(flatten)]
    pub payload: Payload,
    /// Mandatory. Loading fails if the file is absent, which is what makes
    /// shipping a third-party blob without its licence impossible.
    pub license: PathBuf,
}

impl Payload {
    /// Read the file and verify its checksum. The only way to obtain the
    /// bytes, so an unverified image cannot reach flash.
    pub fn read(&self, root: &Path) -> Result<Vec<u8>, Error> {
        let path = root.join(&self.file);
        let bytes = std::fs::read(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let actual = hex_digest(&bytes);
        if !actual.eq_ignore_ascii_case(self.sha256.trim()) {
            return Err(Error::Checksum {
                file: path,
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(bytes)
    }

    pub fn path(&self, root: &Path) -> PathBuf {
        root.join(&self.file)
    }
}

impl Board {
    /// The SoftDevice constraint, parsed. `None` means the board states no
    /// SoftDevice precondition at all.
    pub fn softdevice_req(&self, name: &str) -> Result<Option<VersionReq>, Error> {
        self.requires
            .softdevice
            .as_deref()
            .map(|s| {
                VersionReq::parse(s).map_err(|source| Error::BadConstraint {
                    board: name.to_string(),
                    field: "requires.softdevice".into(),
                    source,
                })
            })
            .transpose()
    }

    pub fn bootloader_ids(&self, name: &str) -> Result<Vec<UsbId>, Error> {
        parse_ids(
            &self.identify.bootloader_usb,
            name,
            "identify.bootloader_usb",
        )
    }

    pub fn candidate_ids(&self, name: &str) -> Result<Vec<UsbId>, Error> {
        parse_ids(&self.identify.candidate_usb, name, "identify.candidate_usb")
    }
}

fn parse_ids(values: &[String], board: &str, field: &str) -> Result<Vec<UsbId>, Error> {
    values
        .iter()
        .map(|v| {
            v.parse::<UsbId>().map_err(|_| Error::BadUsbId {
                board: board.to_string(),
                field: field.to_string(),
                value: v.clone(),
            })
        })
        .collect()
}

impl Catalogue {
    /// The catalogue compiled into this binary.
    ///
    /// Parsed on each call rather than cached: it is under two kilobytes and
    /// no run reads it more than twice, so a `OnceLock` holding a `Result`
    /// would buy nothing and cost a second way for this to fail.
    ///
    /// A parse failure here is a bug in `lnflash/catalogue.toml`, not
    /// something a user can cause or repair, which is why it is still a
    /// `Result` and not a panic — the same rule that keeps `expect()` out of
    /// this crate. The unit test
    /// `the_builtin_catalogue_is_well_formed_and_needs_no_bundle_to_read`
    /// holds it at test time.
    pub fn builtin() -> Result<Self, Error> {
        Self::parse(CATALOGUE_TOML, Path::new(CATALOGUE_NAME))
    }

    fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let catalogue: Self = toml::from_str(text).map_err(|source| Error::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        validate_catalogue(&catalogue, path)?;
        Ok(catalogue)
    }

    pub fn board(&self, name: &str) -> Result<&Board, Error> {
        self.board.get(name).ok_or_else(|| Error::UnknownBoard {
            wanted: name.to_string(),
            available: self.board.keys().cloned().collect(),
        })
    }

    /// The board whose `Board-ID` matches what the bootloader published.
    /// This is stage two of identify — the answer a write is allowed to rest
    /// on — so it matches exactly, never as a substring.
    pub fn board_for_id(&self, board_id: &str) -> Option<(&str, &Board)> {
        self.board
            .iter()
            .find(|(_, b)| b.identify.info_uf2_board_id == board_id)
            .map(|(name, b)| (name.as_str(), b))
    }

    pub fn names(&self) -> Vec<&str> {
        self.board.keys().map(String::as_str).collect()
    }
}

impl Manifest {
    /// The images this bundle carries for one board.
    ///
    /// Separate from [`Catalogue::board`] on purpose: "lnflash does not know
    /// that board" and "this bundle does not carry an image for it" are
    /// different facts and get different errors, because they have different
    /// remedies.
    pub fn payloads(&self, name: &str) -> Result<&Payloads, Error> {
        self.board.get(name).ok_or_else(|| Error::NoImage {
            wanted: name.to_string(),
            available: self.board.keys().cloned().collect(),
        })
    }

    pub fn names(&self) -> Vec<&str> {
        self.board.keys().map(String::as_str).collect()
    }

    /// Read and verify every payload in the bundle. What `--check-bundle`
    /// runs so a user can check a tarball without a board attached.
    pub fn verify_all(&self) -> Result<(), Error> {
        for payloads in self.board.values() {
            payloads.app.read(&self.root)?;
            if let Some(remedy) = &payloads.remedy.softdevice {
                remedy.payload.read(&self.root)?;
            }
        }
        Ok(())
    }
}

/// Load and validate a bundle manifest from the directory that holds it.
///
/// The catalogue is needed because a manifest only names boards; whether
/// those names mean anything, and whether a SoftDevice remedy has a
/// precondition to repair, is the catalogue's to say.
pub fn load(dir: &Path, catalogue: &Catalogue) -> Result<Manifest, Error> {
    let path = dir.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    let mut manifest: Manifest = toml::from_str(&text).map_err(|source| Error::Toml {
        path: path.clone(),
        source,
    })?;
    manifest.root = dir.to_path_buf();
    validate(&manifest, catalogue, &path)?;
    Ok(manifest)
}

fn validate_catalogue(catalogue: &Catalogue, path: &Path) -> Result<(), Error> {
    let bad = |message: String| Error::Invalid {
        path: path.to_path_buf(),
        message,
    };
    if catalogue.board.is_empty() {
        return Err(bad("a catalogue with no boards in it".into()));
    }
    for (name, board) in &catalogue.board {
        if board.identify.info_uf2_board_id.trim().is_empty() {
            return Err(bad(format!(
                "board {name}: identify.info_uf2_board_id is empty, so no board could ever \
                 be confirmed and no write could ever be safe"
            )));
        }
        if board.entry.is_empty() {
            return Err(bad(format!(
                "board {name}: no entry mechanism, so the bootloader is unreachable"
            )));
        }
        if board.identify.bootloader_usb.is_empty() {
            return Err(bad(format!(
                "board {name}: no identify.bootloader_usb, so the bootloader is unrecognisable"
            )));
        }
        board.bootloader_ids(name)?;
        board.candidate_ids(name)?;
        board.softdevice_req(name)?;

        if board.flash.family_id == crate::uf2::FAMILY_NRF52_BOOTLOADER {
            return Err(bad(format!(
                "board {name}: flash.family_id is the bootloader family; that image rewrites \
                 MBR, bootloader and UICR, and a failure there needs SWD to undo"
            )));
        }
        if board.flash.writable_start >= board.flash.writable_end {
            return Err(bad(format!(
                "board {name}: flash window {:#x}..{:#x} is empty",
                board.flash.writable_start, board.flash.writable_end
            )));
        }
        if !(board.flash.writable_start..board.flash.writable_end).contains(&board.flash.app_base) {
            return Err(bad(format!(
                "board {name}: app_base {:#x} is outside the writable window {:#x}..{:#x}",
                board.flash.app_base, board.flash.writable_start, board.flash.writable_end
            )));
        }
    }
    Ok(())
}

fn validate(manifest: &Manifest, catalogue: &Catalogue, path: &Path) -> Result<(), Error> {
    let bad = |message: String| Error::Invalid {
        path: path.to_path_buf(),
        message,
    };
    if manifest.board.is_empty() {
        return Err(bad("a bundle with no images in it".into()));
    }
    for (name, payloads) in &manifest.board {
        // A bundle naming a board nothing knows would flash nothing, and the
        // name is the only handle --board offers, so say it at load time
        // rather than after a board has been brought into its bootloader.
        let board = catalogue.board(name)?;

        exists(manifest, &payloads.app.file, name, "app.file", path)?;
        if let Some(remedy) = &payloads.remedy.softdevice {
            exists(
                manifest,
                &remedy.payload.file,
                name,
                "remedy.softdevice.file",
                path,
            )?;
            // Clause 2, enforced: the notice travels with the distribution.
            exists(
                manifest,
                &remedy.license,
                name,
                "remedy.softdevice.license",
                path,
            )?;
            // A precondition without a remedy is a dead end for the user. It
            // is allowed — "I cannot fix this, here is why" beats writing
            // anyway — but a remedy without the precondition it repairs is
            // nonsense.
            if board.requires.softdevice.is_none() {
                return Err(bad(format!(
                    "board {name}: a softdevice remedy with no requires.softdevice to trigger it"
                )));
            }
        }
    }
    Ok(())
}

fn exists(
    manifest: &Manifest,
    file: &Path,
    board: &str,
    field: &str,
    path: &Path,
) -> Result<(), Error> {
    if file.is_absolute() || file.components().any(|c| c.as_os_str() == "..") {
        return Err(Error::Invalid {
            path: path.to_path_buf(),
            message: format!("board {board}: {field} {file:?} must stay inside the bundle"),
        });
    }
    if manifest.root.join(file).is_file() {
        return Ok(());
    }
    Err(Error::Invalid {
        path: path.to_path_buf(),
        message: format!("board {board}: {field} {file:?} is not in the bundle"),
    })
}

/// Find the bundle: `--bundle`, then `$LNFLASH_BUNDLE`, then next to the
/// executable, then the system path. Returns the directory holding
/// `manifest.toml`.
///
/// Each candidate is accepted both as the directory containing the manifest
/// and as the unpacked tarball root containing `firmware/`, because a user
/// who points `--bundle` at the directory they just `cd`'d into is right.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, Error> {
    let mut tried = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    }
    if let Some(from_env) = std::env::var_os(BUNDLE_ENV) {
        candidates.push(PathBuf::from(from_env));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push(PathBuf::from(SYSTEM_BUNDLE));

    for candidate in candidates {
        for dir in [candidate.clone(), candidate.join(FIRMWARE_DIR)] {
            if dir.join(MANIFEST_NAME).is_file() {
                return Ok(dir);
            }
            tried.push(dir.display().to_string());
        }
    }
    Err(Error::NoBundle(tried))
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// The compiled-in catalogue with one substring replaced, so a test that
    /// mutates a field is mutating the real thing.
    ///
    /// It asserts the substring was there. A `replace` that matches nothing
    /// returns the input unchanged, and a validation test fed unchanged input
    /// passes for the one reason that proves nothing.
    fn mutated_catalogue(from: &str, to: &str) -> Result<Catalogue, Error> {
        assert!(
            CATALOGUE_TOML.contains(from),
            "the catalogue no longer contains {from:?}, so this test mutates nothing"
        );
        Catalogue::parse(&CATALOGUE_TOML.replace(from, to), Path::new(CATALOGUE_NAME))
    }

    fn catalogue() -> Catalogue {
        Catalogue::builtin().unwrap()
    }

    /// A bundle shaped exactly like the real one, with stand-in payloads so
    /// the test states its own checksums.
    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            fs::create_dir_all(dir.path().join("t114")).unwrap();
            let f = Self { dir };
            f.write("t114/leviculum-t114-0.8.0.uf2", b"application image");
            f.write("t114/s140_nrf52_7.3.0_softdevice.hex", b":00000001FF\n");
            f.write(
                "t114/s140_nrf52_7.3.0_license-agreement.txt",
                b"Copyright (c) Nordic Semiconductor ASA",
            );
            f.write_manifest(&f.manifest_text());
            f
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            fs::write(self.dir.path().join(rel), bytes).unwrap();
        }

        fn write_manifest(&self, text: &str) {
            self.write(MANIFEST_NAME, text.as_bytes());
        }

        fn sha(&self, rel: &str) -> String {
            hex_digest(&fs::read(self.dir.path().join(rel)).unwrap())
        }

        /// Add the RAK4631 image, making this the two-board bundle
        /// `scripts/lnflash-bundle.sh` produces (Codeberg #261). No SoftDevice
        /// remedy: the bundle carries none for that board, which is the
        /// decision recorded in the catalogue.
        fn with_rak(self) -> Self {
            fs::create_dir_all(self.dir.path().join("rak4631")).unwrap();
            self.write("rak4631/leviculum-rak4631-0.8.0.uf2", b"the other image");
            let text = format!(
                r#"{}
[board.rak4631.app]
file    = "rak4631/leviculum-rak4631-0.8.0.uf2"
sha256  = "{rak}"
git_sha = "bb7c4f64"
"#,
                self.manifest_text(),
                rak = self.sha("rak4631/leviculum-rak4631-0.8.0.uf2"),
            );
            self.write_manifest(&text);
            self
        }

        fn manifest_text(&self) -> String {
            format!(
                r#"
[bundle]
version = "0.8.0"
built = "2026-08-10"

[board.t114.app]
file    = "t114/leviculum-t114-0.8.0.uf2"
sha256  = "{app}"
git_sha = "bb7c4f64"

[board.t114.remedy.softdevice]
file    = "t114/s140_nrf52_7.3.0_softdevice.hex"
sha256  = "{sd}"
license = "t114/s140_nrf52_7.3.0_license-agreement.txt"
convert = "hex-to-uf2"
"#,
                app = self.sha("t114/leviculum-t114-0.8.0.uf2"),
                sd = self.sha("t114/s140_nrf52_7.3.0_softdevice.hex"),
            )
        }

        fn load(&self) -> Result<Manifest, Error> {
            load(self.dir.path(), &catalogue())
        }
    }

    // -----------------------------------------------------------------
    // The catalogue (Codeberg #342)
    // -----------------------------------------------------------------

    #[test]
    fn the_builtin_catalogue_is_well_formed_and_needs_no_bundle_to_read() {
        // The whole point of the split: this is available with nothing on
        // disk, which is what lets --set-time and --set-telemetry start.
        let catalogue = Catalogue::builtin().unwrap();
        let board = catalogue.board("t114").unwrap();
        assert_eq!(board.transport, Transport::Uf2Msc);
        assert_eq!(board.entry, vec![Entry::Touch1200, Entry::DoubleTap]);
        assert_eq!(board.identify.info_uf2_board_id, "HT-n5262");
        assert_eq!(board.flash.family_id, crate::uf2::FAMILY_NRF52840_APP);
        assert_eq!(board.flash.app_base, 0x2_7000);
        assert_eq!(
            board.softdevice_req("t114").unwrap().unwrap().as_str(),
            ">=7.0.1, <8.0.0"
        );
        assert_eq!(catalogue.names(), vec!["rak4631", "t114"]);
    }

    #[test]
    fn the_rak4631_is_a_board_the_binary_knows_without_any_bundle_on_disk() {
        // Codeberg #261. Every fact here is transcribed from
        // docs/src/concepts/lnode-flashing.md, which records where each was
        // measured; the test is what stops a typo in the transcription.
        let catalogue = catalogue();
        let board = catalogue.board("rak4631").unwrap();
        assert_eq!(board.transport, Transport::Uf2Msc);
        assert_eq!(board.entry, vec![Entry::Touch1200, Entry::DoubleTap]);
        assert_eq!(board.identify.info_uf2_board_id, "WisBlock-RAK4631-Board");
        assert_eq!(board.identify.msc_label.as_deref(), Some("RAK4631"));
        assert_eq!(board.flash.family_id, crate::uf2::FAMILY_NRF52840_APP);
        assert_eq!(board.flash.app_base, 0x2_7000);
        assert_eq!(board.flash.writable_end, 0xEA000);
        assert_eq!(
            board.softdevice_req("rak4631").unwrap().unwrap().as_str(),
            ">=7.0.1, <8.0.0"
        );
    }

    #[test]
    fn usb_ids_in_the_catalogue_are_parsed_not_matched_as_strings() {
        let catalogue = catalogue();
        let board = catalogue.board("t114").unwrap();
        assert_eq!(
            board.bootloader_ids("t114").unwrap(),
            vec!["239a:0071".parse::<UsbId>().unwrap()]
        );
        assert_eq!(board.candidate_ids("t114").unwrap().len(), 2);

        let rak = catalogue.board("rak4631").unwrap();
        assert_eq!(
            rak.bootloader_ids("rak4631").unwrap(),
            vec!["239a:0029".parse::<UsbId>().unwrap()]
        );
        assert_eq!(
            rak.candidate_ids("rak4631").unwrap(),
            vec!["1209:0002".parse::<UsbId>().unwrap()]
        );
    }

    #[test]
    fn no_two_boards_claim_the_same_usb_id() {
        // A shared ID would make `find_candidates` hint the wrong board, and a
        // hint decides which bootloader IDs get waited for after the touch —
        // so the board that came back would never be recognised. Cheap to
        // assert here, expensive to find on a bench.
        let catalogue = catalogue();
        let mut seen: BTreeMap<UsbId, &str> = BTreeMap::new();
        for (name, board) in &catalogue.board {
            for id in board
                .bootloader_ids(name)
                .unwrap()
                .into_iter()
                .chain(board.candidate_ids(name).unwrap())
            {
                if let Some(other) = seen.insert(id, name) {
                    panic!("{id} is claimed by both {other} and {name}");
                }
            }
        }
        assert!(seen.len() >= 5, "{seen:?}");
    }

    #[test]
    fn a_board_lnflash_does_not_know_is_named_along_with_the_ones_it_does() {
        // The XIAO nRF52840 family has no entry and is not meant to get one
        // until a second discriminator exists (docs/src/firmware/boards.md).
        match catalogue().board("xiao_nrf52840") {
            Err(Error::UnknownBoard { wanted, available }) => {
                assert_eq!(wanted, "xiao_nrf52840");
                assert_eq!(available, vec!["rak4631".to_string(), "t114".to_string()]);
            }
            other => panic!("expected UnknownBoard, got {other:?}"),
        }
    }

    #[test]
    fn a_board_is_looked_up_by_the_id_the_bootloader_published() {
        let catalogue = catalogue();
        assert_eq!(catalogue.board_for_id("HT-n5262").unwrap().0, "t114");
        assert_eq!(
            catalogue.board_for_id("WisBlock-RAK4631-Board").unwrap().0,
            "rak4631"
        );
        // Exactly, never as a substring, in either direction.
        assert!(catalogue.board_for_id("HT-n5262-something").is_none());
        assert!(catalogue.board_for_id("WisBlock-RAK4631").is_none());
        assert!(catalogue.board_for_id("RAK4631").is_none());
    }

    #[test]
    fn a_catalogue_naming_the_bootloader_family_will_not_load() {
        let err = mutated_catalogue("0xADA52840", "0xD663823C").unwrap_err();
        assert!(format!("{err}").contains("bootloader family"), "{err}");
    }

    #[test]
    fn an_app_base_outside_the_writable_window_will_not_load() {
        // 0xEC000 is the identity page, above what the bootloader will write.
        let err =
            mutated_catalogue("app_base       = 0x27000", "app_base       = 0xEC000").unwrap_err();
        assert!(
            format!("{err}").contains("outside the writable window"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_board_id_will_not_load_because_nothing_could_confirm_it() {
        let err = mutated_catalogue(
            "info_uf2_board_id = \"HT-n5262\"",
            "info_uf2_board_id = \"\"",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("info_uf2_board_id is empty"),
            "{err}"
        );
    }

    #[test]
    fn a_board_with_no_entry_mechanism_will_not_load() {
        let err = mutated_catalogue(
            r#"entry     = ["touch-1200", "double-tap"]"#,
            "entry     = []",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("no entry mechanism"), "{err}");
    }

    #[test]
    fn an_unknown_transport_will_not_load() {
        assert!(matches!(
            mutated_catalogue("uf2-msc", "carrier-pigeon"),
            Err(Error::Toml { .. })
        ));
    }

    #[test]
    fn a_malformed_version_constraint_will_not_load() {
        assert!(matches!(
            mutated_catalogue(">=7.0.1, <8.0.0", "7ish"),
            Err(Error::BadConstraint { .. })
        ));
    }

    #[test]
    fn a_malformed_usb_id_will_not_load() {
        assert!(matches!(
            mutated_catalogue("239a:0071", "239a-0071"),
            Err(Error::BadUsbId { .. })
        ));
    }

    // -----------------------------------------------------------------
    // The bundle manifest
    // -----------------------------------------------------------------

    #[test]
    fn a_well_formed_bundle_loads_with_its_images_and_their_checksums() {
        let f = Fixture::new();
        let manifest = f.load().unwrap();
        assert_eq!(manifest.bundle.version, "0.8.0");
        let payloads = manifest.payloads("t114").unwrap();
        assert_eq!(payloads.app.git_sha.as_deref(), Some("bb7c4f64"));
        assert_eq!(
            payloads.remedy.softdevice.as_ref().unwrap().payload.convert,
            Some(Convert::HexToUf2)
        );
        assert_eq!(manifest.names(), vec!["t114"]);
    }

    #[test]
    fn payloads_verify_and_the_whole_bundle_verifies() {
        let f = Fixture::new();
        let manifest = f.load().unwrap();
        let payloads = manifest.payloads("t114").unwrap();
        assert_eq!(
            payloads.app.read(&manifest.root).unwrap(),
            b"application image"
        );
        manifest.verify_all().unwrap();
    }

    #[test]
    fn a_payload_whose_bytes_changed_cannot_be_read_at_all() {
        let f = Fixture::new();
        let manifest = f.load().unwrap();
        f.write("t114/leviculum-t114-0.8.0.uf2", b"application imagX");
        let err = manifest.payloads("t114").unwrap().app.read(&manifest.root);
        assert!(matches!(err, Err(Error::Checksum { .. })), "{err:?}");
        assert!(matches!(manifest.verify_all(), Err(Error::Checksum { .. })));
    }

    #[test]
    fn a_bundle_carrying_no_image_for_a_board_says_so_in_its_own_words() {
        // Distinct from "lnflash knows no such board": the catalogue knows
        // both boards, this bundle just has nothing to write to a RAK.
        //
        // This is the #342 distinction, and #261 is exactly when it starts to
        // earn its keep: before the RAK had a catalogue entry the two errors
        // could not be told apart by example. A bundle built before #261, or
        // one built with `--bin t114` alone, must still refuse a RAK with
        // NoImage — "fetch a newer bundle" — rather than UnknownBoard, which
        // would send the user after a newer *binary*.
        let f = Fixture::new();
        let manifest = f.load().unwrap();
        match manifest.payloads("rak4631") {
            Err(Error::NoImage { wanted, available }) => {
                assert_eq!(wanted, "rak4631");
                assert_eq!(available, vec!["t114".to_string()]);
            }
            other => panic!("expected NoImage, got {other:?}"),
        }
    }

    #[test]
    fn a_bundle_carrying_both_boards_loads_and_every_image_in_it_verifies() {
        // Codeberg #261: the shape `scripts/lnflash-bundle.sh` now emits. Two
        // boards, one of which states a SoftDevice remedy and one of which
        // does not — and `--check-bundle` reads both images.
        let f = Fixture::new().with_rak();
        let manifest = f.load().unwrap();
        assert_eq!(manifest.names(), vec!["rak4631", "t114"]);
        let rak = manifest.payloads("rak4631").unwrap();
        assert_eq!(rak.app.read(&manifest.root).unwrap(), b"the other image");
        assert!(rak.remedy.softdevice.is_none());
        assert!(manifest
            .payloads("t114")
            .unwrap()
            .remedy
            .softdevice
            .is_some());
        manifest.verify_all().unwrap();

        // And verify_all really reaches the second board: break only its image.
        f.write("rak4631/leviculum-rak4631-0.8.0.uf2", b"the other imagX");
        assert!(matches!(manifest.verify_all(), Err(Error::Checksum { .. })));
    }

    #[test]
    fn a_bundle_naming_a_board_the_catalogue_does_not_know_will_not_load() {
        let f = Fixture::new();
        f.write_manifest(&f.manifest_text().replace("board.t114", "board.wisdom"));
        let err = f.load().unwrap_err();
        assert!(format!("{err}").contains("wisdom"), "{err}");
        assert!(format!("{err}").contains("knows no board"), "{err}");
    }

    #[test]
    fn a_bundle_with_no_images_in_it_will_not_load() {
        let f = Fixture::new();
        f.write_manifest("[bundle]\nversion = \"0.8.0\"\n");
        let err = f.load().unwrap_err();
        assert!(format!("{err}").contains("no images in it"), "{err}");
    }

    #[test]
    fn a_remedy_without_its_licence_file_will_not_load() {
        let f = Fixture::new();
        fs::remove_file(
            f.dir
                .path()
                .join("t114/s140_nrf52_7.3.0_license-agreement.txt"),
        )
        .unwrap();
        let err = f.load().unwrap_err();
        assert!(
            format!("{err}").contains("s140_nrf52_7.3.0_license-agreement.txt"),
            "the licence must be named in the error: {err}"
        );
    }

    #[test]
    fn a_payload_file_missing_from_the_bundle_will_not_load() {
        let f = Fixture::new();
        fs::remove_file(f.dir.path().join("t114/leviculum-t114-0.8.0.uf2")).unwrap();
        assert!(matches!(f.load(), Err(Error::Invalid { .. })));
    }

    #[test]
    fn a_payload_path_may_not_escape_the_bundle() {
        let f = Fixture::new();
        f.write_manifest(&f.manifest_text().replace(
            "\"t114/s140_nrf52_7.3.0_license-agreement.txt\"",
            "\"../../../etc/passwd\"",
        ));
        let err = f.load().unwrap_err();
        assert!(
            format!("{err}").contains("must stay inside the bundle"),
            "{err}"
        );
    }

    #[test]
    fn a_remedy_with_no_precondition_to_trigger_it_will_not_load() {
        // The precondition now lives in the catalogue, so this crosses the
        // two tables: a bundle offering a SoftDevice for a board that states
        // no SoftDevice constraint is nonsense whichever side is wrong.
        let f = Fixture::new();
        let catalogue = mutated_catalogue("softdevice = \">=7.0.1, <8.0.0\"", "").unwrap();
        let err = load(f.dir.path(), &catalogue).unwrap_err();
        assert!(format!("{err}").contains("no requires.softdevice"), "{err}");
    }

    #[test]
    fn the_bundle_is_found_both_as_the_manifest_dir_and_as_the_tarball_root() {
        let f = Fixture::new();
        let found = locate(Some(f.dir.path())).unwrap();
        assert_eq!(found, f.dir.path());

        let outer = TempDir::new().unwrap();
        let inner = outer.path().join(FIRMWARE_DIR);
        fs::create_dir(&inner).unwrap();
        fs::write(inner.join(MANIFEST_NAME), "").unwrap();
        assert_eq!(locate(Some(outer.path())).unwrap(), inner);
    }

    #[test]
    fn a_bundle_that_is_nowhere_names_everywhere_it_looked() {
        let missing = TempDir::new().unwrap().path().join("gone");
        match locate(Some(&missing)) {
            Err(Error::NoBundle(tried)) => {
                assert!(tried.iter().any(|t| t.contains("gone")));
                assert!(tried.iter().any(|t| t.contains(SYSTEM_BUNDLE)));
            }
            other => panic!("expected NoBundle, got {other:?}"),
        }
    }
}
