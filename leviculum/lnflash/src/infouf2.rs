//! `INFO_UF2.TXT`, the file the bootloader publishes about itself.
//!
//! This is the only trustworthy statement of what a board is: the running
//! application's USB ID belongs to whatever firmware happens to be installed,
//! the bootloader's `Board-ID` belongs to the board
//! (docs/src/concepts/lnode-flashing.md, "Identify").
//!
//! Parsing never fails. Bootloaders differ in which keys they emit — the RAK
//! adds `Ver:`, older builds omit `SoftDevice:` — so an unknown or absent key
//! is data, not an error. Refusing to flash because a key is missing is the
//! caller's decision, taken against a named key rather than a parse failure.

use crate::softdevice::Version;

/// A parsed `INFO_UF2.TXT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InfoUf2 {
    /// The leading line, which carries no key. Free-form bootloader identity.
    pub banner: Option<String>,
    /// Every `Key: Value` line in file order, unknown keys included.
    pub keys: Vec<(String, String)>,
}

/// A `SoftDevice: S140 7.3.0` line, split into the two things it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftDevice {
    pub name: String,
    pub version: Version,
}

pub fn parse(text: &str) -> InfoUf2 {
    let mut info = InfoUf2::default();
    for raw in text.lines() {
        // CRLF is what the real files use; a lone LF also has to work.
        let line = raw.trim_end_matches(['\r', '\n']).trim();
        if line.is_empty() {
            continue;
        }
        match split_key(line) {
            Some((key, value)) => info.keys.push((key.to_string(), value.to_string())),
            None if info.banner.is_none() => info.banner = Some(line.to_string()),
            // A second key-less line. Nothing we have seen emits one; keeping
            // the first is more useful than overwriting it with the last.
            None => {}
        }
    }
    info
}

/// Split at the first `: `, and only when the part before it looks like a
/// key. The banner line contains no colon at all on either real file, but a
/// future one might, and `lib/uf2 (remotes/origin/...)` must not become a key.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(": ")?;
    let plausible = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    plausible.then_some((key, value.trim()))
}

impl InfoUf2 {
    /// First value for `key`, case-insensitively.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// The board's identity. Everything a flash decides rests on this.
    pub fn board_id(&self) -> Option<&str> {
        self.get("Board-ID")
    }

    pub fn model(&self) -> Option<&str> {
        self.get("Model")
    }

    pub fn date(&self) -> Option<&str> {
        self.get("Date")
    }

    /// The SoftDevice the bootloader reports. `None` covers both "no such
    /// line" and "a line we cannot read", which are the same thing to a
    /// caller that must then read the version out of flash instead.
    pub fn softdevice(&self) -> Option<SoftDevice> {
        let value = self.get("SoftDevice")?;
        let (name, version) = value.rsplit_once(' ')?;
        Some(SoftDevice {
            name: name.trim().to_string(),
            version: Version::parse(version.trim()).ok()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off the rig T114, verbatim, CRLF included
    /// (docs/src/concepts/lnode-flashing.md).
    const T114: &str = concat!(
        "UF2 Bootloader 0.9.0-2-g836c8dc-dirty lib/nrfx (v2.0.0) lib/tinyusb \
         (0.12.0-145-g9775e7691) lib/uf2 (remotes/origin/configupdate-9-gadbb8c7)\r\n",
        "Model: HT-n5262\r\n",
        "Board-ID: HT-n5262\r\n",
        "Date: Jul  9 2024\r\n",
        "SoftDevice: S140 7.3.0\r\n",
    );

    /// The RAK4631's file. Same shape plus a `Ver:` key we do not know.
    const RAK: &str = concat!(
        "UF2 Bootloader 0.4.3\r\n",
        "Model: WisBlock RAK4631 Board\r\n",
        "Board-ID: WisBlock-RAK4631-Board\r\n",
        "Date: May 20 2023\r\n",
        "Ver: 0.4.3\r\n",
        "SoftDevice: S140 7.3.0\r\n",
    );

    #[test]
    fn the_t114_file_parses_to_its_measured_values() {
        let info = parse(T114);
        assert_eq!(info.board_id(), Some("HT-n5262"));
        assert_eq!(info.model(), Some("HT-n5262"));
        assert_eq!(info.date(), Some("Jul  9 2024"));
        let sd = info.softdevice().unwrap();
        assert_eq!(sd.name, "S140");
        assert_eq!(sd.version, Version::new(7, 3, 0));
    }

    #[test]
    fn the_banner_line_is_kept_whole_and_is_not_read_as_a_key() {
        let info = parse(T114);
        assert!(info
            .banner
            .as_ref()
            .unwrap()
            .starts_with("UF2 Bootloader 0.9.0-2-"));
        // "lib/nrfx (v2.0.0)" and friends contain no `: `, and the leading
        // "UF2 Bootloader ..." is not a key even though the line is first.
        assert_eq!(info.keys.len(), 4);
    }

    #[test]
    fn the_rak_file_parses_including_the_key_we_do_not_know() {
        let info = parse(RAK);
        assert_eq!(info.board_id(), Some("WisBlock-RAK4631-Board"));
        assert_eq!(info.get("Ver"), Some("0.4.3"));
        assert_eq!(info.softdevice().unwrap().version, Version::new(7, 3, 0));
        assert_eq!(info.keys.len(), 5);
    }

    #[test]
    fn the_two_real_files_are_told_apart_by_board_id_alone() {
        assert_ne!(parse(T114).board_id(), parse(RAK).board_id());
        assert_eq!(parse(T114).board_id(), Some("HT-n5262"));
    }

    #[test]
    fn a_bootloader_too_old_to_report_a_softdevice_still_parses() {
        let text = T114.replace("SoftDevice: S140 7.3.0\r\n", "");
        let info = parse(&text);
        assert_eq!(info.board_id(), Some("HT-n5262"));
        assert_eq!(info.softdevice(), None);
    }

    #[test]
    fn lf_only_line_endings_parse_the_same_as_crlf() {
        assert_eq!(parse(&T114.replace("\r\n", "\n")), parse(T114));
    }

    #[test]
    fn an_unreadable_softdevice_line_reads_as_absent_not_as_a_version() {
        let text = T114.replace("S140 7.3.0", "S140 unknown");
        assert_eq!(parse(&text).softdevice(), None);
    }

    #[test]
    fn keys_are_matched_case_insensitively() {
        assert_eq!(parse(T114).get("board-id"), Some("HT-n5262"));
    }

    #[test]
    fn an_empty_file_yields_no_board_id_rather_than_a_wrong_one() {
        let info = parse("");
        assert_eq!(info.board_id(), None);
        assert_eq!(info.banner, None);
    }
}
