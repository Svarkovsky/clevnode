//! The **radio** axis: what the board comes up on after the flash.
//!
//! A freshly flashed LNode boots the compiled `eu_medium` profile until
//! somebody tells it otherwise. Since commit `f43184b` it remembers what it
//! is told — the configuration goes into its own flash page and survives
//! both a reset and the next UF2 write — so the honest place to choose a
//! frequency is at flash time, once, rather than in every host that ever
//! binds the board.
//!
//! The mechanism is the one `lnsd` already uses, and it is reused rather
//! than re-implemented: the magic-prefixed control frame from
//! [`leviculum_core::rnode`], HDLC-framed by
//! [`leviculum_core::framing::hdlc`], written to the transport CDC, followed
//! by the board's `RADIO_CONFIG_ACK`. There is exactly one wire format for
//! this, and a second copy of it here would be one copy too many.
//!
//! Two fields are not the user's to choose and are worth naming.
//!
//! **Preamble** is derived, not asked for: the RNode firmware picks it from
//! the PHY (`derive_preamble_symbols`), and a host that sends a preamble
//! belonging to some other SF mis-prices every airtime calculation on both
//! sides.
//!
//! **The long-term airtime lock** is sent explicitly at the value the
//! firmware would have derived for the chosen frequency. This is not
//! busywork. A 21-byte frame sets `lt_alock_present`, and the firmware reads
//! that as "the host has an opinion", which switches off its own lawful
//! ETSI default. Sending `0` would therefore persist "no duty-cycle limit"
//! onto a board the operator only asked to put on 869.463 MHz. Sending
//! `firmware_default_lt_alock(freq, None)` persists the same cap the
//! firmware would have chosen for itself.

use std::io;
use std::time::Duration;

use leviculum_core::framing::hdlc::frame;
use leviculum_core::rnode::{
    build_radio_config_frame, derive_preamble_symbols, firmware_default_lt_alock, RadioConfigWire,
    RADIO_CONFIG_ACK,
};

use crate::sys::Fd;

/// `bInterfaceNumber` of the transport CDC, the port the control frame goes
/// to. if00 is the debug log (see [`crate::verify`]), if02 the transport —
/// one composite descriptor in `leviculum-nrf/src/usb.rs` for every board we
/// flash, which is why this is a constant rather than a manifest key.
pub const TRANSPORT_INTERFACE: u8 = 2;

/// How many times the frame is sent before giving up. The board answers in
/// milliseconds when it answers at all, so retries are for a USB stack that
/// was still settling, not for a slow board.
pub const ATTEMPTS: u8 = 3;

/// How long one attempt waits for the ACK. Three of these is the ~10 s
/// budget the whole step is allowed.
pub const ACK_WITHIN: Duration = Duration::from_millis(3500);

/// The bandwidths the SX1262 has a register code for (datasheet table
/// 14-47). Anything else is refused here rather than by a board that answers
/// nothing, because `RadioConfig::from_wire_config` returns `None` for an
/// unknown bandwidth and a rejected frame is indistinguishable from a dead
/// port.
pub const BANDWIDTHS_HZ: [u32; 10] = [
    7_810, 10_420, 15_630, 20_830, 31_250, 41_670, 62_500, 125_000, 250_000, 500_000,
];

/// The tuning range of the SX1262 (datasheet §1: 150-960 MHz). Wide on
/// purpose: this tool is not a regulator, and hard-coding one region is the
/// thing this whole batch exists to stop.
pub const FREQUENCY_HZ: std::ops::RangeInclusive<u32> = 150_000_000..=960_000_000;

/// What the SX1262's PA can be asked for, in dBm (datasheet §5.1: -9 to +22
/// on the high-power PA).
pub const TX_POWER_DBM: std::ops::RangeInclusive<i8> = -9..=22;

/// Spreading factors the firmware configures. The wire parser accepts SF5
/// and SF6, which the modem only reaches in a mode we do not drive.
pub const SPREADING_FACTORS: std::ops::RangeInclusive<u8> = 7..=12;

/// Coding-rate denominators: 4/5 through 4/8.
pub const CODING_RATES: std::ops::RangeInclusive<u8> = 5..=8;

/// One PHY, as a person states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioSettings {
    pub frequency_hz: u32,
    pub bandwidth_hz: u32,
    pub sf: u8,
    /// Coding-rate denominator, 5 for 4/5.
    pub cr: u8,
    pub tx_power_dbm: i8,
}

/// What a board gets unless the user says otherwise: the ReticulumNet NL
/// consensus profile the firmware compiles in (`RadioConfig::eu_medium`),
/// stated here so the flash-time choice and the compiled default cannot
/// drift apart silently — `the_default_matches_the_firmwares_compiled_profile`
/// asserts the numbers.
pub const EU868: RadioSettings = RadioSettings {
    // Verbatim the agreed number, 869463000, not a rounder 869462500: the
    // value of a consensus is the identity of its number.
    frequency_hz: 869_463_000,
    bandwidth_hz: 125_000,
    sf: 8,
    cr: 5,
    tx_power_dbm: leviculum_core::rnode::DEFAULT_TX_POWER_DBM,
};

/// The us915 note, printed whenever us915 is chosen — visibly, not as a
/// docs footnote, because the preset follows the community rather than the
/// regulation and the person keying it in is the one who has to know.
pub const US915_CAVEAT: &str =
    "us915 uses the common community 125 kHz setting, which does NOT meet FCC 15.247(a)(2) \
     (>=500 kHz occupied bandwidth for non-hopping systems). You are choosing the widely-used \
     community profile; ensure this is lawful for your deployment.";

/// Why eu433 is in the table but cannot be chosen: the SX1262 driver's
/// lowest PA profile (`leviculum-nrf/src/sx1262.rs`) is 14 dBm, and ERC
/// 70-03 allows 10 mW e.r.p. at 433 MHz. Offering it would transmit 4 dB
/// over the limit without a word.
pub const EU433_UNAVAILABLE: &str =
    "eu433 needs 10 dBm output, which this radio driver cannot yet produce (lowest PA profile \
     is 14 dBm); not offered.";

/// One region preset: a community profile under a name people use, not a
/// conformance claim — which is why the menu label spells the community and
/// the caveat is part of the definition rather than of some caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresetDef {
    /// The flag value: `--radio-preset <name>`.
    pub name: &'static str,
    /// How the menu shows it: a community profile, not a regulatory claim.
    pub menu_label: &'static str,
    pub settings: RadioSettings,
    /// Printed when this preset is chosen, menu or flag.
    pub caveat: Option<&'static str>,
    /// Why this preset cannot be chosen yet; `None` means it ships.
    pub unavailable: Option<&'static str>,
}

/// Every preset the tool knows, shipped or waiting. Values are the
/// community consensus per region (the Reticulum wiki's "Popular RNode
/// Settings"), decided in #75 part 2b — not invented here.
pub const PRESETS: [PresetDef; 4] = [
    PresetDef {
        name: "eu868",
        menu_label: "eu868 (ReticulumNet consensus)",
        // The compiled firmware default: one definition, so the preset and
        // the default cannot disagree.
        settings: EU868,
        caveat: None,
        unavailable: None,
    },
    PresetDef {
        name: "us915",
        menu_label: "us915 (US community — see FCC note)",
        settings: RadioSettings {
            frequency_hz: 914_875_000,
            bandwidth_hz: 125_000,
            sf: 8,
            cr: 5,
            tx_power_dbm: 22,
        },
        caveat: Some(US915_CAVEAT),
        unavailable: None,
    },
    PresetDef {
        name: "au915",
        menu_label: "au915",
        settings: RadioSettings {
            frequency_hz: 925_875_000,
            bandwidth_hz: 250_000,
            sf: 9,
            cr: 5,
            tx_power_dbm: 22,
        },
        caveat: None,
        unavailable: None,
    },
    PresetDef {
        name: "eu433",
        menu_label: "eu433",
        settings: RadioSettings {
            frequency_hz: 433_575_000,
            bandwidth_hz: 125_000,
            sf: 8,
            cr: 5,
            // ERC 70-03: 10 mW e.r.p. The reason it is unavailable: the PA
            // driver cannot produce this yet.
            tx_power_dbm: 10,
        },
        caveat: None,
        unavailable: Some(EU433_UNAVAILABLE),
    },
];

/// The presets a user can actually pick, in menu order.
pub fn selectable() -> impl Iterator<Item = &'static PresetDef> {
    PRESETS.iter().filter(|p| p.unavailable.is_none())
}

/// Why `--radio-preset <name>` was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    #[error(
        "{0:?} is not a preset; the presets are: {choices}",
        choices = PRESETS
            .iter()
            .filter(|p| p.unavailable.is_none())
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(", ")
    )]
    Unknown(String),
    #[error("{0}")]
    Unavailable(&'static str),
}

/// Look a preset up by its flag name. An unavailable preset is refused with
/// its reason rather than shipped with the wrong power.
pub fn preset(name: &str) -> Result<&'static PresetDef, PresetError> {
    let Some(def) = PRESETS.iter().find(|p| p.name == name) else {
        return Err(PresetError::Unknown(name.to_string()));
    };
    match def.unavailable {
        Some(why) => Err(PresetError::Unavailable(why)),
        None => Ok(def),
    }
}

/// Why a value was not accepted. Every variant says the offending value and
/// what would have been allowed, because a re-prompt that only says "no" is
/// a re-prompt the user answers the same way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    #[error(
        "{text:?} is not a number for {field}",
        field = field.label()
    )]
    NotANumber { field: Field, text: String },
    #[error(
        "{0} Hz is outside {low}-{high} MHz, which is what the SX1262 tunes",
        low = FREQUENCY_HZ.start() / 1_000_000,
        high = FREQUENCY_HZ.end() / 1_000_000
    )]
    Frequency(u32),
    #[error(
        "{0} Hz is not a bandwidth the SX1262 has a setting for. It takes: {choices}",
        choices = BANDWIDTHS_HZ.map(|hz| hz.to_string()).join(", ")
    )]
    Bandwidth(u32),
    #[error(
        "SF{0} is outside SF{low}-SF{high}",
        low = SPREADING_FACTORS.start(),
        high = SPREADING_FACTORS.end()
    )]
    SpreadingFactor(u8),
    #[error(
        "4/{0} is not a coding rate; the denominator runs {low}-{high}",
        low = CODING_RATES.start(),
        high = CODING_RATES.end()
    )]
    CodingRate(u8),
    #[error(
        "{0} dBm is outside {low} to {high} dBm, which is what the PA delivers",
        low = TX_POWER_DBM.start(),
        high = TX_POWER_DBM.end()
    )]
    TxPower(i32),
}

/// One field of a [`RadioSettings`], so the prompts and the command-line
/// flags validate through the same code rather than two lists that drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Frequency,
    Bandwidth,
    SpreadingFactor,
    CodingRate,
    TxPower,
}

/// The order they are asked in.
pub const FIELDS: [Field; 5] = [
    Field::Frequency,
    Field::Bandwidth,
    Field::SpreadingFactor,
    Field::CodingRate,
    Field::TxPower,
];

impl Field {
    /// What the prompt calls it.
    pub fn label(&self) -> &'static str {
        match self {
            Field::Frequency => "frequency (Hz)",
            Field::Bandwidth => "bandwidth (Hz)",
            Field::SpreadingFactor => "spreadingfactor",
            Field::CodingRate => "codingrate",
            Field::TxPower => "txpower (dBm)",
        }
    }

    /// This field's value in `settings`, as the prompt shows it.
    pub fn value_of(&self, settings: &RadioSettings) -> String {
        match self {
            Field::Frequency => settings.frequency_hz.to_string(),
            Field::Bandwidth => settings.bandwidth_hz.to_string(),
            Field::SpreadingFactor => settings.sf.to_string(),
            Field::CodingRate => settings.cr.to_string(),
            Field::TxPower => settings.tx_power_dbm.to_string(),
        }
    }

    /// Parse `text` into this field and check it. Nothing is written to
    /// `settings` unless both succeed, so a rejected answer leaves the value
    /// the next prompt offers as its default intact.
    pub fn apply(&self, settings: &mut RadioSettings, text: &str) -> Result<(), Invalid> {
        let text = text.trim();
        let not_a_number = || Invalid::NotANumber {
            field: *self,
            text: text.to_string(),
        };
        match self {
            Field::Frequency => {
                let hz: u32 = text.parse().map_err(|_| not_a_number())?;
                check_frequency(hz)?;
                settings.frequency_hz = hz;
            }
            Field::Bandwidth => {
                let hz: u32 = text.parse().map_err(|_| not_a_number())?;
                check_bandwidth(hz)?;
                settings.bandwidth_hz = hz;
            }
            Field::SpreadingFactor => {
                let sf: u8 = text.parse().map_err(|_| not_a_number())?;
                check_sf(sf)?;
                settings.sf = sf;
            }
            Field::CodingRate => {
                let cr: u8 = text.parse().map_err(|_| not_a_number())?;
                check_cr(cr)?;
                settings.cr = cr;
            }
            Field::TxPower => {
                let dbm: i32 = text.parse().map_err(|_| not_a_number())?;
                check_tx_power(dbm)?;
                settings.tx_power_dbm = dbm as i8;
            }
        }
        Ok(())
    }
}

pub fn check_frequency(hz: u32) -> Result<(), Invalid> {
    FREQUENCY_HZ
        .contains(&hz)
        .then_some(())
        .ok_or(Invalid::Frequency(hz))
}

pub fn check_bandwidth(hz: u32) -> Result<(), Invalid> {
    BANDWIDTHS_HZ
        .contains(&hz)
        .then_some(())
        .ok_or(Invalid::Bandwidth(hz))
}

pub fn check_sf(sf: u8) -> Result<(), Invalid> {
    SPREADING_FACTORS
        .contains(&sf)
        .then_some(())
        .ok_or(Invalid::SpreadingFactor(sf))
}

pub fn check_cr(cr: u8) -> Result<(), Invalid> {
    CODING_RATES
        .contains(&cr)
        .then_some(())
        .ok_or(Invalid::CodingRate(cr))
}

pub fn check_tx_power(dbm: i32) -> Result<(), Invalid> {
    i8::try_from(dbm)
        .ok()
        .filter(|dbm| TX_POWER_DBM.contains(dbm))
        .map(|_| ())
        .ok_or(Invalid::TxPower(dbm))
}

impl RadioSettings {
    /// Every field, checked. What the flags go through, so an impossible
    /// `--radio-sf 3` stops the run before a board is touched rather than
    /// after it has been written.
    pub fn check(&self) -> Result<(), Invalid> {
        check_frequency(self.frequency_hz)?;
        check_bandwidth(self.bandwidth_hz)?;
        check_sf(self.sf)?;
        check_cr(self.cr)?;
        check_tx_power(self.tx_power_dbm as i32)
    }

    /// The preamble the firmware would derive for this PHY.
    pub fn preamble_symbols(&self) -> u16 {
        derive_preamble_symbols(self.sf, self.cr, self.bandwidth_hz)
    }

    /// The long-term airtime lock that goes on the wire: the lawful default
    /// for this frequency, sent explicitly because the frame's presence
    /// switches the firmware's own derivation off (see the module note).
    pub fn lt_alock(&self) -> u16 {
        firmware_default_lt_alock(self.frequency_hz as u64, None)
    }

    /// The wire form of these settings.
    pub fn to_wire(&self) -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: self.frequency_hz,
            bandwidth_hz: self.bandwidth_hz,
            sf: self.sf,
            cr: self.cr,
            tx_power_dbm: self.tx_power_dbm,
            preamble_len: self.preamble_symbols(),
            // What `RadioConfig::eu_medium` compiles in, and what a board on
            // a shared channel has to do.
            csma_enabled: true,
            // Only the integration runner ever wants a mute board, and it
            // sets it at run time.
            radio_silent: false,
            // No short-term lock: nothing in this tool has a basis for one.
            st_alock: 0,
            lt_alock: self.lt_alock(),
            lt_alock_present: true,
        }
    }

    /// The bytes that go down the port: the control frame, HDLC-framed.
    pub fn framed(&self) -> Vec<u8> {
        let mut out = Vec::new();
        frame(&build_radio_config_frame(&self.to_wire()), &mut out);
        out
    }

    /// One line of `key=value`, for the transcript and for grep.
    pub fn describe(&self) -> String {
        format!(
            "freq={} bw={} sf={} cr=4/{} txpower={}dBm preamble={} lt_alock={}",
            self.frequency_hz,
            self.bandwidth_hz,
            self.sf,
            self.cr,
            self.tx_power_dbm,
            self.preamble_symbols(),
            self.lt_alock()
        )
    }
}

/// What the user chose. The two arms are the whole surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioChoice {
    /// A named preset from [`PRESETS`], sent rather than assumed so what the
    /// board has stored is a fact somebody chose, not a compiled-in
    /// leftover. The default answer is the eu868 entry.
    Preset(&'static PresetDef),
    /// Field by field, from the prompts or from the flags.
    Custom(RadioSettings),
}

impl RadioChoice {
    pub fn settings(&self) -> RadioSettings {
        match self {
            RadioChoice::Preset(preset) => preset.settings,
            RadioChoice::Custom(settings) => *settings,
        }
    }
}

/// How the radio step is reached.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RadioPlan {
    /// Ask the user. `--yes` answers "the defaults", because `Assumed` takes
    /// every default rather than blocking on a prompt nobody will see.
    #[default]
    Ask,
    /// Already decided on the command line; do not ask.
    Fixed(RadioChoice),
    /// Do not touch the radio configuration at all.
    Skip,
}

/// The dialect decision on an already-opened port (#238): one capability
/// probe picks between the envelope config frame (firmware that answers
/// the probe) and the legacy magic (firmware from before the envelope).
///
/// `Ok(false)` is "the board did not answer" — a fact for the caller to
/// report, not a failure of the flash, which has already happened by the
/// time this runs. Takes the fd rather than a path so the open lives with
/// the caller, which proves the port still belongs to the board it was
/// resolved for — and so the scripted-pty tests drive the same code the
/// flash flow runs.
pub fn send_configured(fd: &Fd, settings: &RadioSettings) -> io::Result<bool> {
    use crate::envelope::{self, ControlOutcome};
    match envelope::probe_capabilities(fd)? {
        Some(caps) if caps.accepts(leviculum_core::envelope::TYPE_RADIO_CONFIG) => Ok(matches!(
            envelope::send_radio_config(fd, &settings.to_wire())?,
            ControlOutcome::Acked
        )),
        // No capability report (firmware older than the envelope), or a
        // report without the config type: the legacy magic frame, accepted
        // through the transition window.
        _ => {
            let acked = envelope::transact(
                fd,
                &build_radio_config_frame(&settings.to_wire()),
                envelope::CONTROL_TIMING,
                |data| (data == RADIO_CONFIG_ACK).then_some(()),
            )?;
            Ok(acked.is_some())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::framing::hdlc::{DeframeResult, Deframer};
    use leviculum_core::rnode::parse_radio_config;

    /// Take the bytes apart the way the firmware does: deframe, check the
    /// magic, parse the payload.
    fn round_trip(framed: &[u8]) -> RadioConfigWire {
        let mut deframer = Deframer::new();
        let results = deframer.process(framed);
        let mut frames: Vec<Vec<u8>> = results
            .into_iter()
            .filter_map(|r| match r {
                DeframeResult::Frame(data) => Some(data),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 1, "one frame in, one frame out");
        let payload = frames.remove(0);
        assert_eq!(
            payload.len(),
            leviculum_core::rnode::RADIO_CONFIG_FRAME_LEN,
            "the firmware compares the length before it parses anything"
        );
        assert_eq!(
            &payload[..2],
            &leviculum_core::rnode::RADIO_CONFIG_MAGIC[..]
        );
        parse_radio_config(&payload[2..]).expect("the firmware parses what we send")
    }

    #[test]
    fn a_chosen_configuration_reaches_the_board_as_the_values_that_were_chosen() {
        let chosen = RadioSettings {
            frequency_hz: 867_100_000,
            bandwidth_hz: 250_000,
            sf: 9,
            cr: 7,
            tx_power_dbm: 14,
        };
        let wire = round_trip(&chosen.framed());
        assert_eq!(wire.frequency_hz, 867_100_000);
        assert_eq!(wire.bandwidth_hz, 250_000);
        assert_eq!(wire.sf, 9);
        assert_eq!(wire.cr, 7);
        assert_eq!(wire.tx_power_dbm, 14);
        // Derived, not asked for, and it has to be the derivation the
        // firmware itself would have made for this PHY.
        assert_eq!(wire.preamble_len, derive_preamble_symbols(9, 7, 250_000));
        assert!(wire.csma_enabled);
        assert!(!wire.radio_silent);
    }

    #[test]
    fn the_default_matches_the_firmwares_compiled_profile() {
        // `RadioConfig::eu_medium()`: the ReticulumNet NL consensus,
        // 869.463 MHz, SF8, BW125, CR4/5, 22 dBm, preamble derived to 18.
        // Sending the defaults must be a no-op in effect, or "flash the
        // defaults" would quietly change a board. The TX power is the board
        // maximum, the same value the host resolves an absent `txpower` to
        // (`rnode::DEFAULT_TX_POWER_DBM`) — this assertion is what keeps the
        // flash-time choice and the compiled profile from drifting apart.
        let wire = round_trip(&EU868.framed());
        assert_eq!(wire.frequency_hz, 869_463_000);
        assert_eq!(wire.bandwidth_hz, 125_000);
        assert_eq!(wire.sf, 8);
        assert_eq!(wire.cr, 5);
        assert_eq!(wire.tx_power_dbm, 22);
        assert_eq!(wire.preamble_len, 18);
    }

    #[test]
    fn the_eu868_preset_is_the_reticulumnet_consensus() {
        let def = preset("eu868").unwrap();
        assert_eq!(
            def.settings,
            RadioSettings {
                frequency_hz: 869_463_000,
                bandwidth_hz: 125_000,
                sf: 8,
                cr: 5,
                tx_power_dbm: 22,
            }
        );
        // One definition: the preset IS the compiled default.
        assert_eq!(def.settings, EU868);
        assert_eq!(def.menu_label, "eu868 (ReticulumNet consensus)");
        assert_eq!(def.caveat, None);
        // Inside ERC 70-03 h1.7: the ETSI 10% duty cycle applies, and the
        // gap check does not refuse the channel's BW125 occupied width.
        assert_eq!(def.settings.lt_alock(), 1000);
        assert_eq!(
            leviculum_core::rnode::erp_band_gap(869_463_000, 125_000),
            None
        );
    }

    #[test]
    fn the_us915_preset_is_the_community_profile_with_its_fcc_note() {
        let def = preset("us915").unwrap();
        assert_eq!(
            def.settings,
            RadioSettings {
                frequency_hz: 914_875_000,
                bandwidth_hz: 125_000,
                sf: 8,
                cr: 5,
                tx_power_dbm: 22,
            }
        );
        let caveat = def.caveat.expect("us915 carries the FCC note");
        assert!(caveat.contains("15.247(a)(2)"), "{caveat}");
        assert!(caveat.contains("500 kHz"), "{caveat}");
        // No US duty cycle: the derived long-term lock is off.
        assert_eq!(def.settings.lt_alock(), 0);
    }

    #[test]
    fn the_au915_preset_is_the_community_profile() {
        let def = preset("au915").unwrap();
        assert_eq!(
            def.settings,
            RadioSettings {
                frequency_hz: 925_875_000,
                bandwidth_hz: 250_000,
                sf: 9,
                cr: 5,
                tx_power_dbm: 22,
            }
        );
        assert_eq!(def.caveat, None);
        assert_eq!(def.settings.lt_alock(), 0);
    }

    #[test]
    fn eu433_is_in_the_table_but_refused_by_name() {
        // The tuple is decided (ERC 70-03: 10 mW e.r.p.) and waits only on a
        // PA profile below 14 dBm; the table carries it so the decision is
        // not re-researched when the driver catches up.
        let def = PRESETS.iter().find(|p| p.name == "eu433").unwrap();
        assert_eq!(
            def.settings,
            RadioSettings {
                frequency_hz: 433_575_000,
                bandwidth_hz: 125_000,
                sf: 8,
                cr: 5,
                tx_power_dbm: 10,
            }
        );
        let err = preset("eu433").unwrap_err();
        assert_eq!(err, PresetError::Unavailable(EU433_UNAVAILABLE));
        let said = format!("{err}");
        assert!(said.contains("10 dBm"), "{said}");
        assert!(said.contains("14"), "{said}");
        assert!(said.contains("not offered"), "{said}");
        // And it is not in the menu.
        assert!(selectable().all(|p| p.name != "eu433"));
    }

    #[test]
    fn an_unknown_preset_name_is_refused_with_the_choices() {
        let err = preset("as923").unwrap_err();
        assert_eq!(err, PresetError::Unknown("as923".to_string()));
        let said = format!("{err}");
        assert!(said.contains("as923"), "{said}");
        assert!(said.contains("eu868, us915, au915"), "{said}");
    }

    #[test]
    fn the_menu_offers_the_shipped_presets_in_the_decided_order() {
        let names: Vec<_> = selectable().map(|p| p.name).collect();
        assert_eq!(names, ["eu868", "us915", "au915"]);
    }

    #[test]
    fn every_preset_derives_the_same_preamble_floor() {
        // SF8/BW125 and SF9/BW250 both sit at 2.048 ms symbol time, so all
        // four presets land on the 18-symbol minimum — the control that the
        // derivation path, not a fixed number, is what goes on the wire.
        for def in &PRESETS {
            assert_eq!(def.settings.preamble_symbols(), 18, "{}", def.name);
        }
    }

    #[test]
    fn a_negative_transmit_power_survives_the_wire() {
        // tx_power rides as one byte reinterpreted as i8; -9 is the bottom of
        // the PA's range and the value most likely to come back as 247.
        let quiet = RadioSettings {
            tx_power_dbm: -9,
            ..EU868
        };
        assert_eq!(round_trip(&quiet.framed()).tx_power_dbm, -9);
    }

    #[test]
    fn the_lawful_long_term_lock_is_sent_rather_than_switched_off() {
        // The trap: a 21-byte frame sets lt_alock_present, which turns the
        // firmware's own ETSI derivation off. Sending 0 would persist "no
        // duty cycle limit" onto a board that only asked for a frequency.
        let wire = round_trip(&EU868.framed());
        assert!(wire.lt_alock_present);
        // 869.463 MHz is sub-band P: 10%, which is 1000 in the u16 encoding.
        assert_eq!(wire.lt_alock, 1000);
        assert_eq!(wire.st_alock, 0);

        // 868.1 MHz is sub-band M: 1%.
        let m = RadioSettings {
            frequency_hz: 868_100_000,
            ..EU868
        };
        assert_eq!(round_trip(&m.framed()).lt_alock, 100);

        // Outside the EU band there is no cap to derive, and the firmware
        // would have arrived at the same 0.
        let us = RadioSettings {
            frequency_hz: 915_000_000,
            ..EU868
        };
        assert_eq!(round_trip(&us.framed()).lt_alock, 0);
    }

    #[test]
    fn a_frame_carrying_a_flag_byte_is_escaped_and_still_parses() {
        // 0x7E (the flag) and 0x7D (the escape) inside the payload are what
        // HDLC framing exists for, and a hand-rolled framer is where they get
        // forgotten. 0x337E7D40 Hz = 863.927.616 Hz puts both in the
        // frequency field.
        let awkward = RadioSettings {
            frequency_hz: 0x337E_7D40,
            ..EU868
        };
        let framed = awkward.framed();
        let body = &framed[1..framed.len() - 1];
        assert!(
            !body.contains(&0x7E),
            "a raw flag inside the frame would end it early: {body:02x?}"
        );
        assert!(body.contains(&0x7D), "both bytes have to be escaped");
        assert_eq!(round_trip(&framed).frequency_hz, 0x337E_7D40);
    }

    #[test]
    fn a_spreading_factor_the_firmware_cannot_configure_is_refused() {
        let mut settings = EU868;
        for bad in ["6", "13", "0"] {
            let err = Field::SpreadingFactor
                .apply(&mut settings, bad)
                .unwrap_err();
            assert!(matches!(err, Invalid::SpreadingFactor(_)), "{err}");
            assert!(format!("{err}").contains("SF7-SF12"), "{err}");
        }
        assert_eq!(settings.sf, EU868.sf, "a refusal must not half-apply");
        assert!(Field::SpreadingFactor.apply(&mut settings, "12").is_ok());
        assert_eq!(settings.sf, 12);
    }

    #[test]
    fn a_coding_rate_outside_four_fifths_to_four_eighths_is_refused() {
        let mut settings = EU868;
        for bad in ["4", "9"] {
            assert!(matches!(
                Field::CodingRate.apply(&mut settings, bad),
                Err(Invalid::CodingRate(_))
            ));
        }
        assert!(Field::CodingRate.apply(&mut settings, "8").is_ok());
        assert_eq!(settings.cr, 8);
    }

    #[test]
    fn a_bandwidth_the_modem_has_no_register_code_for_is_refused_here() {
        // 100 kHz reads like a plausible bandwidth and is not one. Left to
        // the board it would be a frame silently dropped by
        // `RadioConfig::from_wire_config`, which looks exactly like a dead
        // port.
        let mut settings = EU868;
        let err = Field::Bandwidth.apply(&mut settings, "100000").unwrap_err();
        assert!(matches!(err, Invalid::Bandwidth(100_000)));
        assert!(format!("{err}").contains("125000"), "{err}");
        assert!(Field::Bandwidth.apply(&mut settings, "62500").is_ok());
        assert_eq!(settings.bandwidth_hz, 62_500);
    }

    #[test]
    fn a_frequency_no_sx1262_tunes_is_refused() {
        let mut settings = EU868;
        for bad in ["100000000", "2400000000"] {
            let err = Field::Frequency.apply(&mut settings, bad).unwrap_err();
            assert!(
                matches!(err, Invalid::Frequency(_) | Invalid::NotANumber { .. }),
                "{err}"
            );
        }
        assert!(Field::Frequency.apply(&mut settings, "433175000").is_ok());
        assert_eq!(settings.frequency_hz, 433_175_000);
    }

    #[test]
    fn a_transmit_power_the_pa_cannot_deliver_is_refused() {
        let mut settings = EU868;
        for bad in ["23", "-10", "200"] {
            assert!(
                matches!(
                    Field::TxPower.apply(&mut settings, bad),
                    Err(Invalid::TxPower(_))
                ),
                "{bad} was accepted"
            );
        }
        assert!(Field::TxPower.apply(&mut settings, "-9").is_ok());
        assert_eq!(settings.tx_power_dbm, -9);
        assert!(Field::TxPower.apply(&mut settings, "22").is_ok());
    }

    #[test]
    fn something_that_is_not_a_number_is_refused_by_name() {
        let mut settings = EU868;
        let err = Field::Frequency
            .apply(&mut settings, "869.525")
            .unwrap_err();
        assert!(matches!(err, Invalid::NotANumber { .. }));
        assert!(format!("{err}").contains("frequency (Hz)"), "{err}");
        assert!(Field::Bandwidth.apply(&mut settings, "").is_err());
    }

    #[test]
    fn the_flags_go_through_the_same_check_as_the_prompts() {
        assert!(EU868.check().is_ok());
        assert!(RadioSettings { sf: 5, ..EU868 }.check().is_err());
        assert!(RadioSettings {
            bandwidth_hz: 100_000,
            ..EU868
        }
        .check()
        .is_err());
        assert!(RadioSettings {
            tx_power_dbm: 30,
            ..EU868
        }
        .check()
        .is_err());
    }

    #[test]
    fn the_prompt_default_of_a_field_is_the_value_that_field_holds() {
        assert_eq!(Field::Frequency.value_of(&EU868), "869463000");
        assert_eq!(Field::Bandwidth.value_of(&EU868), "125000");
        assert_eq!(Field::SpreadingFactor.value_of(&EU868), "8");
        assert_eq!(Field::CodingRate.value_of(&EU868), "5");
        assert_eq!(Field::TxPower.value_of(&EU868), "22");
    }

    #[test]
    fn a_choice_resolves_to_the_settings_that_were_chosen() {
        assert_eq!(
            RadioChoice::Preset(preset("eu868").unwrap()).settings(),
            EU868
        );
        assert_eq!(
            RadioChoice::Preset(preset("us915").unwrap())
                .settings()
                .frequency_hz,
            914_875_000
        );
        let custom = RadioSettings { sf: 11, ..EU868 };
        assert_eq!(RadioChoice::Custom(custom).settings(), custom);
        assert_eq!(RadioPlan::default(), RadioPlan::Ask);
    }

    #[test]
    fn the_transcript_line_names_every_value_that_went_on_the_wire() {
        let said = EU868.describe();
        for expected in [
            "freq=869463000",
            "bw=125000",
            "sf=8",
            "cr=4/5",
            "txpower=22dBm",
            "preamble=18",
            "lt_alock=1000",
        ] {
            assert!(said.contains(expected), "{expected} missing from {said}");
        }
    }

    #[test]
    fn the_config_rides_the_envelope_when_the_firmware_answers_the_probe() {
        use crate::sys::testpty::{spawn_stub, Pty};
        use leviculum_core::envelope::{self, classify_control_frame, ControlAction};
        use std::sync::{Arc, Mutex};

        let accepted: &[u8] = &[envelope::TYPE_RADIO_CONFIG, envelope::TYPE_CAPABILITIES];
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let pty = Pty::open();
        spawn_stub(&pty, move |frame_bytes| {
            record.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, accepted) {
                ControlAction::CapabilityQuery => {
                    Some(envelope::encode_capability_report(accepted))
                }
                ControlAction::RadioConfig(_) => {
                    Some(envelope::encode_ack(envelope::TYPE_RADIO_CONFIG))
                }
                _ => None,
            }
        });

        let settings = EU868;
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert!(send_configured(&fd, &settings).unwrap());

        // Migration equivalence, host side: what went over the wire is an
        // envelope config frame that parses to exactly the configuration a
        // legacy magic frame would have carried.
        let seen = seen.lock().unwrap();
        let sent_config = seen
            .iter()
            .find_map(|data| match classify_control_frame(data, accepted) {
                ControlAction::RadioConfig(wire) => Some(wire),
                _ => None,
            })
            .expect("an envelope config frame was sent");
        assert_eq!(sent_config, settings.to_wire());
    }

    #[test]
    fn the_config_falls_back_to_the_legacy_magic_for_old_firmware() {
        use crate::sys::testpty::{spawn_stub, Pty};
        use leviculum_core::envelope::{classify_control_frame, ControlAction};
        use leviculum_core::rnode::RADIO_CONFIG_MAGIC;
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let pty = Pty::open();
        // Firmware from before the envelope: the legacy magic is the only
        // control frame it answers; the probe runs into scripted silence.
        spawn_stub(&pty, move |frame_bytes| {
            record.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, &[]) {
                ControlAction::LegacyRadioConfig(_) => Some(RADIO_CONFIG_ACK.to_vec()),
                _ => None,
            }
        });

        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert!(send_configured(&fd, &EU868).unwrap());

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|data| data.len() == 21 && data[..2] == RADIO_CONFIG_MAGIC),
            "the legacy magic frame was sent after the probe went unanswered"
        );
    }
}
