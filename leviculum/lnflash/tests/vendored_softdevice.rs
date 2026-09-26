//! The parsers, run against the real vendored S140 image rather than against
//! a fixture shaped like one.
//!
//! Every number asserted here was measured on the rig on 2026-08-09 and is
//! recorded in `docs/src/concepts/lnode-flashing.md`. That makes this file a
//! two-way check: it fails if the parser is wrong, and it fails if the
//! concept doc is wrong. A disagreement is a finding to resolve, never a
//! constant to adjust until the test passes.
//!
//! Needs no hardware — the image is in the tree.

use lnflash::ihex;
use lnflash::softdevice::{self, Version};
use lnflash::uf2::{self, Image};

const SOFTDEVICE_HEX: &str = include_str!("../payload/t114/s140_nrf52_7.3.0_softdevice.hex");

/// Everything below `USER_FLASH_START` is declined by the bootloader.
const USER_FLASH_START: u32 = 0x1000;
/// Where our application lives, and the line the SoftDevice must stay below.
const APP_BASE: u32 = 0x2_7000;

fn spans() -> Vec<ihex::Span> {
    ihex::parse(SOFTDEVICE_HEX).expect("the vendored S140 hex parses")
}

fn converted() -> Image {
    Image::from_spans(&spans(), uf2::FAMILY_NRF52840_APP)
}

#[test]
fn the_hex_covers_the_mbr_and_the_softdevice_and_nothing_else() {
    let spans = spans();
    let ranges: Vec<(u32, u32)> = spans.iter().map(|s| (s.start, s.end())).collect();
    assert_eq!(
        ranges,
        vec![(0x0, 0xB00), (0x1000, 0x2_6498)],
        "measured 2026-08-09: MBR then the SoftDevice itself"
    );
}

#[test]
fn the_softdevice_span_is_the_measured_hundred_and_forty_nine_kib() {
    let sd = &spans()[1];
    assert_eq!(sd.len(), 152_728);
    assert_eq!(sd.len() / 1024, 149);
}

#[test]
fn conversion_produces_the_measured_six_hundred_and_eight_blocks() {
    let image = converted();
    assert_eq!(image.blocks.len(), 608);
    assert_eq!(image.encode().unwrap().len(), 311_296);
}

#[test]
fn the_converted_image_carries_the_application_family() {
    assert_eq!(converted().family_id(), Some(uf2::FAMILY_NRF52840_APP));
    assert_ne!(uf2::FAMILY_NRF52840_APP, uf2::FAMILY_NRF52_BOOTLOADER);
}

#[test]
fn the_highest_byte_touched_stays_below_the_application_base() {
    // This is the row that matters: the update cannot reach the application,
    // which is why an installed application survives a SoftDevice update.
    let (low, high) = converted().address_range().unwrap();
    assert_eq!((low, high), (0x0, 0x2_6500));
    assert!(high < APP_BASE);
}

#[test]
fn exactly_eleven_blocks_fall_below_the_writable_window() {
    // The bootloader declines these silently and still returns success, so a
    // report that counts copied blocks is not evidence that all of them landed.
    let image = converted();
    assert_eq!(image.blocks_below(USER_FLASH_START), 11);
    assert_eq!(image.spans(), vec![(0x0, 0xB00), (0x1000, 0x2_6500)]);
}

#[test]
fn the_converted_image_round_trips_through_a_file() {
    let image = converted();
    let bytes = image.encode().unwrap();
    let read_back = Image::parse(&bytes).unwrap();
    assert_eq!(read_back, image);
    read_back.check_numbering().unwrap();
    assert_eq!(read_back.blocks[607].block_no, 607);
    assert_eq!(read_back.blocks[607].num_blocks, 608);
}

#[test]
fn the_image_states_its_own_version_at_the_address_nrf_sdm_names() {
    // The second, independent reading: not the bootloader's `SoftDevice:`
    // line but the word the SoftDevice keeps in flash. Both rig boards read
    // 7003000 there, and this is the image that puts it there.
    assert_eq!(
        softdevice::installed_version(&converted()),
        Some(Version::new(7, 3, 0))
    );
}

#[test]
fn the_vendored_hex_is_nordics_crlf_original() {
    // Nordic ships CRLF. A checkout that silently normalised it to LF would
    // still parse, so nothing else here would notice — but the file would no
    // longer be the bytes Nordic distributes, and its sha256 in a built
    // bundle would stop matching Nordic's.
    assert!(SOFTDEVICE_HEX.contains(":020000040000FA\r\n"));
    assert_eq!(SOFTDEVICE_HEX.matches("\r\n").count(), 9726);
    assert_eq!(SOFTDEVICE_HEX.lines().count(), 9726);
}

#[test]
fn the_same_image_with_lf_endings_decodes_to_the_same_blocks() {
    // The LF fixture: Nordic's records, the other ending. The vendored file
    // exercises CRLF; this keeps the LF path covered against the real image
    // rather than a toy one, and states that the ending carries no content.
    let lf = SOFTDEVICE_HEX.replace("\r\n", "\n");
    assert!(!lf.contains('\r'));

    let from_lf = ihex::parse(&lf).expect("the LF form of the vendored S140 hex parses");
    assert_eq!(from_lf, spans());
    assert_eq!(
        Image::from_spans(&from_lf, uf2::FAMILY_NRF52840_APP),
        converted()
    );
}

#[test]
fn the_vendored_licence_travels_with_the_vendored_blob() {
    // Clause 2 in one assertion: the blob is in the tree, so the licence is
    // too. `manifest` enforces the same thing for the bundle.
    let licence = include_str!("../payload/t114/s140_nrf52_7.3.0_license-agreement.txt");
    // Nordic's own file, not the circulated variant: clause 2 obliges us to
    // reproduce *the above copyright notice*, and only this one carries the
    // years. The variant reads "Copyright (c) Nordic Semiconductor ASA".
    assert!(licence.contains("Copyright (c) 2007 - 2020, Nordic Semiconductor ASA"));
    assert!(!licence.trim().is_empty());
}
