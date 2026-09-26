//! The two bounds, each shown to bind on its own.
//!
//! Every test drives the numbers the firmware actually ships
//! ([`LORA_QUEUE_SLOTS`] / [`LORA_QUEUE_BYTES`]), so a change to either
//! constant is a change to what these assert.

use super::*;

/// MTU-sized packet, the shape that makes the byte bound bind first.
const MTU: usize = 500;

#[test]
fn byte_bound_binds_far_below_the_slot_count() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    // 12 × 500 = 6000 B fits, 13 × 500 = 6500 B does not.
    let fits = LORA_QUEUE_BYTES / MTU;
    for _ in 0..fits {
        assert_eq!(q.reserve(MTU), Ok(()));
    }
    assert_eq!(q.reserve(MTU), Err(QueueBound::Bytes));
    // The point of the assertion: the queue is nowhere near full by slot
    // count, so nothing but the byte bound can have refused this.
    assert_eq!(q.queued_slots(), fits);
    assert!(
        q.queued_slots() * 4 < LORA_QUEUE_SLOTS,
        "slot count {} is not far below {}",
        q.queued_slots(),
        LORA_QUEUE_SLOTS
    );
    assert_eq!(q.queued_bytes(), fits * MTU);
}

#[test]
fn a_refused_packet_leaves_the_queue_exactly_as_it_found_it() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    for _ in 0..(LORA_QUEUE_BYTES / MTU) {
        assert_eq!(q.reserve(MTU), Ok(()));
    }
    let (slots, bytes) = (q.queued_slots(), q.queued_bytes());
    // Ten refusals in a row must not consume ten slots: the byte-bound path
    // hands the speculatively taken slot back.
    for _ in 0..10 {
        assert_eq!(q.reserve(MTU), Err(QueueBound::Bytes));
    }
    assert_eq!((q.queued_slots(), q.queued_bytes()), (slots, bytes));
}

#[test]
fn slot_bound_binds_on_tiny_packets() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    // A 20-byte packet: 64 of them are 1280 B, a fifth of the byte budget.
    for _ in 0..LORA_QUEUE_SLOTS {
        assert_eq!(q.reserve(20), Ok(()));
    }
    assert_eq!(q.reserve(20), Err(QueueBound::Slots));
    assert!(
        q.queued_bytes() * 4 < LORA_QUEUE_BYTES,
        "byte total {} is not far below {}",
        q.queued_bytes(),
        LORA_QUEUE_BYTES
    );
}

#[test]
fn dequeuing_frees_both_budgets() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    let fits = LORA_QUEUE_BYTES / MTU;
    for _ in 0..fits {
        assert_eq!(q.reserve(MTU), Ok(()));
    }
    assert_eq!(q.reserve(MTU), Err(QueueBound::Bytes));
    q.release(MTU);
    assert_eq!(q.queued_slots(), fits - 1);
    assert_eq!(q.queued_bytes(), (fits - 1) * MTU);
    assert_eq!(q.reserve(MTU), Ok(()));

    // And the same for the slot bound, with tiny packets.
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    for _ in 0..LORA_QUEUE_SLOTS {
        assert_eq!(q.reserve(1), Ok(()));
    }
    assert_eq!(q.reserve(1), Err(QueueBound::Slots));
    q.release(1);
    assert_eq!(q.reserve(1), Ok(()));
}

#[test]
fn draining_the_whole_queue_returns_it_to_empty() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    let lens = [1usize, 255, 500, 12, 300];
    for _ in 0..4 {
        for len in lens {
            assert_eq!(q.reserve(len), Ok(()));
        }
    }
    for _ in 0..4 {
        for len in lens {
            q.release(len);
        }
    }
    assert_eq!(q.queued_slots(), 0);
    assert_eq!(q.queued_bytes(), 0);
    // An empty queue admits a full MTU packet again.
    assert_eq!(q.reserve(MTU), Ok(()));
}

/// Control: ordinary traffic, well under both bounds, is admitted exactly as
/// it was before the bounds existed.
///
/// Without this a `reserve` that refuses everything would pass every test
/// above — they all assert refusals — while silencing the radio completely.
#[test]
fn control_ordinary_traffic_is_admitted_unchanged() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    // A realistic burst: an announce, a path response, a link proof, a data
    // packet. Four packets, ~600 B — the shape that overran the old 4-slot
    // queue and must sail through the new one.
    for (i, len) in [184usize, 60, 32, 300].into_iter().enumerate() {
        assert_eq!(q.reserve(len), Ok(()), "packet {i} refused");
    }
    assert_eq!(q.queued_slots(), 4);
    assert_eq!(q.queued_bytes(), 576);
    assert!(q.queued_bytes() < q.max_bytes());
    assert!(q.queued_slots() < q.max_slots());
}

/// Control: a queue that never refuses is not what was built. The old
/// behaviour — four slots, no byte bound — must be reachable, and refuse.
#[test]
fn control_the_old_four_slot_queue_still_behaves_like_four_slots() {
    let q = QueueBudget::new(4, usize::MAX);
    for _ in 0..4 {
        assert_eq!(q.reserve(MTU), Ok(()));
    }
    assert_eq!(q.reserve(MTU), Err(QueueBound::Slots));
}

#[test]
fn a_packet_larger_than_the_whole_budget_is_refused() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    assert_eq!(q.reserve(LORA_QUEUE_BYTES + 1), Err(QueueBound::Bytes));
    assert_eq!(q.queued_slots(), 0);
    assert_eq!(q.queued_bytes(), 0);
    // Exactly the budget still fits: the bound is inclusive.
    assert_eq!(q.reserve(LORA_QUEUE_BYTES), Ok(()));
}

/// A release the queue never saw must not wrap the counters shut.
#[test]
fn release_without_reserve_saturates_at_empty() {
    let q = QueueBudget::new(LORA_QUEUE_SLOTS, LORA_QUEUE_BYTES);
    q.release(MTU);
    assert_eq!(q.queued_slots(), 0);
    assert_eq!(q.queued_bytes(), 0);
    assert_eq!(q.reserve(MTU), Ok(()));
}

#[test]
fn bound_names_are_stable() {
    assert_eq!(QueueBound::Slots.as_str(), "slots");
    assert_eq!(QueueBound::Bytes.as_str(), "bytes");
}

/// The shipped numbers, asserted so a change to either is a deliberate one.
#[test]
fn shipped_bounds_are_the_reference_shape() {
    assert_eq!(LORA_QUEUE_BYTES, 6144, "CONFIG_QUEUE_SIZE for T114/RAK4631");
    assert_eq!(LORA_QUEUE_SLOTS, 64);
    // The byte bound must be the one that binds under large packets, which is
    // only true while the slot count exceeds budget/MTU. Both operands are
    // compile-time constants, so this is a const block: it fails the build
    // rather than one test run (same move as c746bf8).
    const { assert!(LORA_QUEUE_SLOTS > LORA_QUEUE_BYTES / 500) };
}
