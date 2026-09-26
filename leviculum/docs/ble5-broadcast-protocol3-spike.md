# Spike: ble-leviculum, a BLE 5 broadcast carrier (protocol 3)

Read only investigation. No code was built or changed. Goal: decide whether
a connectionless BLE 5 broadcast mesh is feasible on the current nRF stack
before committing it to the roadmap. This note is meant to seed a Codeberg
issue for protocol 3.

Protocol name: **ble-leviculum**, a BLE 5 connectionless broadcast carrier for
Reticulum packets. Leviculum originated, not an upstream RNS standard. See the
manual chapter "Bluetooth interfaces" for the three protocol naming scheme.

**Findings valid as of:** 2026-06-26, against `nrf-softdevice` rev 5949a5b and
SoftDevice S140 v7.0.0. The payload and feature numbers below are pinned to that
dependency. If the submodule or the SoftDevice is bumped, re-run this spike, the
255 byte limit and the absence of periodic advertising can change.

## Background: three distinct Bluetooth usage protocols

1. BLE as a cable (Python RNS style). Connection oriented GATT link used to
   drive a dumb RNode radio. The peer is a radio, not a Reticulum node.
   Enabled by the byte channel seam in PR #80. Related: #26.
2. BLE 4 mesh (Columba). Connection oriented, one link per peer, the peer is
   a full Reticulum node. Hardware allows only 3 or 4 simultaneous links, so
   it does not scale. Partial implementation already in tree
   (leviculum-core/src/framing/ble.rs, leviculum-nrf/src/ble/), buggy and
   incomplete. Tracked by #45.
3. BLE 5 broadcast mesh (this spike). Connectionless extended advertising, so
   any number of devices in range form a mesh without per peer links. New
   carrier protocol. Not tracked yet.

## Question

Does the pinned nrf-softdevice expose BLE 5 extended advertising, and what is
the per advertisement payload ceiling versus the Reticulum MTU?

## Environment probed

- nrf-softdevice git rev 5949a5b (leviculum-nrf/Cargo.toml:76).
- SoftDevice S140 v7.0.0 (nrf-softdevice-s140/src/bindings.rs:855,
  SD_VERSION = 7000001).
- Current firmware features: ble-peripheral, ble-gatt-server only. No
  ble-central, no periodic advertising (leviculum-nrf/Cargo.toml feature
  aggregator).
- Reticulum MTU 500 bytes (leviculum-core/src/constants.rs:47).

## Findings

### Extended advertising is supported in the high level wrapper

nrf-softdevice/src/ble/peripheral.rs exposes extended variants on both
ConnectableAdvertisement and NonconnectableAdvertisement, mapped to the
BLE_GAP_ADV_TYPE_EXTENDED_* kinds. The connectionless broadcast primitive
exists: BLE_GAP_ADV_TYPE_EXTENDED_NONCONNECTABLE_NONSCANNABLE_UNDIRECTED
(bindings.rs:572, value 10). advertise() does not cap the data at 31 bytes;
it passes the length through as u16 to sd_ble_gap_adv_set_configure.

### Payload ceiling is 255 bytes, not the BLE 5 theoretical 1650

From nrf-softdevice-s140/src/bindings.rs:

- BLE_GAP_ADV_SET_DATA_SIZE_MAX = 31 (legacy)
- BLE_GAP_ADV_SET_DATA_SIZE_EXTENDED_MAX_SUPPORTED = 255 (nonconnectable)
- BLE_GAP_ADV_SET_DATA_SIZE_EXTENDED_CONNECTABLE_MAX_SUPPORTED = 238

So one extended advertisement carries at most 255 bytes. After a few bytes of
AD structure and a carrier marker, usable Reticulum payload is about 245 bytes.

### Receive side exists but is gated

nrf-softdevice/src/ble/central.rs scan() handles BLE_GAP_EVT_ADV_REPORT and has
ScanConfig.extended to accept extended advertising reports. This sits behind
the ble-central feature, which is currently off.

### Periodic advertising is absent in S140 7.0.0

No periodic advertising symbols in the bindings. Scanner synchronised periodic
advertising would require a newer SoftDevice. Not required for a basic
broadcast mesh; repeated extended advertising is sufficient.

## Verdict

Feasible on the current stack. The make or break question is answered yes:
connectionless extended advertising is available, without the 31 byte legacy
limit.

Architectural fit is clean. To the core a BLE 5 broadcast interface is just
another lossy broadcast medium, the same model LoRa already uses, so the
existing robustness logic applies. The 245 byte ceiling and any fragmentation
are carrier quirks that live in the interface, consistent with the interface
isolation rule.

## Consequence: fragmentation for full size packets

Small packets (announces, path requests, most broadcast control traffic) fit
in one advertisement. A full 500 byte Reticulum packet does not and must be
fragmented across two advertisements and reassembled. Because broadcast is
connectionless and lossy, a fragmented large packet only arrives if both
fragments do. Reliable large transfers use links over a connection oriented
path, not broadcast, so this is acceptable, but the fragmentation and
reassembly belong in the BLE 5 interface.

## Open questions (next nails, outside the nRF stack)

- lnsd side: does BlueZ on the target adapters support extended advertising TX
  and RX, via the bluer crate.
- Phone side: extended advertising support is hardware and OS dependent.
- Exact framing: AD structure and carrier marker (service UUID or manufacturer
  specific data) to identify Reticulum advertisements among others.
- Fragmentation header format and reassembly timeout at the interface.
- Optional: SoftDevice upgrade to 7.2.0 or later if scanner synchronised
  periodic advertising is wanted.

## Proposed scope for the protocol 3 issue

1. Enable ble-central so a node can scan and receive extended advertisements.
2. Define the BLE 5 broadcast carrier framing (marker plus fragmentation
   header) with usable payload about 245 bytes per advertisement.
3. Implement a BLE 5 broadcast interface (TX via nonconnectable extended
   advertising, RX via extended scan) with fragmentation and reassembly,
   presented to the core as a normal lossy broadcast Interface.
4. Validate on nRF first, then lnsd via bluer, then a phone smoke test.
