//! When a control-envelope ack may claim the record reached flash
//! (Codeberg #358).
//!
//! # The contract
//!
//! **When the client's call returns, a reset cannot lose the setting.**
//!
//! The firmware's persisted control records — telemetry target, fixed
//! position, media profile — are applied in RAM by the task that answers
//! the frame and written to flash by a separate store task. Until #358
//! the ack was written between those two events, so it meant "applied",
//! not "durable", and a client that acted on it could reboot the board
//! inside the window. It did, three times, on the rig: a scripted reset
//! 11 ms after the ack; and twice more with a 1500 ms sleep in front of
//! the reset, once against a blank page and once against a *stale*
//! record — the second of those with the store task still working a
//! queued write, which is why a sleep cannot fix this. A margin races a
//! *queue*, and a queue has no upper bound a constant can name.
//!
//! # What this crate is
//!
//! The bookkeeping that lets the answering task ask "is *my* record on
//! the page yet?" and get an answer that is about its own request rather
//! than about flash activity in general:
//!
//! * the answering task takes a [`SaveTicket`] from the record's
//!   [`PersistGate`] and sends it to the store task with the value;
//! * the store task hands the ticket back through [`PersistGate::finish`]
//!   once the page write has succeeded or been given up on;
//! * the answering task's [`PersistGate::poll`] turns into `Some` exactly
//!   then, and the ack (or the honest refusal) goes out after that.
//!
//! It lives in its own crate for the reason [`leviculum-queue-budget`] and
//! [`leviculum-rx-arming`] do: the firmware crate only builds for
//! `thumbv7em`, so ordering asserted only there is asserted nowhere. The
//! tests below drive this type against a deliberately slow flash stub and
//! carry the old ack ordering as a positive control — the same test body
//! that passes with the gate consulted loses the setting without it.
//!
//! The waiting itself is not here: bounding the wait needs a clock, and
//! the firmware's is Embassy's. This crate owns *when the answer is
//! allowed to claim durability*; `leviculum_nrf::telemetry` owns how long
//! it is willing to wait for that claim.
//!
//! [`leviculum-queue-budget`]: https://codeberg.org/Lew_Palm/leviculum
//! [`leviculum-rx-arming`]: https://codeberg.org/Lew_Palm/leviculum

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicU32, Ordering};

/// What became of one save request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persisted {
    /// The record is on the page. A reset now comes back with it — the
    /// contract in the module docs, and the only outcome that may be
    /// acked.
    Durable,
    /// The store task gave up: the value is applied in RAM and a reset
    /// would lose it. Distinguishable on the wire
    /// ([`REFUSE_PERSIST`](https://codeberg.org/Lew_Palm/leviculum)), never
    /// dressed up as an ack.
    Lost,
}

/// A receipt for one save request, handed to the store task with the
/// value and handed back when the page write is over.
///
/// Copy because it travels through a depth-1 Embassy channel beside the
/// record it names, and opaque because its number means nothing outside
/// the [`PersistGate`] that issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveTicket(u32);

/// One persisted record's answer to "has my save reached flash?".
///
/// One gate per record rather than one for the page: the store task
/// rewrites all three records on every save, but only the record a
/// request *names* carries the requester's value — the other two are read
/// back off the page and copied across the erase. A single shared gate
/// would therefore report a media-profile write as proof that a queued
/// telemetry target is durable, which is the same lie in a new place.
pub struct PersistGate {
    /// Tickets handed out so far. The next ticket is `issued + 1`, so
    /// ticket numbers start at 1 and `0` can mean "nothing yet".
    issued: AtomicU32,
    /// The highest finished ticket and its outcome, packed as
    /// `seq << 1 | durable`. One word so a reader never sees a sequence
    /// number from one save beside an outcome from another.
    done: AtomicU32,
}

impl Default for PersistGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PersistGate {
    /// A gate with nothing issued and nothing finished.
    pub const fn new() -> Self {
        Self {
            issued: AtomicU32::new(0),
            done: AtomicU32::new(0),
        }
    }

    /// Take a receipt for the save this caller is about to request.
    ///
    /// Call it in the same breath as the request — the ticket is what
    /// makes the wait about *this* value rather than about the next
    /// completion the store task happens to report.
    pub fn issue(&self) -> SaveTicket {
        let seq = self.issued.fetch_add(1, Ordering::Relaxed) + 1;
        // 31 bits of sequence, because the outcome bit shares the word.
        // Exhausting them takes 2^31 saves; the flash these tickets
        // describe is rated for 10^5 erase cycles, so the page is worn
        // out four orders of magnitude earlier. Asserted rather than
        // handled: there is no correct behaviour to fall back to, and a
        // silent wrap would resolve a future ticket instantly.
        debug_assert!(seq <= u32::MAX >> 1, "persist ticket sequence exhausted");
        SaveTicket(seq)
    }

    /// Report what became of `ticket`'s page write. Called by the store
    /// task, once per request it takes off its queue, on every exit path
    /// — including the read-compare-write skip, which is
    /// [`Persisted::Durable`]: the value is on the page, it just did not
    /// need writing.
    pub fn finish(&self, ticket: SaveTicket, outcome: Persisted) {
        let packed = (ticket.0 << 1) | u32::from(outcome == Persisted::Durable);
        // `fetch_max` rather than `store`: the store task finishes a
        // record's tickets in issue order, but a displaced request is
        // never finished at all, and this way an out-of-order report
        // could only ever fail to satisfy a ticket, never wrongly
        // satisfy one.
        self.done.fetch_max(packed, Ordering::AcqRel);
    }

    /// Whether `ticket`'s value is settled, and how.
    ///
    /// `None` is "not yet" — the caller must keep waiting, and must not
    /// answer the frame. A ticket is also settled by a *later* save of
    /// the same record: the newer value superseded ours before it ever
    /// reached the page, so what a reset would come back with is that
    /// newer value, and there is nothing of ours left for it to lose.
    pub fn poll(&self, ticket: SaveTicket) -> Option<Persisted> {
        let done = self.done.load(Ordering::Acquire);
        if done >> 1 >= ticket.0 {
            Some(if done & 1 == 1 {
                Persisted::Durable
            } else {
                Persisted::Lost
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // The gate itself
    // -----------------------------------------------------------------

    #[test]
    fn a_fresh_ticket_is_unsettled() {
        let gate = PersistGate::new();
        let ticket = gate.issue();
        assert_eq!(
            gate.poll(ticket),
            None,
            "nothing has been written, so nothing may be acked"
        );
    }

    #[test]
    fn a_finished_ticket_reports_its_outcome() {
        let gate = PersistGate::new();
        let ticket = gate.issue();
        gate.finish(ticket, Persisted::Durable);
        assert_eq!(gate.poll(ticket), Some(Persisted::Durable));
    }

    #[test]
    fn a_write_that_was_given_up_on_is_lost_not_durable() {
        let gate = PersistGate::new();
        let ticket = gate.issue();
        gate.finish(ticket, Persisted::Lost);
        assert_eq!(
            gate.poll(ticket),
            Some(Persisted::Lost),
            "a failed flash write must not read as an ack"
        );
    }

    /// #358's third manifestation in miniature: the store task was still
    /// working a write queued *before* ours when the margin expired. That
    /// completion is not ours and must not settle our ticket.
    #[test]
    fn an_earlier_save_does_not_settle_a_later_ticket() {
        let gate = PersistGate::new();
        let queued_ahead = gate.issue();
        let ours = gate.issue();
        gate.finish(queued_ahead, Persisted::Durable);
        assert_eq!(
            gate.poll(ours),
            None,
            "the store task finished someone else's write; ours is still in the queue"
        );
        gate.finish(ours, Persisted::Durable);
        assert_eq!(gate.poll(ours), Some(Persisted::Durable));
    }

    #[test]
    fn a_later_save_of_the_same_record_settles_an_earlier_ticket() {
        let gate = PersistGate::new();
        let ours = gate.issue();
        let superseding = gate.issue();
        gate.finish(superseding, Persisted::Durable);
        assert_eq!(
            gate.poll(ours),
            Some(Persisted::Durable),
            "our value was replaced before it reached the page; a reset loses nothing of ours"
        );
    }

    #[test]
    fn records_do_not_settle_each_others_tickets() {
        let target = PersistGate::new();
        let fixed_position = PersistGate::new();
        let media = PersistGate::new();
        let ticket = target.issue();
        // The page write that carries a media profile also rewrites the
        // target record — but with what was already on the page, not with
        // the target still sitting in the queue.
        media.finish(media.issue(), Persisted::Durable);
        fixed_position.finish(fixed_position.issue(), Persisted::Durable);
        assert_eq!(target.poll(ticket), None);
    }

    // -----------------------------------------------------------------
    // The store task against a slow flash, with the old ordering as the
    // positive control
    // -----------------------------------------------------------------

    /// The value one record holds. A byte because what it means does not
    /// matter here; what matters is which one survives a reset.
    type Value = u8;

    const OLD: Value = 1;
    const NEW: Value = 2;
    const OTHER: Value = 3;

    /// A store task whose page write takes `latency` steps, standing in
    /// for the SoftDevice erase-plus-write the real one awaits.
    struct SlowFlash {
        /// What a reset would read back.
        page: Value,
        /// Requests waiting for the writer, oldest first. Depth is
        /// unbounded here on purpose: the point of #358's third
        /// manifestation is that a save can sit behind another one.
        queue: Vec<(SaveTicket, Value)>,
        /// Steps the write in progress still needs.
        remaining: u32,
        /// Steps one page write costs.
        latency: u32,
        /// Whether the writer fails every attempt, as a wedged
        /// SoftDevice flash does.
        wedged: bool,
    }

    impl SlowFlash {
        fn new(page: Value, latency: u32) -> Self {
            Self {
                page,
                queue: Vec::new(),
                remaining: 0,
                latency,
                wedged: false,
            }
        }

        fn request(&mut self, ticket: SaveTicket, value: Value) {
            self.queue.push((ticket, value));
        }

        /// One step of the store task's loop.
        fn step(&mut self, gate: &PersistGate) {
            if self.queue.is_empty() {
                return;
            }
            if self.remaining == 0 {
                self.remaining = self.latency;
            }
            self.remaining -= 1;
            if self.remaining > 0 {
                return;
            }
            let (ticket, value) = self.queue.remove(0);
            if self.wedged {
                gate.finish(ticket, Persisted::Lost);
            } else {
                self.page = value;
                gate.finish(ticket, Persisted::Durable);
            }
        }
    }

    /// When the answering task writes the answer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AckPolicy {
        /// The ordering before #358: the RAM state is updated, the save is
        /// requested, the ack goes out. The positive control.
        WhenApplied,
        /// A fixed sleep in front of the answer — periculum's
        /// `PERSIST_MARGIN` stopgap, in steps.
        AfterMargin(u32),
        /// The fix: the answer waits for this record's gate.
        WhenPersisted,
    }

    /// What the client read off the wire.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Acked,
        PersistFailed,
        PersistTimedOut,
    }

    struct Board {
        /// The applied state — the atomics the real firmware keeps.
        ram: Value,
        gate: PersistGate,
        flash: SlowFlash,
    }

    impl Board {
        fn new(stored: Value, latency: u32) -> Self {
            Self {
                ram: stored,
                gate: PersistGate::new(),
                flash: SlowFlash::new(stored, latency),
            }
        }

        /// The serial task's control-frame arm: apply, request the save,
        /// answer.
        ///
        /// `budget` bounds the wait the way the firmware's
        /// `PERSIST_CONFIRM_WITHIN` does — a client must not hang forever
        /// on a wedged store task.
        fn set(&mut self, value: Value, policy: AckPolicy, budget: u32) -> Answer {
            self.ram = value;
            let ticket = self.gate.issue();
            self.flash.request(ticket, value);
            let wait = match policy {
                AckPolicy::WhenApplied => 0,
                AckPolicy::AfterMargin(steps) => steps,
                AckPolicy::WhenPersisted => budget,
            };
            for _ in 0..wait {
                if policy == AckPolicy::WhenPersisted {
                    if let Some(outcome) = self.gate.poll(ticket) {
                        return match outcome {
                            Persisted::Durable => Answer::Acked,
                            Persisted::Lost => Answer::PersistFailed,
                        };
                    }
                }
                self.flash.step(&self.gate);
            }
            match policy {
                AckPolicy::WhenPersisted => Answer::PersistTimedOut,
                // Both stopgaps answer the same way whatever the flash is
                // doing — which is exactly the defect.
                _ => Answer::Acked,
            }
        }

        /// What the client does next: reboot the board. Everything the
        /// store task had not written is gone.
        fn reset(&mut self) {
            self.flash.queue.clear();
            self.flash.remaining = 0;
            self.ram = self.flash.page;
        }
    }

    /// Positive control. This is the defect, reproduced: the ack goes out
    /// while the write is still owed, and the reset the client sends on
    /// the strength of that ack comes back with the old value.
    #[test]
    fn old_ordering_the_ack_is_followed_by_a_reset_that_loses_the_setting() {
        let mut board = Board::new(OLD, 3);
        assert_eq!(board.set(NEW, AckPolicy::WhenApplied, 100), Answer::Acked);
        board.reset();
        assert_eq!(
            board.ram, OLD,
            "the ack claimed durability the store task had not delivered"
        );
    }

    /// The same body with the gate consulted.
    #[test]
    fn ack_after_the_store_task_confirms_survives_the_same_reset() {
        let mut board = Board::new(OLD, 3);
        assert_eq!(board.set(NEW, AckPolicy::WhenPersisted, 100), Answer::Acked);
        board.reset();
        assert_eq!(board.ram, NEW);
    }

    /// Why the fix is a gate and not a bigger sleep. The margin is
    /// generous for one write and still loses the setting once a write is
    /// queued ahead of ours — the state #358's third manifestation caught
    /// the board in, with a boot-time save still in the store task's
    /// queue.
    #[test]
    fn a_fixed_margin_still_loses_the_setting_behind_a_queued_write() {
        let mut board = Board::new(OLD, 3);
        let queued_ahead = board.gate.issue();
        board.flash.request(queued_ahead, OTHER);

        assert_eq!(
            board.set(NEW, AckPolicy::AfterMargin(4), 100),
            Answer::Acked
        );
        board.reset();
        assert_eq!(
            board.ram, OTHER,
            "the margin covered one write, and there were two"
        );
    }

    #[test]
    fn the_gate_waits_out_a_write_queued_ahead_of_ours() {
        let mut board = Board::new(OLD, 3);
        let queued_ahead = board.gate.issue();
        board.flash.request(queued_ahead, OTHER);

        assert_eq!(board.set(NEW, AckPolicy::WhenPersisted, 100), Answer::Acked);
        board.reset();
        assert_eq!(board.ram, NEW);
    }

    #[test]
    fn a_flash_that_never_takes_the_write_is_answered_as_not_persisted() {
        let mut board = Board::new(OLD, 3);
        board.flash.wedged = true;
        assert_eq!(
            board.set(NEW, AckPolicy::WhenPersisted, 100),
            Answer::PersistFailed,
            "a write that errored must reach the client as an error, not as an ack"
        );
        board.reset();
        assert_eq!(board.ram, OLD, "and the refusal has to be the truth");
    }

    #[test]
    fn a_wedged_store_task_does_not_hang_the_client_forever() {
        let mut board = Board::new(OLD, 3);
        // A store task that never runs: nothing steps the flash.
        let ticket = board.gate.issue();
        board.flash.request(ticket, OTHER);
        board.flash.latency = u32::MAX;

        assert_eq!(
            board.set(NEW, AckPolicy::WhenPersisted, 10),
            Answer::PersistTimedOut,
            "the wait is bounded and says so rather than blocking the port"
        );
    }

    /// All three persisted control records, each with its own gate, each
    /// surviving the reset that used to lose it.
    #[test]
    fn all_three_control_records_survive_the_reset_after_their_ack() {
        for (record, stored, set_to) in [
            ("telemetry target", OLD, NEW),
            ("fixed position", OLD, OTHER),
            ("media profile", OTHER, NEW),
        ] {
            let mut board = Board::new(stored, 3);
            assert_eq!(
                board.set(set_to, AckPolicy::WhenPersisted, 100),
                Answer::Acked,
                "{record}"
            );
            board.reset();
            assert_eq!(board.ram, set_to, "{record}");
        }
    }
}
