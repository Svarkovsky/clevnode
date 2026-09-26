# lnflash(1)

## NAME

lnflash -- flash, configure and watch LNode boards

## SYNOPSIS

**lnflash** [*options*]\
**lnflash** **--set-time** | **--set-telemetry** | **--set-media** [*spec*] | **--set-name** [*name*] | **--set-position** *lat,lon[,alt]* | **--set-tx-power** *dbm* | **--set-tx-spacing** *ms* | **--set-ble-tx-gap** *ms* | **--announce**\
**lnflash** **--watch** [*serial-or-port*] [**--out** *file*]\
**lnflash** **--summarize** *file*

## DESCRIPTION

**lnflash** brings an LNode board up on the firmware bundle beside the binary: it finds attached boards, brings each into its bootloader, confirms from the bootloader what the board is, checks the SoftDevice precondition and writes. After a flash it offers radio settings and a telemetry target. It needs no network and no external programs; writing needs root because the bootloader drive is a root:disk block device.

The configure sessions (**--set-** *something*) talk to boards that are already running and never flash: activation is configuration, not firmware. Each session ends the run, so only one of them can be given at a time.

**--watch** and **--summarize** are the field-testing pair: the first records a board's debug log with wall-clock timestamps, the second reads such a recording back. See FIELD WATCH below.

## FLASHING OPTIONS

**--bundle** *path*
:   Where the bundle is. Otherwise $LNFLASH_BUNDLE, then next to the binary, then /usr/share/lnflash.

**--board** *name*
:   Only flash this board; refuse anything else.

**--dry-run**
:   Report what is attached and what would happen; change nothing.

**--yes**
:   Answer yes to every confirmation. Fails rather than waits when a board needs a physical double-tap.

**--check-bundle**
:   Verify the bundle's manifest and payload checksums, then exit.

**--radio-preset** *name*, **--radio-freq** *hz*, **--radio-bw** *hz*, **--radio-sf** *n*, **--radio-cr** *n*, **--radio-txpower** *dbm*, **--no-radio**
:   The radio settings written after a flash. A preset (eu868, us915, au915) and explicit values are two ways to state one configuration; pick one. **--no-radio** leaves the board's stored settings alone.

**--telemetry** *address*, **--telemetry-profile** *name*, **--telemetry-key** *hex*, **--no-telemetry**
:   The telemetry target offered after a flash, or the answer for **--set-telemetry**.

**--quiet**
:   Print less. With **--watch**, print nothing (an **--out** file is then required).

## CONFIGURE SESSIONS

Each finds every running LNode on the bus, talks to it over the control envelope on the transport CDC (if02), reports what each board answered and exits. **--set-time** teaches the boards the host clock; **--set-tx-spacing** sets the on-air transmit spacing (not persisted); **--set-tx-power** sets the transmit power (persisted); **--set-position**/**--clear-position** pin or release a fixed position; **--set-media** reads or sets which carriers a board meshes over; **--set-name**/**--clear-name** read or set what the board is called; **--set-telemetry** configures the telemetry target; **--set-ble-tx-gap** sets the gap the board leaves between packets on one Bluetooth connection, 0 to 5000 ms (not persisted; 0 imposes nothing); **--announce** makes each board announce its LXMF delivery destination immediately on all interfaces, exactly as its telemetry path does — a board without a calendar clock withholds the announce and says so (run **--set-time** first). A flag given with no value, where allowed, only reads the boards back.

## FIELD WATCH

**--watch** [*serial-or-port*]
:   Open a running board's debug CDC (if00) with DTR and RTS raised — the firmware transmits only with both set — and keep reading. Every line is prefixed with an ISO-8601 wall-clock timestamp with milliseconds (the board's own `t=` stays in the line). If the port vanishes (reset, reflash, unplug) the gap is logged as its own `[WATCH]` line and the port is reopened with a bounded backoff; the watch never exits on EOF. With no value and exactly one running board, that board is watched; with several, name a board's USB serial or bus port (e.g. 3-2.4). A value containing a slash is opened directly as a serial port path. The watch runs until interrupted. It is not a daemon and has no background mode; run it in a terminal, or under **nohup**(1) yourself.

**--out** *file*
:   Append every watched line to *file* as well as stdout, flushed per line so a crash loses nothing. The file is the evidence a field walk leaves.

**--summarize** *file*
:   Read a watch file and print, per hour, how many LoRa receptions of each class it holds — announce, data, path request — plus the last line seen per class and the number of reconnect gaps. Classification uses the `flags=` byte in a `[LORA] RX` line when present (the low two bits are the Reticulum packet type; a data packet to the well-known path request destination is a path request) and the class word otherwise; a bare `RX n bytes` line counts as unclassified. The watch itself never filters: the file is the evidence, the summary is a view of it.

## EXAMPLES

Watch the only attached board, keeping the log:

    lnflash --watch --out walk-$(date +%F).log

Watch one of several boards by serial, silently:

    lnflash --watch 183004F712B4A7FE --out walk.log --quiet

Summarize the walk afterwards:

    lnflash --summarize walk.log

## EXIT STATUS

0 when every addressed board did what was asked (for **--watch**: never reached; the watch runs until killed).

On a flash run the three outcomes are kept apart, because they need different things done about them:

0
:   Every board was written and named the build in this bundle on its debug port.

1
:   The flash failed: a board never came back, or came back naming a different build, or nothing was written to it.

2
:   Every board took the write and none contradicted it, and at least one could not be read back. The firmware is on the board as far as anything here knows; which build it is running is unknown. Read it back again (**--watch**, or re-run the flash) rather than assuming the write failed.

Every other session exits 0 when every addressed board did what was asked and 1 otherwise.

## SEE ALSO

**lnsd**(1), **lnstatus**(1), **lnprobe**(1)

The flashing design and its evidence: `docs/src/concepts/lnode-flashing.md`. Field testing: `docs/src/guide/field-testing.md`.
