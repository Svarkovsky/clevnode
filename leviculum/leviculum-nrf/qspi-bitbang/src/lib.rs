#![no_std]
//! The JEDEC id read (opcode 0x9F), clocked out one pin edge at a time.
//!
//! This is the second opinion on `state=no-answer` (Codeberg #384). The
//! QSPI peripheral asked the part who it was and got `00:00:00` back, and
//! that single answer cannot tell two very different boards apart: a part
//! that is absent or unpowered, and a part that is present but which the
//! QSPI peripheral's own setup never reaches. The first is not a software
//! problem at all; the second is our bug, and would be waiting on every
//! other board with the same driver.
//!
//! So the failure path drops the peripheral, takes the same pins back as
//! ordinary GPIOs, and asks again by hand at a few hundred kHz. Two
//! drivers, one bus. If the hand-clocked read names the part, the part is
//! there and the peripheral setup is at fault. If it is silent too,
//! nothing on that unit drives MISO under either driver.
//!
//! # Why the shifter is here and not in the firmware
//!
//! What could go wrong in a bit-bang read is arithmetic: which edge
//! launches a bit, which edge samples it, which end of the byte goes
//! first. An off-by-one in the sampling edge does not fail loudly — it
//! returns a *plausible* id, shifted by a bit, and sends the next person
//! chasing a part number that was never on the bus. That is exactly the
//! kind of defect a host test catches and a board never will, and the
//! firmware crate cross-compiles and runs no host tests. So the edges live
//! here, over a [`Bus`] trait, with a fake part replaying a known response
//! underneath them; the firmware supplies six GPIOs and a delay.
//!
//! # The transfer
//!
//! SPI mode 0 on the part's single-line path, which is the mode both
//! candidate parts (MX25R1635F, IS25LP080D) power up in and the only one
//! opcode 0x9F is specified for:
//!
//! - CS# high and SCK low to begin, so the first clock edge of the
//!   transfer is a rising one.
//! - The master presents a command bit on IO0 (the part's SI) while SCK is
//!   low; the part samples it on the rising edge.
//! - The part presents a data bit on IO1 (its SO) on a falling edge; the
//!   master samples it on the next rising edge.
//!
//! Eight command bits of `0x9F`, MSB first, then 24 clocks of reading. The
//! part launches the first id bit on the eighth falling edge — the one
//! that ends the command byte — so the first *sample* is the ninth rising
//! edge, and [`read_jedec_id`] gets that ordering by construction:
//! [`write_bit`] ends on a falling edge, [`read_bit`] starts on a rising
//! one.
//!
//! IO2 and IO3 are the part's WP# and HOLD#; they must be held high for
//! the whole transfer or the part ignores or suspends it. Holding them is
//! the caller's job, not this crate's — they carry no edges, so they are
//! setup rather than arithmetic, and there is nothing here for a test to
//! catch about them. (The QSPI peripheral does the same thing implicitly,
//! via `CINSTRCONF.LIO2`/`LIO3`, `embassy-nrf-0.9.0/src/qspi.rs:290`.)
//!
//! # Read-only, by construction
//!
//! The only opcode this crate knows is `0x9F`. There is no write path, no
//! erase path, and no way to reach one: [`read_jedec_id`] shifts out a
//! constant. A part that answers is left exactly as it was found.

/// Read JEDEC ID (opcode 0x9F): manufacturer, memory type, capacity. The
/// only opcode this crate emits.
pub const CMD_READ_JEDEC_ID: u8 = 0x9F;

/// Bits in the answer: three bytes, MSB first.
const RESPONSE_BITS: usize = 24;

/// The six-wire bus, as much of it as a hand-clocked read touches.
///
/// Four pins carry the transfer — SCK, CS#, IO0 (the part's SI) and IO1
/// (its SO) — and the implementor is responsible for having IO2 and IO3
/// driven high before it hands the bus over, since those are WP# and
/// HOLD#.
///
/// [`Bus::settle`] is one half clock period. Everything here is a
/// diagnostic run once per boot on a board that has already failed to
/// answer, so the implementation should be generous: hundreds of kHz with
/// slack beats anything tuned.
pub trait Bus {
    /// Drive SCK. `true` is the rising edge, `false` the falling one.
    fn set_sck(&mut self, high: bool);
    /// Drive CS#. `false` selects the part.
    fn set_cs(&mut self, high: bool);
    /// Drive IO0, the part's SI.
    fn set_io0(&mut self, high: bool);
    /// Sample IO1, the part's SO. `true` is a high level on the wire, so a
    /// bus nobody drives reads as whatever the pin's pull leaves it at.
    fn read_io1(&mut self) -> bool;
    /// Wait half a clock period.
    fn settle(&mut self);
}

/// Clock out `0x9F` and shift in the three bytes the part answers with.
///
/// Returns `[0, 0, 0]` when nothing on the bus drives IO1 low-to-high —
/// which is a real answer, not an error: it says the wire stayed at its
/// idle level for all 24 clocks. The caller reports the bytes and draws no
/// conclusion here.
///
/// Leaves CS# high and SCK low, the state it started the transfer from.
/// Restoring the *pin configuration* is the caller's job.
pub fn read_jedec_id<B: Bus>(bus: &mut B) -> [u8; 3] {
    // Idle: CS# deasserted, clock parked low, and a settle so the part
    // sees a clean level on both before the select.
    bus.set_cs(true);
    bus.set_sck(false);
    bus.settle();

    bus.set_cs(false);
    bus.settle();

    for i in (0..8).rev() {
        write_bit(bus, (CMD_READ_JEDEC_ID >> i) & 1 == 1);
    }

    let mut id = [0u8; 3];
    for bit in 0..RESPONSE_BITS {
        if read_bit(bus) {
            id[bit / 8] |= 0x80 >> (bit % 8);
        }
    }

    bus.set_cs(true);
    bus.settle();
    id
}

/// One command bit: present it while SCK is low, then a full clock.
///
/// Ends on the falling edge, which is the edge the part launches its own
/// data on. That is what makes the first [`read_bit`] after the eighth of
/// these sample the part's first id bit rather than a clock too early.
fn write_bit<B: Bus>(bus: &mut B, bit: bool) {
    bus.set_io0(bit);
    // Setup time: the level is on IO0 before the part samples it.
    bus.settle();
    bus.set_sck(true);
    bus.settle();
    bus.set_sck(false);
    bus.settle();
}

/// One data bit: sample IO1 at the end of the high phase, then fall.
///
/// Sampling *after* the settle rather than immediately at the edge gives
/// the part a whole half period to have driven the level, which on a bus
/// clocked this slowly is a large margin over any part's output delay.
fn read_bit<B: Bus>(bus: &mut B) -> bool {
    bus.set_sck(true);
    bus.settle();
    let bit = bus.read_io1();
    bus.set_sck(false);
    bus.settle();
    bit
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NOR part on the other end of four wires, in software.
    ///
    /// It models the one thing the shifter can get wrong: which edge does
    /// what. It samples IO0 on rising edges, launches its answer on
    /// falling ones, counts both, and refuses to do anything at all while
    /// CS# is high — so a shifter that clocks a bit too early, too late,
    /// or outside the selection produces a different id here.
    struct FakePart {
        /// The three bytes it answers `0x9F` with.
        answer: [u8; 3],
        /// Whether anything is on the bus to drive IO1 at all. A `false`
        /// part never launches; the wire keeps [`FakePart::idle`].
        present: bool,
        /// Level IO1 sits at before the part drives it, and whenever it is
        /// not driving. A real deselected part leaves the wire to its pull.
        idle: bool,
        cs_high: bool,
        sck_high: bool,
        io0: bool,
        /// Rising edges seen since select — the part's sample clock.
        rising: usize,
        /// Falling edges seen since select — the part's launch clock.
        falling: usize,
        /// Command byte as sampled off IO0, MSB first.
        command: u8,
        /// What the part is presenting on IO1 right now.
        so: bool,
        /// Edges clocked while CS# was high. Any is a bug in the shifter.
        edges_while_deselected: usize,
        /// Half periods waited. Only ever asserted as "nonzero".
        settles: usize,
    }

    impl FakePart {
        fn new(answer: [u8; 3]) -> Self {
            Self {
                answer,
                present: true,
                idle: false,
                cs_high: true,
                sck_high: false,
                io0: false,
                rising: 0,
                falling: 0,
                command: 0,
                so: false,
                edges_while_deselected: 0,
                settles: 0,
            }
        }

        /// A part that is absent, or unpowered, or simply not wired to
        /// these pins: the wire keeps whatever level it idles at.
        fn silent(idle: bool) -> Self {
            let mut part = Self::new([0, 0, 0]);
            part.present = false;
            part.idle = idle;
            part.so = idle;
            part
        }

        fn answer_bit(&self, index: usize) -> bool {
            self.answer[index / 8] & (0x80 >> (index % 8)) != 0
        }
    }

    impl Bus for FakePart {
        fn set_sck(&mut self, high: bool) {
            if high == self.sck_high {
                return;
            }
            self.sck_high = high;
            if self.cs_high {
                self.edges_while_deselected += 1;
                return;
            }
            if high {
                // Rising: the part samples SI. Only the first eight bits
                // are a command; anything after is the master's business.
                if self.rising < 8 {
                    self.command = (self.command << 1) | u8::from(self.io0);
                }
                self.rising += 1;
            } else {
                self.falling += 1;
                // Falling: the part launches. The eighth falling edge ends
                // the command byte and carries the first id bit with it.
                if self.present && self.falling >= 8 {
                    let index = self.falling - 8;
                    self.so = if index < RESPONSE_BITS {
                        self.answer_bit(index)
                    } else {
                        self.idle
                    };
                }
            }
        }

        fn set_cs(&mut self, high: bool) {
            if high && !self.cs_high {
                // Deselect resets the part's view of the transfer and
                // releases the wire.
                self.rising = 0;
                self.falling = 0;
                self.so = self.idle;
            }
            self.cs_high = high;
        }

        fn set_io0(&mut self, high: bool) {
            self.io0 = high;
        }

        fn read_io1(&mut self) -> bool {
            if self.cs_high {
                self.idle
            } else {
                self.so
            }
        }

        fn settle(&mut self) {
            self.settles += 1;
        }
    }

    #[test]
    fn reads_back_the_id_the_part_replays() {
        let mut part = FakePart::new([0xC2, 0x28, 0x15]);
        assert_eq!(read_jedec_id(&mut part), [0xC2, 0x28, 0x15]);
    }

    #[test]
    fn sends_the_jedec_opcode_msb_first() {
        let mut part = FakePart::new([0x9D, 0x60, 0x14]);
        read_jedec_id(&mut part);
        assert_eq!(part.command, 0x9F);
    }

    /// The ISSI part too, so the test is on the shifter and not on one
    /// byte pattern that happens to survive an off-by-one.
    #[test]
    fn reads_back_the_other_boards_part() {
        let mut part = FakePart::new([0x9D, 0x60, 0x14]);
        assert_eq!(read_jedec_id(&mut part), [0x9D, 0x60, 0x14]);
    }

    /// Every bit position, alone. A shifter that samples one edge early or
    /// late, or fills the bytes in the wrong order, moves the set bit and
    /// this catches it wherever it lands — including at the seam between
    /// the command byte and the answer, which is where the edge that
    /// matters is.
    #[test]
    fn every_single_bit_lands_where_it_was_sent() {
        for bit in 0..RESPONSE_BITS {
            let mut answer = [0u8; 3];
            answer[bit / 8] = 0x80 >> (bit % 8);
            let mut part = FakePart::new(answer);
            assert_eq!(
                read_jedec_id(&mut part),
                answer,
                "bit {bit} did not come back in its own place"
            );
        }
    }

    /// A part that answers all-ones is not the same reading as a bus at
    /// rest, and both have to survive the shifter unchanged: `ff:ff:ff` is
    /// what a pulled-up undriven wire looks like, and telling it from a
    /// real id is the whole point of the line this feeds.
    #[test]
    fn saturated_answers_survive() {
        let mut part = FakePart::new([0xFF, 0xFF, 0xFF]);
        assert_eq!(read_jedec_id(&mut part), [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn a_bus_nobody_drives_reads_as_its_idle_level() {
        let mut low = FakePart::silent(false);
        assert_eq!(read_jedec_id(&mut low), [0x00, 0x00, 0x00]);

        let mut high = FakePart::silent(true);
        assert_eq!(read_jedec_id(&mut high), [0xFF, 0xFF, 0xFF]);
    }

    /// The part is selected for the whole transfer and for nothing else.
    /// A clock edge outside the selection is a bug the fake counts, and
    /// the transfer has to end deselected so the next driver on these pins
    /// starts from the state this one did.
    #[test]
    fn clocks_only_while_selected_and_leaves_the_bus_idle() {
        let mut part = FakePart::new([0xC2, 0x28, 0x15]);
        read_jedec_id(&mut part);
        assert_eq!(part.edges_while_deselected, 0);
        assert!(part.cs_high, "transfer must end with the part deselected");
        assert!(
            !part.sck_high,
            "transfer must end with the clock parked low"
        );
    }

    /// Exactly 32 clocks: eight of command, 24 of answer. One more or one
    /// fewer is the off-by-one this whole file exists to catch, and the
    /// count is checked before the deselect resets it.
    #[test]
    fn clocks_the_transfer_exactly_once() {
        struct Counting<'a> {
            part: &'a mut FakePart,
            rising_at_last_sample: usize,
        }

        impl Bus for Counting<'_> {
            fn set_sck(&mut self, high: bool) {
                self.part.set_sck(high);
            }
            fn set_cs(&mut self, high: bool) {
                if high && !self.part.cs_high {
                    self.rising_at_last_sample = self.part.rising;
                }
                self.part.set_cs(high);
            }
            fn set_io0(&mut self, high: bool) {
                self.part.set_io0(high);
            }
            fn read_io1(&mut self) -> bool {
                self.part.read_io1()
            }
            fn settle(&mut self) {
                self.part.settle();
            }
        }

        let mut part = FakePart::new([0xC2, 0x28, 0x15]);
        let mut counting = Counting {
            part: &mut part,
            rising_at_last_sample: 0,
        };
        read_jedec_id(&mut counting);
        assert_eq!(counting.rising_at_last_sample, 8 + RESPONSE_BITS);
    }

    /// Every edge is separated from the next by a half period. Without
    /// this the shifter would still be arithmetically right and would
    /// still fail on a board, because the levels would never settle.
    #[test]
    fn waits_between_every_edge() {
        let mut part = FakePart::new([0xC2, 0x28, 0x15]);
        read_jedec_id(&mut part);
        // 2 for the idle-then-select preamble, 3 per command bit,
        // 2 per answer bit, 1 for the deselect.
        assert_eq!(part.settles, 2 + 8 * 3 + RESPONSE_BITS * 2 + 1);
    }
}
