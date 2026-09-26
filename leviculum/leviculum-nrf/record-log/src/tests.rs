//! Host tests over the simulated part.
//!
//! The two that matter most are named for what they prove rather than for
//! what they call: [`power_cut_at_every_byte_offset_of_a_record_write`] and
//! [`a_pinned_metadata_sector_fails_the_wear_check`]. The first is the
//! reason the store can be trusted across a battery pull; the second is the
//! positive control for the wear pin, without which "wear is level" is a
//! sentence rather than a measurement.

use alloc::vec;
use alloc::vec::Vec;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

use crate::sim::{SimNor, ERASED};
use crate::*;

/// A 4-byte-aligned literal for the tests that drive [`SimNor`] directly.
/// `[u8; N]` on the stack is only byte-aligned, and the part refuses an
/// unaligned buffer exactly as the QSPI peripheral does.
fn aligned<const N: usize>(bytes: [u8; N]) -> Aligned<N> {
    Aligned(bytes)
}

const SECTORS: u32 = 16;
const REGION: u32 = SECTORS * SECTOR_SIZE;
/// The median stored object the 2026-09-09 field walk measured: 272 B of
/// `lxmf_data` plus a 32-byte propagation stamp.
const FIELD_BODY: usize = 304;

fn key(n: u32) -> [u8; KEY_LEN] {
    let mut k = [0u8; KEY_LEN];
    k[0..4].copy_from_slice(&n.to_le_bytes());
    k[28..32].copy_from_slice(&n.to_be_bytes());
    k
}

fn body(n: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (n as u8).wrapping_add(i as u8)).collect()
}

fn fresh(sectors: u32) -> RecordLog<SimNor> {
    RecordLog::open(SimNor::new(sectors), 0, sectors * SECTOR_SIZE).unwrap()
}

fn collect(log: &mut RecordLog<SimNor>) -> Vec<Record> {
    let mut out = Vec::new();
    log.for_each(|r| out.push(*r)).unwrap();
    out
}

/// The wear pin itself: the spread between the most- and least-erased
/// sector. Round-robin reclaim keeps it at 0 or 1 forever; anything that
/// writes to a fixed place does not.
fn wear_spread(counts: &[u32]) -> u32 {
    let max = counts.iter().copied().max().unwrap_or(0);
    let min = counts.iter().copied().min().unwrap_or(0);
    max - min
}

// ---------------------------------------------------------------- the CRC

#[test]
fn crc16_matches_the_standard_check_value() {
    // CRC-16/CCITT-FALSE's published check value, and the same vector
    // `leviculum_core::framing::hdlc`'s own test uses. If this ever
    // disagrees, the two implementations have drifted.
    assert_eq!(crc16_update(CRC_INIT, b"123456789"), 0x29B1);
}

#[test]
fn crc16_is_incremental() {
    let split = crc16_update(crc16_update(CRC_INIT, b"12345"), b"6789");
    assert_eq!(split, crc16_update(CRC_INIT, b"123456789"));
}

// ------------------------------------------- the simulated part's semantics

#[test]
fn a_fresh_part_reads_as_erased() {
    let mut sim = SimNor::new(2);
    assert!(sim.bytes().iter().all(|b| *b == ERASED));
    let mut buf = aligned([0u8; 8]);
    sim.read(0, &mut buf.0).unwrap();
    assert_eq!(buf.0, [ERASED; 8]);
}

#[test]
fn an_erase_covers_exactly_its_sector() {
    let mut sim = SimNor::new(2);
    sim.write(0, &aligned([0u8; 8]).0).unwrap();
    sim.write(SECTOR_SIZE, &aligned([0u8; 8]).0).unwrap();
    sim.erase(0, SECTOR_SIZE).unwrap();
    assert!(sim.bytes()[..SECTOR_SIZE as usize]
        .iter()
        .all(|b| *b == ERASED));
    assert_eq!(sim.bytes()[SECTOR_SIZE as usize], 0);
    assert_eq!(sim.erase_counts(), &[1, 0]);
}

#[test]
fn programming_clears_bits_and_never_restores_them() {
    let mut sim = SimNor::new(1);
    sim.write(0, &aligned([0b1111_0000, 0xFF, 0xFF, 0xFF]).0)
        .unwrap();
    assert_eq!(sim.bytes()[0], 0b1111_0000);
    // A second program may clear further bits in the same byte without an
    // erase — which is exactly what a record commit does to its flags byte.
    sim.write(0, &aligned([0b1010_0000, 0xFF, 0xFF, 0xFF]).0)
        .unwrap();
    assert_eq!(sim.bytes()[0], 0b1010_0000);
    // And programming 0xFF over anything leaves it alone, which is what
    // makes the record padding free.
    assert_eq!(sim.bytes()[1..4], [0xFF; 3]);
}

#[test]
#[should_panic(expected = "would raise a bit")]
fn programming_a_one_over_a_zero_is_refused() {
    // The negative control for the program-once model: without it, the
    // simulation is RAM and every commit-ordering bug passes.
    let mut sim = SimNor::new(1);
    sim.write(0, &aligned([0x00, 0xFF, 0xFF, 0xFF]).0).unwrap();
    sim.write(0, &aligned([0xFF, 0xFF, 0xFF, 0xFF]).0).unwrap();
}

#[test]
#[should_panic(expected = "4-byte aligned")]
fn an_unaligned_program_address_is_refused() {
    let mut sim = SimNor::new(1);
    sim.write(2, &aligned([0u8; 4]).0).unwrap();
}

#[test]
#[should_panic(expected = "multiple of 4")]
fn a_program_length_off_the_unit_is_refused() {
    let mut sim = SimNor::new(1);
    sim.write(0, &aligned([0u8; 4]).0[..3]).unwrap();
}

#[test]
#[should_panic(expected = "sector-aligned")]
fn an_erase_off_the_sector_grid_is_refused() {
    let mut sim = SimNor::new(2);
    sim.erase(2048, 2048 + SECTOR_SIZE).unwrap();
}

// -------------------------------------------------------- mounting, layout

#[test]
fn the_record_header_is_the_forty_two_bytes_the_paper_tabulates() {
    assert_eq!(HEADER_LEN, 42);
    assert_eq!(SECTOR_SIZE, 4096);
    // 11 field-sized records per sector, as the paper's capacity table says.
    assert_eq!(record_stride(FIELD_BODY), 348);
    assert_eq!(SECTOR_PAYLOAD as usize / record_stride(FIELD_BODY), 11);
}

#[test]
fn a_fresh_region_formats_itself_and_holds_nothing() {
    let mut log = fresh(SECTORS);
    assert_eq!(collect(&mut log).len(), 0);
    assert_eq!(log.active_sector(), 0);
    assert_eq!(log.sequence(), 0);
    let sim = log.into_flash();
    // Exactly one sector was touched to format the region.
    assert_eq!(sim.erase_counts()[0], 1);
    assert!(sim.erase_counts()[1..].iter().all(|c| *c == 0));
}

#[test]
fn mounting_a_formatted_region_writes_nothing() {
    let mut log = fresh(SECTORS);
    for i in 0..5u32 {
        log.append(&key(i), i, 1, &body(i, FIELD_BODY)).unwrap();
    }
    let sim = log.into_flash();
    let spent = sim.spent();
    let erases: Vec<u32> = sim.erase_counts().to_vec();

    let mut log = RecordLog::open(sim, 0, REGION).unwrap();
    assert_eq!(collect(&mut log).len(), 5);
    let sim = log.into_flash();
    // Mounting is a read. If it were not, the mount itself would be a
    // fixed-location write on every boot, which is the failure mode the
    // paper's endurance table is about.
    assert_eq!(sim.spent(), spent);
    assert_eq!(sim.erase_counts(), erases.as_slice());
}

#[test]
fn a_region_shorter_than_two_sectors_is_refused() {
    assert!(matches!(
        RecordLog::open(SimNor::new(4), 0, SECTOR_SIZE),
        Err(Error::BadRegion)
    ));
    assert!(matches!(
        RecordLog::open(SimNor::new(4), 512, 2 * SECTOR_SIZE),
        Err(Error::BadRegion)
    ));
    assert!(matches!(
        RecordLog::open(SimNor::new(4), 2 * SECTOR_SIZE, 4 * SECTOR_SIZE),
        Err(Error::OutOfBounds)
    ));
}

#[test]
fn a_body_past_a_sector_is_refused() {
    let mut log = fresh(SECTORS);
    assert!(matches!(
        log.append(&key(0), 0, 0, &body(0, MAX_BODY + 1)),
        Err(Error::BodyTooLarge)
    ));
    // The largest body that does fit is accepted and reads back.
    let big = body(1, MAX_BODY);
    log.append(&key(1), 1, 0, &big).unwrap();
    let recs = collect(&mut log);
    assert_eq!(recs.len(), 1);
    let mut out = vec![0u8; MAX_BODY];
    log.read_body(&recs[0], &mut out).unwrap();
    assert_eq!(out, big);
}

// --------------------------------------------------------- append and scan

#[test]
fn appended_records_read_back_in_order() {
    let lengths = [0usize, 1, 2, 3, 4, 63, FIELD_BODY, 1000];
    let mut log = fresh(SECTORS);
    for (i, len) in lengths.iter().enumerate() {
        let i = i as u32;
        log.append(&key(i), 1_000_000 + i, (i as u8) | 0x80, &body(i, *len))
            .unwrap();
    }
    let sim = log.into_flash();
    let mut log = RecordLog::open(sim, 0, REGION).unwrap();

    let recs = collect(&mut log);
    assert_eq!(recs.len(), lengths.len());
    for (i, (rec, len)) in recs.iter().zip(lengths.iter()).enumerate() {
        let i = i as u32;
        assert_eq!(rec.key, key(i));
        assert_eq!(rec.time, 1_000_000 + i);
        assert_eq!(rec.tag, (i as u8) | 0x80);
        assert_eq!(rec.len as usize, *len);
        assert!(rec.is_live());
        assert_eq!(rec.offset % PROGRAM_UNIT, 0, "records stay 4-byte aligned");
        let mut out = vec![0u8; *len];
        assert_eq!(log.read_body(rec, &mut out).unwrap(), *len);
        assert_eq!(out, body(i, *len));
    }
}

#[test]
fn a_record_never_straddles_a_sector() {
    let mut log = fresh(4);
    // 11 of these fit in a sector with 268 bytes to spare, which is less
    // than one more record: the twelfth has to start a new sector.
    for i in 0..12u32 {
        log.append(&key(i), i, 0, &body(i, FIELD_BODY)).unwrap();
    }
    assert_eq!(log.active_sector(), 1);
    let recs = collect(&mut log);
    assert_eq!(recs.len(), 12);
    for rec in &recs {
        let start = rec.offset % SECTOR_SIZE;
        assert!(
            start + rec.stride() <= SECTOR_SIZE,
            "record at {:#x} runs past its sector",
            rec.offset
        );
        assert!(start >= SECTOR_HEADER_LEN);
    }
    assert_eq!(recs[11].offset / SECTOR_SIZE, 1);
}

#[test]
fn reclaim_is_round_robin_and_takes_the_oldest_sector() {
    let sectors = 4u32;
    let per_sector = (SECTOR_PAYLOAD as usize / record_stride(FIELD_BODY)) as u32;
    let mut log = fresh(sectors);
    // One full lap plus one sector, so sector 0 is reclaimed and its
    // records are gone.
    let total = per_sector * (sectors + 1);
    for i in 0..total {
        log.append(&key(i), i, 0, &body(i, FIELD_BODY)).unwrap();
    }
    // 55 records of 348 bytes, 11 to a sector: four reclaims, ending back
    // on sector 0 with the records it started with gone.
    let reclaims = total.div_ceil(per_sector) - 1;
    assert_eq!(reclaims, 4);
    assert_eq!(log.active_sector(), reclaims % sectors);
    assert_eq!(log.active_sector(), 0);

    let sim = log.into_flash();
    // Sector 0 was erased twice — once to format, once on the lap — and
    // every other sector once. That is the spread the wear pin allows.
    assert_eq!(sim.erase_counts(), &[2, 1, 1, 1]);
    let mut log = RecordLog::open(sim, 0, sectors * SECTOR_SIZE).unwrap();

    let recs = collect(&mut log);
    // Whatever survives is the newest run, contiguous and in order.
    assert_eq!(recs.len() as u32, per_sector * sectors);
    let first = u32::from_le_bytes(recs[0].key[0..4].try_into().unwrap());
    assert_eq!(first, total - recs.len() as u32);
    for (n, rec) in recs.iter().enumerate() {
        assert_eq!(rec.key, key(first + n as u32));
        assert_eq!(rec.time, first + n as u32);
    }
    assert_eq!(log.sequence(), reclaims);
}

#[test]
fn purge_withdraws_a_record_without_moving_anything() {
    let mut log = fresh(SECTORS);
    for i in 0..3u32 {
        log.append(&key(i), i, 0, &body(i, 64)).unwrap();
    }
    let recs = collect(&mut log);
    let offsets: Vec<u32> = recs.iter().map(|r| r.offset).collect();
    log.purge(&recs[1]).unwrap();

    let sim = log.into_flash();
    let mut log = RecordLog::open(sim, 0, REGION).unwrap();
    let after = collect(&mut log);
    assert_eq!(after.len(), 3);
    assert_eq!(
        after.iter().map(|r| r.offset).collect::<Vec<_>>(),
        offsets,
        "a purge is a bit, not a move"
    );
    assert!(after[0].is_live());
    assert!(!after[1].is_live());
    assert_eq!(after[1].flags, FLAG_PURGED);
    assert!(after[2].is_live());
    // The body of a purged record is still there and still correct: the
    // reclaim is what removes it, nothing else.
    let mut out = vec![0u8; 64];
    log.read_body(&after[1], &mut out).unwrap();
    assert_eq!(out, body(1, 64));
    // And purging twice is not an error.
    log.purge(&after[1]).unwrap();
    assert_eq!(collect(&mut log).len(), 3);
}

#[test]
fn a_lost_bit_in_a_body_hides_that_record_and_no_other() {
    let mut log = fresh(SECTORS);
    for i in 0..4u32 {
        log.append(&key(i), i, 0, &body(i, 64)).unwrap();
    }
    let recs = collect(&mut log);
    let victim = recs[1];
    let mut sim = log.into_flash();
    // body(1, 64)[7] is 0x08; clearing its one set bit is a bit the part
    // could plausibly lose, and `corrupt` refuses a no-op.
    sim.corrupt(victim.body_offset() + 7, 0xF7);

    let mut log = RecordLog::open(sim, 0, REGION).unwrap();
    let after = collect(&mut log);
    assert_eq!(after.len(), 3, "only the damaged record disappears");
    let keys: Vec<[u8; KEY_LEN]> = after.iter().map(|r| r.key).collect();
    assert_eq!(keys, vec![key(0), key(2), key(3)]);
}

#[test]
fn a_committed_flags_byte_is_the_only_thing_that_makes_a_record() {
    // Everything a record needs is on the part except the commit, which is
    // exactly the state a power cut leaves. It must not be readable.
    let mut log = fresh(SECTORS);
    log.append(&key(0), 0, 0, &body(0, 32)).unwrap();
    let recs = collect(&mut log);
    let mut sim = log.into_flash();
    // 0xFE -> 0xFA is a value no commit produces.
    sim.corrupt(recs[0].offset + 39, 0xFA);
    let mut log = RecordLog::open(sim, 0, REGION).unwrap();
    assert_eq!(collect(&mut log).len(), 0);
}

// ------------------------------------------------------------- power cuts

/// Drive one power cut, `cut` program/erase bytes into the record write
/// that follows `pre` complete records, and assert what came back.
fn probe_cut(sectors: u32, body_len: usize, pre: u32, cut: usize, cost: usize) {
    let region = sectors * SECTOR_SIZE;
    let mut log = RecordLog::open(SimNor::new(sectors), 0, region).unwrap();
    for i in 0..pre {
        log.append(&key(i), i, 1, &body(i, body_len)).unwrap();
    }
    log.flash_mut().arm_power_cut(cut);
    let landed = log.append(&key(pre), pre, 1, &body(pre, body_len)).is_ok();
    assert_eq!(
        landed,
        cut >= cost,
        "cut={cut} of {cost}: the append's own verdict must match the budget"
    );

    let mut sim = log.into_flash();
    sim.power_on();
    let mut log = RecordLog::open(sim, 0, region).unwrap();
    let recs = collect(&mut log);

    let expected = if landed { pre + 1 } else { pre };
    assert_eq!(
        recs.len() as u32,
        expected,
        "cut={cut} of {cost}: every completed record and no partial one"
    );
    for (n, rec) in recs.iter().enumerate() {
        let n = n as u32;
        assert_eq!(rec.key, key(n), "cut={cut}");
        assert_eq!(rec.time, n, "cut={cut}");
        assert!(rec.is_live(), "cut={cut}");
        let mut out = vec![0u8; rec.len as usize];
        log.read_body(rec, &mut out).unwrap();
        assert_eq!(
            out,
            body(n, body_len),
            "cut={cut}: body {n} survived intact"
        );
    }

    // A store that reopens read-only after a cut is half a store. The next
    // record has to land, which is where "seal the dirty sector" earns its
    // keep: appending into the half-written tail would need to raise bits
    // and `SimNor` would panic.
    log.append(&key(9999), 9999, 2, &body(9999, body_len))
        .unwrap();
    assert_eq!(collect(&mut log).len() as u32, expected + 1, "cut={cut}");
}

/// What one append costs the part in program/erase bytes, measured rather
/// than derived, so the sweep below cannot silently stop short of the end.
fn append_cost(sectors: u32, body_len: usize, pre: u32) -> usize {
    let region = sectors * SECTOR_SIZE;
    let mut log = RecordLog::open(SimNor::new(sectors), 0, region).unwrap();
    for i in 0..pre {
        log.append(&key(i), i, 1, &body(i, body_len)).unwrap();
    }
    let before = log.flash_mut().spent();
    log.append(&key(pre), pre, 1, &body(pre, body_len)).unwrap();
    log.flash_mut().spent() - before
}

#[test]
fn power_cut_at_every_byte_offset_of_a_record_write() {
    // Body lengths chosen so that 42 + len lands on each residue mod 4 —
    // the padding path differs in each — plus the empty body and the median
    // the field walk measured.
    let lengths = [0usize, 1, 2, 3, 5, 61, FIELD_BODY];
    let mut offsets = 0usize;
    for len in lengths {
        let cost = append_cost(SECTORS, len, 3);
        assert_eq!(
            cost,
            record_stride(len) + PROGRAM_UNIT as usize,
            "a record write is its stride plus the 4-byte commit"
        );
        for cut in 0..=cost {
            probe_cut(SECTORS, len, 3, cut, cost);
            offsets += 1;
        }
    }
    // 715 offsets over seven body lengths. Kept as an assertion so a change
    // that quietly shrinks the sweep is a failure, not a faster test.
    assert_eq!(offsets, 715);
}

#[test]
fn power_cut_at_every_byte_offset_of_a_reclaim() {
    // Three 1000-byte records fill a sector, so the fourth append has to
    // erase the next one: this sweep covers the erase, the new sector
    // header, the record and the commit.
    let sectors = 4u32;
    let body_len = 1000usize;
    let pre = 3u32;
    let cost = append_cost(sectors, body_len, pre);
    assert_eq!(
        cost,
        SECTOR_SIZE as usize
            + SECTOR_HEADER_LEN as usize
            + record_stride(body_len)
            + PROGRAM_UNIT as usize
    );
    let mut offsets = 0usize;
    for cut in 0..=cost {
        probe_cut(sectors, body_len, pre, cut, cost);
        offsets += 1;
    }
    assert_eq!(offsets, 5157);
}

#[test]
fn a_cut_costs_at_most_the_rest_of_one_sector() {
    // The sealed-sector rule is a cost as well as a guarantee. Pin it: a
    // cut early in a nearly empty sector loses that sector's remaining
    // room and nothing else.
    let mut log = fresh(SECTORS);
    log.append(&key(0), 0, 1, &body(0, 64)).unwrap();
    log.flash_mut().arm_power_cut(8);
    assert!(log.append(&key(1), 1, 1, &body(1, 64)).is_err());
    let mut sim = log.into_flash();
    sim.power_on();
    let mut log = RecordLog::open(sim, 0, REGION).unwrap();
    log.append(&key(2), 2, 1, &body(2, 64)).unwrap();
    let recs = collect(&mut log);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].offset / SECTOR_SIZE, 0);
    assert_eq!(
        recs[1].offset / SECTOR_SIZE,
        1,
        "the dirty sector is sealed"
    );
    let sim = log.into_flash();
    // Sealing costs one extra erase and nothing more.
    assert_eq!(sim.erase_counts()[0], 1);
    assert_eq!(sim.erase_counts()[1], 1);
    assert!(sim.erase_counts()[2..].iter().all(|c| *c == 0));
}

// ------------------------------------------------------------ the wear pin

#[test]
fn wear_is_level_after_wrapping_the_part_twice() {
    let per_sector = (SECTOR_PAYLOAD as usize / record_stride(FIELD_BODY)) as u32;
    let laps = 3u32;
    let mut log = fresh(SECTORS);
    for i in 0..laps * SECTORS * per_sector {
        log.append(&key(i), i, 0, &body(i, FIELD_BODY)).unwrap();
    }
    let sim = log.into_flash();
    let counts = sim.erase_counts();
    assert!(
        counts.iter().copied().min().unwrap() >= 2,
        "the part must have been wrapped at least twice: {counts:?}"
    );
    assert_eq!(
        wear_spread(counts),
        0,
        "round-robin reclaim erases every sector the same number of times: {counts:?}"
    );
}

#[test]
fn a_pinned_metadata_sector_fails_the_wear_check() {
    // The negative control, and the paper's argument made executable. The
    // part is one sector larger than the log's region; sector 0 stands in
    // for the fixed index sector a reference-shaped store would rewrite on
    // every accepted record. The same workload, the same wear check, and
    // it fails — which is what makes the passing case above a measurement
    // rather than a tautology.
    let per_sector = (SECTOR_PAYLOAD as usize / record_stride(FIELD_BODY)) as u32;
    let laps = 3u32;
    let records = laps * SECTORS * per_sector;

    let sim = SimNor::new(SECTORS + 1);
    let mut log = RecordLog::open(sim, SECTOR_SIZE, REGION).unwrap();
    for i in 0..records {
        log.append(&key(i), i, 0, &body(i, FIELD_BODY)).unwrap();
        log.flash_mut().erase(0, SECTOR_SIZE).unwrap();
    }
    let sim = log.into_flash();
    let counts = sim.erase_counts();

    assert_eq!(counts[0], records, "the pinned sector takes one erase each");
    assert_eq!(counts[0], 528);
    assert_eq!(
        counts[1], laps,
        "and the log's own sectors take one per lap"
    );
    assert!(
        wear_spread(counts) > 1,
        "the wear check has to fail here: {counts:?}"
    );
    // And the log's own sectors are untouched by the neighbour's fate.
    assert_eq!(wear_spread(&counts[1..]), 0, "{counts:?}");
    // The ratio is the whole argument: at the measured field duty the
    // pinned sector burns its 100 000 cycles while the log burns three.
    assert!(counts[0] / counts[1] > 100, "{counts:?}");
}

#[test]
fn mount_refuses_to_format_and_open_does_it() {
    // The firmware's boot probe reads a part that may already hold
    // somebody else's filesystem, so asking the question must not be an
    // act. `is_formatted` borrows the device, which is what lets this
    // assert both halves: the answer, and that asking cost nothing.
    let mut sim = SimNor::new(SECTORS);
    assert!(!is_formatted(&mut sim, 0, REGION).unwrap());
    assert_eq!(
        sim.spent(),
        0,
        "probing an unformatted region writes nothing"
    );
    assert!(sim.bytes().iter().all(|b| *b == ERASED));
    assert!(sim.erase_counts().iter().all(|c| *c == 0));

    // `mount` on the same region agrees and hands back nothing to mount.
    assert!(RecordLog::mount(sim, 0, REGION).unwrap().is_none());

    // `open` is the one that formats: one erase, one sector header.
    let mut log = RecordLog::open(SimNor::new(SECTORS), 0, REGION).unwrap();
    log.append(&key(0), 0, 3, &body(0, 40)).unwrap();
    let mut sim = log.into_flash();
    assert_eq!(sim.erase_counts()[0], 1);
    assert!(sim.erase_counts()[1..].iter().all(|c| *c == 0));

    // And now `mount` finds it, without writing anything either.
    assert!(is_formatted(&mut sim, 0, REGION).unwrap());
    let spent = sim.spent();
    let mut mounted = RecordLog::mount(sim, 0, REGION).unwrap().unwrap();
    assert_eq!(mounted.count().unwrap(), (1, 0));
    let recs = collect(&mut mounted);
    assert_eq!(recs[0].tag, 3);
    assert_eq!(mounted.flash_mut().spent(), spent, "a mount is a read");
}

#[test]
fn count_separates_live_from_purged() {
    let mut log = fresh(SECTORS);
    for i in 0..5u32 {
        log.append(&key(i), i, 0, &body(i, 24)).unwrap();
    }
    let recs = collect(&mut log);
    log.purge(&recs[1]).unwrap();
    log.purge(&recs[3]).unwrap();
    assert_eq!(log.count().unwrap(), (3, 2));
}

#[test]
fn program_operations_per_page_are_counted_and_bounded() {
    // The commit word is re-programmed with three of its four bytes already
    // at their final values. That changes no bit, but it is still a program
    // operation, and a NOR page accepts only so many of those between
    // erases — a datasheet number for the part, which we do not have for
    // either MX25R1635F or IS25LP080D. So this does not argue the exposure,
    // it measures it, on the workload that maximises it: bodyless records,
    // the smallest stride the format allows, so as many commit words as
    // possible land in one 256-byte page, each of them programmed three
    // times (record write, commit, purge).
    let mut log = fresh(SECTORS);
    let mut written = 0u32;
    while log.sector_room() >= MIN_STRIDE {
        log.append(&key(written), written, 0, &[]).unwrap();
        written += 1;
    }
    assert_eq!(log.active_sector(), 0, "the sector must not have rolled");
    for record in collect(&mut log) {
        log.purge(&record).unwrap();
    }

    let sim = log.into_flash();
    let counts = sim.page_programs();
    // 19, and the arithmetic is worth writing down because it is what moves
    // if the format does. A 44-byte stride puts six records in a 256-byte
    // page: six record writes, six commits, six purges, plus the tail of
    // the record that started in the page before. Page 0 of the sector is
    // lower (17) despite its extra 12-byte header program, because the
    // header pushes one record's commit word out of it.
    assert_eq!(
        sim.max_page_programs(),
        19,
        "program operations per page changed: {:?}. Re-derive the number \
         rather than relaxing it — it is what a datasheet's partial-program \
         limit gets compared against.",
        &counts[..SECTOR_SIZE as usize / crate::sim::PAGE_SIZE as usize]
    );
    assert_eq!(counts[0], 17, "{counts:?}");
    // Every page of an untouched sector is still at zero, so the counter is
    // measuring this sector and not accumulating over the part.
    assert!(
        counts[(SECTOR_SIZE / crate::sim::PAGE_SIZE) as usize..]
            .iter()
            .all(|c| *c == 0),
        "{counts:?}"
    );
}
