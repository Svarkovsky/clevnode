# How far one firmware build reaches

Every board we support costs a build, a bundle entry, a row in the test
matrix and a place in everyone's head. The policy that keeps that cost
from growing with the hardware catalogue:

> **A firmware build serves a whole family of boards. A build for one
> hardware configuration is only justified where universality is
> unreachable, and the burden of proof lies with the specialised build.**

This is not an aspiration. It is already how the two builds we have
behave, and it was decided twice before it was written down.

## Why a family and not a board

Boards differ in dozens of ways and almost none of them matter. What
decides whether firmware runs at all is the SX1262 wiring: seven pins,
plus the TCXO voltage and whether DIO2 owns the antenna switch. Every
other difference is peripheral in the literal sense.

Those seven pins are usually not a property of the product. They are a
property of whatever part carries the radio. When the radio ships
together with the MCU as one module, every carrier board built around
that module inherits identical wiring, and a product family of a dozen
devices collapses to a single set of pins. The RAK4630 is the clean case:
twelve different carriers in Meshtastic's tree, from the Pocket V2 to a
solar repeater to an Ethernet gateway, all repeat the same seven numbers
because the RAK4630 datasheet fixes them.

The unit of support is therefore the pinout family, and the concrete
membership lists live in
[Supported boards](../firmware/boards.md).

## What universality requires of the firmware

A single image only serves a family if everything a carrier adds either
announces itself or costs nothing when absent. That is a design
constraint on peripheral handling, not a hope:

- **Probe where the bus allows it.** The RAK baseboard display is found
  by an I2C address probe (`ack_probe`, `leviculum-nrf/src/display.rs:55`);
  when nothing answers, the task logs and exits
  (`DetectedKind::None`, `leviculum-nrf/src/display.rs:158-164`).
- **Fail into the harmless state.** The user button is configured
  `Pull::Up` (`leviculum-nrf/src/button.rs:36`), so an absent button
  reads as not pressed rather than as noise.
- **Park rather than spin.** The GNSS task awaits a UART that simply
  stays silent when no receiver is fitted.
- **Publish nowhere.** The battery task samples a pin that floats on a
  bare module, but its only subscriber is the display task that is not
  running, so no wrong reading escapes.

Measured on this tree, carrying all of that costs 47.6 KiB of flash and
2.5 KiB of RAM over the stripped build. The RAM figure is affordable by
construction rather than by luck: the same image already runs on a
populated carrier with the same chip and the same memory, and a carrier
board adds peripherals, never RAM.

Where a bus cannot be probed, writing blind is acceptable only for a
known board. The T114 drives its ST7789 panel blind because MISO is not
connected and detection is physically impossible, which is safe because
we know what else is on those pins. The same reasoning does not transfer
to an unfamiliar board, where the identical pins may carry something that
must not be driven.

**Runtime detection is therefore the lever, and a build-time feature is
the fallback.** Every peripheral moved from a feature flag to a probe
removes a reason for a second build. Codeberg #240 does this for GNSS
presence.

## When a specialised build is justified

Three conditions, any one of which is sufficient:

1. **The radio wiring differs.** No amount of runtime detection recovers
   from pins that are simply elsewhere.
2. **A radio parameter is board-specific rather than family-specific and
   is compiled in.** The Elecrow ThinkNode M1 matches all seven T114 pins
   but runs its TCXO at 3.3 V against the family's 1.8 V. Either the
   value becomes data, or the board needs its own build.
3. **A peripheral is dangerous when mishandled.** A board with an
   external power amplifier needs its enable line driven correctly.
   Silence is not a safe default there, unlike a missing display.

Convenience, code tidiness, and "it would be cleaner to separate them"
are not on this list.

**A frequency region is not on it either.** The compiled default is the
eu868 community profile, but the radio configuration is data, chosen at
flash time: `lnflash` ends every flash with a preset menu — eu868,
us915, au915, or a custom five-number entry — and stores the choice on
the board (see [Flashing an LNode](lnode-flashing.md), "The radio
configuration belongs to the flash"). The presets are the settings each
regional Reticulum community has converged on; a default, not legal
advice.

## The limit that bites

Universality reaches exactly as far as the identification does. A build
may serve twelve carriers, but something has to decide that the board in
front of it is one of those twelve, and that decision is made from the
bootloader (see [Flashing an LNode](lnode-flashing.md)). Where the
bootloader identifier is bound to the same unit as the wiring, the two
line up and the family is safe end to end. Where a vendor shares one
identifier across models with different wiring, a correct universal build
can still be written onto a board it does not fit.

So the reach of a build and the precision of its identification have to
be argued together. A family is only as wide as the narrowest of the two.
