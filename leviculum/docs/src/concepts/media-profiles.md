# Media profiles

An LNode meshes over LoRa **and** BLE at once, by default, from the
moment it boots. That is the right behaviour for a field node and the
wrong one for a measurement: a packet delivered over the other medium
masks a loss on the medium under test, so *every single-medium number a
dual-carrier node produces is falsifiable*. "LoRa PDR was 94 %" is not a
statement about LoRa if BLE was carrying the same mesh.

A media profile is the node's declaration of which carriers it meshes
over. It is **declared** (a host says so), **applied** (the firmware
honours it at boot and at runtime), and **proven** (the board says out
loud, every boot, what it is on). All three are needed: a declaration
nothing applies is a comment, and an application nothing proves is a
hope.

## The default is both carriers on

Absence of a profile changes nothing. A board with no record on its flash
page, a board whose record is corrupt, a board whose record names a
carrier this firmware does not know — all of them come up on both
carriers, which is what every fielded board is already doing. There is no
firmware update on this path that can take a board off the mesh.

That is why the record has a magic and a checksum rather than being a
bare flag byte: a zeroed page read as flags would say "both carriers
off", which is a silent way to lose a node.

## Setting one

```console
$ lnflash --set-media lora=on,ble=off
/dev/ttyACM0: media profile set — running lora=on ble=off, configured lora=on ble=off.
  It survives resets; the board's own [MEDIA] line on if00 says the same.
```

`lora=` and/or `ble=`, comma or space separated, `on`/`off`
(`true`/`false`, `yes`/`no`, `1`/`0` also accepted), keys and values
case-insensitive. A carrier not named keeps the board's own setting —
`lnflash` reads the board back first and applies the spec on top, the
same read-modify-write contract `--set-tx-power` holds for the radio, and
for the same reason: a host that substitutes its own default for the
field it did not mean to touch changes what it claimed not to change.

`--set-media` with no value only reads:

```console
$ lnflash --set-media
/dev/ttyACM0: running lora=on ble=off, configured lora=on ble=off.
```

## What "off" means on the board

**At boot**, a carrier that is off is never started. The LoRa task that
resets, configures and keys the SX1262 is not spawned, so the chip is
never brought up; the Columba tasks are not spawned, so there is no
advertisement, no scan, no GATT service and no connection. Nothing is
transmitted and nothing is received.

The SoftDevice is still enabled with `ble=off`, deliberately. It is not
only the BLE stack: `sd_flash_write` is the one legal way to write
internal flash once it is enabled, and both persistence store tasks ride
on it. A board that could not persist its own profile could not be put
back on BLE — the one state this feature must never be able to reach.

**At runtime**, switching a carrier off stops it carrying Reticulum
traffic in both directions immediately: the interface drops what the core
hands it and the binary's receive arm drops what the medium hands up.

For BLE the runtime off also takes the carrier off the air: the Columba
tasks disconnect every live link, central and peripheral role — a
connected phone sees the board go, exactly as if it had left range — and
stop advertising and scanning. Each dropped link unwinds through the
same per-link teardown as range loss, so the core receives the same
peer-lost report and culls its paths identically.

For LoRa it does not: the LoRa task keeps listening (nothing is
transmitted, and what it hears is dropped before the core sees it). LoRa
radio silence needs the boot path, which is why the acceptance for LoRa
silence is set-then-reset and not a runtime set.

## What "on" means, and when it needs a reset

Switching a carrier back on is immediate **if it came up at boot** — its
tasks are still there, gated, and for BLE they resume advertising and
scanning at once. A carrier that did *not* come up has no task to
un-gate, and an embassy task cannot be spawned from nothing after the
fact, so it cannot start before the next reset.

The board says which case it is in rather than acking either way. Both
media frames are answered with a report carrying two profiles:

* `running` — what the board is carrying traffic on right now.
* `configured` — what a reset would come up with.

They differ exactly when a carrier is waiting for a reset, and `lnflash`
renders that difference as a sentence naming the carrier. An ack would
have said "done" to a request the board cannot honour yet, and a
measurement run reading that ack would believe it had a BLE link that
does not exist.

Because "configured on, not running" is a *terminal* statement — the
host prints "Reset the board" for it — the board must not be able to say
it while it is merely still booting. USB comes up before the carriers by
design, so the serial task answers frames during a window in which
nothing has been spawned yet; in that window the board reports the
declared profile as running, and narrows it to what really started as
soon as both spawn decisions are made. Without that, a set-then-reset
script that connects the moment the tty appears reads "did not come up"
off a board that came up perfectly (seen on the rig, #255).

## The proof line

Every boot, on the debug CDC, before anything can have moved:

```text
[MEDIA] lora=on ble=off src=flash t=1183
```

The two carrier fields are what the board is **running**; `src=` is
`flash` for a profile read off the page and `default` for the both-on
fallback. It is re-emitted with the `[FW_BUILD]` banner every five
seconds, so a capture attached after the boot window still reads the
carriers off the board rather than off an operator's memory, and a
runtime change shows up within five seconds.

**The shape is an interface.** Assertions read it; it is frozen in
`docs/src/structured-event-logs.md` and must not drift.

## Where the pieces live

| Piece | Where |
|-------|-------|
| Wire format, both frames and the report | `leviculum-core/src/envelope.rs` (`TYPE_MEDIA_PROFILE`, `TYPE_MEDIA_QUERY`, `TYPE_MEDIA_REPORT`) |
| Flash record | `leviculum-core/src/media_profile_store.rs` (`"LMED"`) |
| Page layout and the store task | `leviculum-nrf/src/telemetry.rs` (`+0x200` on `BoardConfig::telemetry_flash_page`) |
| Runtime state, the gate and the banner | `leviculum-nrf/src/media.rs` |
| Running/configured state machine, boot window, drop-run reporting (host tests) | `leviculum-nrf/media-state/` |
| Boot spawn decisions | `leviculum-nrf/src/bin/{t114,rak4631}.rs`, `leviculum-nrf/src/ble/mod.rs` |
| Host flag | `lnflash/src/media.rs`, `lnflash/src/flow.rs` |

## What this is not

It is not a power-saving feature and not a way to run a node on one
carrier in the field — a node with a carrier off is a node that cannot be
reached over it, which for a mesh is a fault, not a setting. It exists so
that a measurement can say which medium it measured. Deployments run both
carriers; that is the default and it stays the default.
