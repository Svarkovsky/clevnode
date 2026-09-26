# LNode Firmware: USB Serial Ports

A flashed LNode presents **two** USB CDC-ACM serial ports to the host.
Knowing which is which is the difference between reading a debug log and
talking the Reticulum transport protocol.

## The two ports

The firmware exposes two CDC-ACM serial ports. The **lower-numbered**
port is the debug log output; the **higher-numbered** port is the
Reticulum transport interface that carries HDLC frames. The actual
`/dev/ttyACM*` numbers depend on what else is plugged into USB.

> The firmware exposes two USB CDC-ACM serial ports. The lower-numbered
> port is the debug log output. The higher-numbered port is the
> Reticulum transport interface that carries HDLC frames. The actual
> `/dev/ttyACM*` numbers depend on other connected USB devices.
> (`leviculum-nrf/README.md:44-46`)

Each CDC-ACM class occupies two USB interfaces (a Communication
interface plus a Data interface), so the two ports map onto four USB
interface numbers:

| Port | USB interface nums | Carries |
|------|--------------------|---------|
| Debug | 00 (comm) + 01 (data) | human-readable log lines |
| Transport | 02 (comm) + 03 (data) | Reticulum HDLC frames |

(`leviculum-nrf/udev/99-leviculum.rules`, header comment.)

## Stable device paths via udev

Because the `/dev/ttyACM*` enumeration order is not stable, install the
shipped udev rules to get fixed symlinks:

```sh
sudo cp udev/99-leviculum.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
```

(`leviculum-nrf/README.md:50-53`)

After the next plug-in, the symlinks point at the correct ports
regardless of enumeration order. The names are board-family specific,
keyed off the per-board USB PID:

| Board | USB VID:PID | Debug symlink | Transport symlink |
|-------|-------------|---------------|-------------------|
| T114 | `1209:0001` | `/dev/leviculum-debug` | `/dev/leviculum-transport` |
| RAK4631 / Pocket V2 | `1209:0002` | `/dev/leviculum-rak-debug` | `/dev/leviculum-rak-transport` |

(Symlink names and PIDs: `leviculum-nrf/udev/99-leviculum.rules`. The
firmware-side USB VID/PID constants:
`leviculum-nrf/src/boards/t114.rs:139-140` for `1209:0001`,
`leviculum-nrf/src/boards/rak4631.rs:126-127` for `1209:0002`.)

> **Multiple boards of the same kind.** The short symlinks
> (`/dev/leviculum-transport`) land on whichever device udev sees first.
> The rules also emit per-serial-number symlinks
> (`/dev/leviculum-transport-<SERIAL>`); use those when more than one
> board of the same family is attached.
> (`leviculum-nrf/udev/99-leviculum.rules`, header comment and
> `SYMLINK+="leviculum-transport-%s{serial}"` lines.)

Without the rules installed there is still a stable path: systemd's own
`/dev/serial/by-id/` entries carry the firmware's USB strings and the
board serial, and the CDC interface number distinguishes the two ports
the same way (`-if00` debug, `-if02` transport):

```
/dev/serial/by-id/usb-leviculum_leviculum_T114_<SERIAL>-if00   debug
/dev/serial/by-id/usb-leviculum_leviculum_T114_<SERIAL>-if02   transport
```

## Reading the debug port

The debug port is plain text at 115200 baud:

```sh
picocom /dev/leviculum-debug -b 115200
```

(`leviculum-nrf/README.md:59-60`)

On the debug port you will see the boot banner, the firmware git SHA and
the periodic diagnostics the firmware emits: the `[FW_BUILD]` banner
every 5 s, the `[STACK]` watermark lines, and the LoRa TX/RX events.
(`leviculum-nrf/src/bin/t114.rs:399-408` for the banner task.) Do
**not** point `lnsd` at the debug port; it carries log text, not HDLC
frames.

> **The identity lines are not among them in practice.** The firmware
> prints `LNode started -- identity: …`, `[IDENTITY] t114_node=…` and
> `[IDENTITY] t114_probe=…` once during boot
> (`leviculum-nrf/src/bin/t114.rs:181-199`), but all three go through
> the runtime-gated log path: with no reader attached yet, `log_fmt`
> counts the line and returns before it reaches either the ring buffer
> or the persistent tail (`leviculum-nrf/src/log.rs:159-163`). A reader
> that attaches after boot sees them replaced by the gate's own
> summary, `[LOG_GATE] opened, dropped N runtime lines pre-attach`, and
> nothing re-emits them later. Read the destination hash off the
> network instead — see [Finding the node's destination
> hash](#finding-the-nodes-destination-hash).

## Querying panic evidence over the debug port

The debug port is not entirely write-only: it accepts one command. A
single `p` byte makes the firmware replay its persistent panic
evidence — the `[PANIC_COUNT] total=N` line and, if a post-mortem
record is stored, the full `[HARDFAULT_PMRT]` / `[PANIC_PMRT]` block,
bracketed by `[PM_QUERY] begin` / `[PM_QUERY] done` markers
(`leviculum-nrf/src/usb.rs`, `debug_reader_task`;
`leviculum-nrf/src/lib.rs`, `postmortem_query`).

This exists because the boot-time replay of the same block is emitted
exactly once into the 8 KiB log ring: after the 30 s headless fallback
opens the runtime-drain gate, runtime output laps the ring, so a host
that attaches later never sees it. The post-mortem records in retained
RAM (the `.retained` region in `leviculum-nrf/memory.x`, placed where
the Adafruit bootloader provably never writes)
survive the boot read (it marks them seen rather than erasing them)
and soft resets — power loss wipes them, and a reflash must be assumed
to — so the query can retrieve the evidence any time after the crash,
as long as the board stays powered.

That "retained RAM" is younger than the feature it carries. Until
9b4d82a (2026-08-31) the same five cross-boot records lived in
`.uninit`, which flip-link packs against the top of RAM — and the top
of RAM is where the Adafruit bootloader starts its stack
(`__StackTop = 0x20040000`, `nrf_common.ld`, confirmed by the initial
SP in the shipped bootloader's vector table). Every reset runs that
bootloader before our reset handler, so the hardfault post-mortem (top
36 B) and the boot trace (top 48 B) were overwritten on **every** boot
and could never have been read back; the panic post-mortem, the panic
counter (Codeberg #65) and the persistent log tail sat lower in the
same 3.1 KiB and survived only as far as the bootloader's stack
happened not to reach on a given boot. The evidence was a live positive
control on the rig: three consecutive commanded resets out of a running
system, with the reset cause latched as `sreq=1`, still read
`prev_magic=absent prev_boot=0`. The fix was placement, not logic — a
dedicated `RETAINED` region below the bootloader's stack floor and
outside every region it declares, held there by two link-time
`ASSERT`s. Read a `[PANIC_COUNT]` or `[PM_QUERY]` result from firmware
older than 9b4d82a as unreliable rather than as a zero.

The committed helper drives the whole exchange:

```sh
scripts/lnode-panic-query.sh /dev/leviculum-rak-debug
```

It asserts DTR+RTS (the debug port transmits only with DTR raised),
sends `p`, and prints the tagged response lines. Exit 0 means a
complete response was captured; on older firmware without the query
command it times out with exit 1. **Do not power-cycle a board whose
evidence you still need** — the retained region lives in RAM, and power
loss is the one thing that wipes it.

## Pointing a daemon at the transport port

The transport port carries HDLC-framed Reticulum packets. It is **not**
an RNode: a standalone LNode runs a complete stack in its own firmware
and is the daemon's *neighbour node*, not its radio. The firmware
implements no RNode KISS command set — there is no `CMD_DETECT`,
`CMD_FW_VERSION` or `CMD_PLATFORM` responder anywhere in
`leviculum-nrf/` — so `RNodeInterface` cannot drive it, and neither can
`rnodeconf`. The interface type is `SerialInterface`.

> `SerialInterface` is a raw serial HDLC link […] Leviculum's
> `SerialInterface` honours [the LoRa keys] too and configures the
> attached LNode's radio over the serial port — the LNode frames HDLC,
> so it cannot be driven by the KISS-framed `RNodeInterface`.
> (`docs/src/guide/configuration.md:182-191`)

```ini
[interfaces]

  [[LNode T114]]
    type = SerialInterface
    enabled = yes
    port = /dev/leviculum-transport
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    frequency = 869463000
    bandwidth = 125000
    txpower = 22
    spreadingfactor = 8
    codingrate = 5
```

For a RAK4631 / WisMesh Pocket V2 the only change is the port
(`/dev/leviculum-rak-transport`).

**Who applies the LoRa keys.** Under `lnsd` the five LoRa keys are sent
to the board as a radio-config frame at interface startup
(`leviculum-std/src/interfaces/serial.rs:186`), so the config decides
the channel. Under Python-RNS `rnsd` they are inert: its
`SerialInterface` reads port settings only and pushes nothing to the
board, which then keeps whatever profile is in its flash — the compiled
`eu_medium` default (869.463 MHz, BW 125 kHz, SF8, CR4/5, 22 dBm;
`leviculum-nrf/src/lora.rs:136-165`, `RadioConfig::eu_medium`) or the
preset chosen at flash time. The values above are that default written
out, so a Python-driven LNode and an `lnsd`-driven one land on the same
channel. Changing the channel of a Python-driven board is a reflash
(`lnflash --radio-preset`), not a config edit.

After editing `/etc/reticulum/config`, restart the daemon so it picks up
the new interface:

```sh
sudo systemctl restart lnsd
```

(Same restart flow as any config change; see the
[lnsd Quickstart](../lnsd-quickstart.md).)

## Confirming the link came up

Run the standard health-check and look for the new interface in the
`interface_stats` section with `status=up` and non-zero counters once
LoRa traffic flows:

```sh
lnstest diag --config /etc/reticulum
```

(`lnstest diag` usage and the `interface_stats` reading are described in the
[lnsd Quickstart](../lnsd-quickstart.md#check-its-working).)

## Finding the node's destination hash

A standalone LNode answers probes on one destination,
`rnstransport.probe`, and announces it 15 s after boot and then every
2 hours (`leviculum-core/src/node/mod.rs:513-517`;
`MGMT_ANNOUNCE_INTERVAL_MS`, `leviculum-core/src/constants.rs:163`).
The hash is carried in the announce itself, so the way to learn it is
to receive one, not to read it off the debug port.

With the interface configured and the daemon running, press the board's
reset button and wait about 20 s. The daemon reopens the port by itself
after the board re-enumerates, then records the announce:

```sh
rnpath -t
```
```
<6a1ab9ea64747f298c1f205dfcf0f5a3> is 1 hop away via <6a1ab9ea64747f298c1f205dfcf0f5a3> on SerialInterface[LNode T114]
```

The entry on the LNode's own interface is the board. The leading hash
is the destination; the `via` hash is the node's transport ID, which is
the same value here because a directly attached neighbour announces at
hop 0. Probing it takes the aspect name as well, since the name cannot
be recovered from the hash:

```sh
rnprobe rnstransport.probe 6a1ab9ea64747f298c1f205dfcf0f5a3
```
```
Valid reply from <6a1ab9ea64747f298c1f205dfcf0f5a3>
Round-trip time is 126.497 milliseconds over 1 hop
```

Miss the 15 s window and the next announce is 2 hours out; resetting
the board again is quicker.

The probe destination is the only addressed service the firmware
offers. Remote management is not enabled on the standalone binary
(`leviculum-nrf/src/bin/t114.rs:141` sets `respond_to_probes` and
nothing else), so `rnstatus -R` and `rnpath -R` have no responder;
`rncp`, `rnsh` and `rnx` have no counterpart either. What the board
does beyond that — forwarding announces, answering path requests,
relaying packets — needs no hash from the operator and shows up as
paths *via* the LNode in `rnpath -t`.

For the full key-by-key reference of the serial and LoRa keys, and the
meaning of the optional ones (`flow_control`, `airtime_limit_*`,
`preamble_symbols`), see the RNode and Serial section of the
[Configuration](../guide/configuration.md#rnode-and-serial-rnodeinterface-serialinterface)
chapter.
