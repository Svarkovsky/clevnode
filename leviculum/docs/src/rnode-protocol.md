# RNode Interface Protocol Research

Research based on Python RNS v1.1.3, source files:
- `RNS/Interfaces/RNodeInterface.py` (1558 lines)
- `RNS/Interfaces/RNodeMultiInterface.py` (1149 lines)
- `RNS/Interfaces/Interface.py` (302 lines, base class)
- `RNS/Interfaces/KISSInterface.py` (standard KISS, for comparison)

---

## 1. Serial Protocol

### 1.1 Framing

The RNode serial protocol uses **KISS framing**, not HDLC. This is a critical
distinction from the TCP/Serial framing used elsewhere in Reticulum.

| Constant | Value | Purpose |
|----------|-------|---------|
| `FEND`   | `0xC0` | Frame delimiter (start and end) |
| `FESC`   | `0xDB` | Escape byte |
| `TFEND`  | `0xDC` | Escaped FEND (after FESC) |
| `TFESC`  | `0xDD` | Escaped FESC (after FESC) |

**Frame format:**

```
[FEND 0xC0] [CMD byte] [escaped payload...] [FEND 0xC0]
```

**Escaping (KISS standard):**

When the payload contains `0xC0` (FEND) or `0xDB` (FESC), they are replaced:
- `0xDB` -> `0xDB 0xDD` (FESC TFESC)  -- escape is applied FIRST
- `0xC0` -> `0xDB 0xDC` (FESC TFEND)

Note the escape ordering: FESC bytes are escaped first, then FEND bytes.
This matches Python's `data.replace(bytes([0xdb]), bytes([0xdb, 0xdd])).replace(bytes([0xc0]), bytes([0xdb, 0xdc]))`.

**Comparison with HDLC framing (used for TCP):**

| Property | KISS (RNode) | HDLC (TCP) |
|----------|-------------|------------|
| Delimiter | `0xC0` | `0x7E` |
| Escape byte | `0xDB` | `0x7D` |
| Escape method | Substitution (`0xDC`/`0xDD`) | XOR with `0x20` |
| CRC | None | None (Reticulum simplified HDLC) |
| First byte after delimiter | Command byte | Payload starts immediately |

**Key difference:** KISS frames carry a command byte after the opening FEND.
Standard HDLC frames do not. The RNode protocol is a KISS superset with
RNode-specific command extensions.

### 1.2 Command Set (complete table)

#### Configuration Commands (Host -> Device, Device -> Host as confirmation)

| Command | Byte | Direction | Payload | Description |
|---------|------|-----------|---------|-------------|
| `CMD_DATA` | `0x00` | Both | Raw packet bytes (KISS-escaped) | Reticulum packet data |
| `CMD_FREQUENCY` | `0x01` | Both | 4 bytes, big-endian, Hz | Set/report operating frequency |
| `CMD_BANDWIDTH` | `0x02` | Both | 4 bytes, big-endian, Hz | Set/report channel bandwidth |
| `CMD_TXPOWER` | `0x03` | Both | 1 byte, dBm | Set/report TX power |
| `CMD_SF` | `0x04` | Both | 1 byte (5-12) | Set/report spreading factor |
| `CMD_CR` | `0x05` | Both | 1 byte (5-8) | Set/report coding rate (4/5 through 4/8) |
| `CMD_RADIO_STATE` | `0x06` | Both | 1 byte: `0x00`=off, `0x01`=on, `0xFF`=ask | Set/report radio on/off state |
| `CMD_RADIO_LOCK` | `0x07` | Device->Host | 1 byte | Report radio lock state |
| `CMD_DETECT` | `0x08` | Both | Host sends `0x73`, device responds `0x46` | Device presence detection handshake |
| `CMD_LEAVE` | `0x0A` | Host->Device | `0xFF` | Host is disconnecting (shutdown notification) |
| `CMD_ST_ALOCK` | `0x0B` | Both | 2 bytes, big-endian, value/100 = percent | Short-term airtime limit |
| `CMD_LT_ALOCK` | `0x0C` | Both | 2 bytes, big-endian, value/100 = percent | Long-term airtime limit |
| `CMD_READY` | `0x0F` | Device->Host | (none meaningful) | Device ready for next TX packet |

#### Statistics Commands (Device -> Host, unsolicited)

| Command | Byte | Direction | Payload | Description |
|---------|------|-----------|---------|-------------|
| `CMD_STAT_RX` | `0x21` | Device->Host | 4 bytes, big-endian | Total RX packet count |
| `CMD_STAT_TX` | `0x22` | Device->Host | 4 bytes, big-endian | Total TX packet count |
| `CMD_STAT_RSSI` | `0x23` | Device->Host | 1 byte (unsigned + 157 offset) | Last packet RSSI |
| `CMD_STAT_SNR` | `0x24` | Device->Host | 1 byte (signed * 0.25 dB) | Last packet SNR |
| `CMD_STAT_CHTM` | `0x25` | Device->Host | 11 bytes (see below) | Channel time/utilization stats |
| `CMD_STAT_PHYPRM` | `0x26` | Device->Host | 12 bytes (see below) | Physical layer parameters |
| `CMD_STAT_BAT` | `0x27` | Device->Host | 2 bytes: [state, percent] | Battery status |
| `CMD_STAT_CSMA` | `0x28` | Device->Host | 3 bytes: [band, min, max] | CSMA contention window params |
| `CMD_STAT_TEMP` | `0x29` | Device->Host | 1 byte (value - 120 = Celsius) | CPU temperature |

#### System Commands

| Command | Byte | Direction | Payload | Description |
|---------|------|-----------|---------|-------------|
| `CMD_BLINK` | `0x30` | Host->Device | (unknown) | Blink LED for identification |
| `CMD_RANDOM` | `0x40` | Device->Host | 1 byte | Hardware random byte |
| `CMD_FB_EXT` | `0x41` | Host->Device | 1 byte: `0x00`=disable, `0x01`=enable | External framebuffer control |
| `CMD_FB_READ` | `0x42` | Both | Host sends `0x01`; Device responds with 512 bytes | Read framebuffer |
| `CMD_FB_WRITE` | `0x43` | Host->Device | [line_byte] + [8 bytes line data] | Write framebuffer line |
| `CMD_BT_CTRL` | `0x46` | Host->Device | (unknown) | Bluetooth control |
| `CMD_PLATFORM` | `0x48` | Both | Host sends `0x00`; Device responds with platform byte | Query/report platform |
| `CMD_MCU` | `0x49` | Both | Host sends `0x00`; Device responds with MCU byte | Query/report MCU type |
| `CMD_FW_VERSION` | `0x50` | Both | Host sends `0x00`; Device responds with 2 bytes [major, minor] | Query/report firmware version |
| `CMD_ROM_READ` | `0x51` | Host->Device | (unknown) | Read ROM data |
| `CMD_RESET` | `0x55` | Both | Host sends `0xF8`; Device sends `0xF8` on reset | Hard reset / reset notification |
| `CMD_DISP_READ` | `0x66` | Both | Host sends `0x01`; Device responds with 1024 bytes | Read display buffer |

#### Multi-Interface Commands (RNodeMultiInterface only)

| Command | Byte | Direction | Payload | Description |
|---------|------|-----------|---------|-------------|
| `CMD_INTERFACES` | `0x71` | Both | Host queries; Device responds with 2 bytes per interface [vport, type] | List available radio interfaces |
| `CMD_SEL_INT` | `0x1F` | Host->Device | 1 byte: interface index | Select subinterface for next command |
| `CMD_INT0_DATA` | `0x00` | Device->Host | Packet data | Data received on interface 0 |
| `CMD_INT1_DATA` | `0x10` | Device->Host | Packet data | Data received on interface 1 |
| `CMD_INT2_DATA` | `0x20` | Device->Host | Packet data | Data received on interface 2 |
| `CMD_INT3_DATA` | `0x70` | Device->Host | Packet data | Data received on interface 3 |
| `CMD_INT4_DATA` | `0x75` | Device->Host | Packet data | Data received on interface 4 |
| `CMD_INT5_DATA` | `0x90` | Device->Host | Packet data | Data received on interface 5 |
| `CMD_INT6_DATA` | `0xA0` | Device->Host | Packet data | Data received on interface 6 |
| `CMD_INT7_DATA` | `0xB0` | Device->Host | Packet data | Data received on interface 7 |
| `CMD_INT8_DATA` | `0xC0` | Device->Host | Packet data | Data received on interface 8 |
| `CMD_INT9_DATA` | `0xD0` | Device->Host | Packet data | Data received on interface 9 |
| `CMD_INT10_DATA` | `0xE0` | Device->Host | Packet data | Data received on interface 10 |
| `CMD_INT11_DATA` | `0xF0` | Device->Host | Packet data | Data received on interface 11 |

**Note:** `CMD_INT8_DATA` (0xC0) collides with `FEND`. This appears to be
an oversight or intentional oddity in the multi-interface protocol. It
means interface 8 data cannot actually be distinguished from a frame
delimiter. In practice, multi-interface devices may not populate all 12
slots.

#### Error Codes (in CMD_ERROR payload)

| Error | Byte | Description |
|-------|------|-------------|
| `ERROR_INITRADIO` | `0x01` | Radio initialization failed |
| `ERROR_TXFAILED` | `0x02` | Transmission failed |
| `ERROR_EEPROM_LOCKED` | `0x03` | EEPROM is locked |
| `ERROR_QUEUE_FULL` | `0x04` | TX queue full (single-interface only) |
| `ERROR_MEMORY_LOW` | `0x05` | Memory exhausted (single-interface only) |
| `ERROR_MODEM_TIMEOUT` | `0x06` | Modem communication timeout (single-interface only) |

#### Platform Constants

| Platform | Byte | Description |
|----------|------|-------------|
| `PLATFORM_AVR` | `0x90` | AVR-based RNode |
| `PLATFORM_ESP32` | `0x80` | ESP32-based RNode |
| `PLATFORM_NRF52` | `0x70` | nRF52-based RNode |

#### Radio Chip Types (Multi-Interface only)

| Chip | Byte | Frequency Range |
|------|------|----------------|
| `SX127X` | `0x00` | Sub-GHz (137 MHz - 1 GHz) |
| `SX1276` | `0x01` | Sub-GHz |
| `SX1278` | `0x02` | Sub-GHz |
| `SX126X` | `0x10` | Sub-GHz |
| `SX1262` | `0x11` | Sub-GHz |
| `SX128X` | `0x20` | 2.4 GHz (2.2 GHz - 2.6 GHz) |
| `SX1280` | `0x21` | 2.4 GHz |

### 1.3 Initialization Sequence

The host performs this exact sequence after opening the serial port:

**Step 1: Open serial port**
```
Baud: 115200
Data bits: 8
Stop bits: 1
Parity: None
Flow control: None (xonxoff=False, rtscts=False, dsrdtr=False)
Timeout: 0 (non-blocking reads)
```

**Step 2: Wait 2.0 seconds**

This is a hard-coded sleep to let the device settle after USB enumeration
or power-on. Critical for reliability.

**Step 3: Start read loop thread**

A background thread begins reading bytes from the serial port and parsing
KISS frames.

**Step 4: Send detect + query commands (single frame sequence)**

```
C0 08 73 C0 50 00 C0 48 00 C0 49 00 C0
```

This decodes as four back-to-back KISS frames:
1. `FEND CMD_DETECT DETECT_REQ(0x73) FEND` -- "Are you an RNode?"
2. `CMD_FW_VERSION 0x00 FEND` -- "What firmware version?"
3. `CMD_PLATFORM 0x00 FEND` -- "What platform?"
4. `CMD_MCU 0x00 FEND` -- "What MCU?"

Note: Frames 2-4 rely on FEND at end of previous frame serving as start
of next frame (KISS allows this).

For RNodeMultiInterface, an additional query is appended:
5. `CMD_INTERFACES 0x00 FEND` -- "List your radio interfaces"

**Step 5: Wait for detect response (200ms for serial, 5s for TCP/BLE)**

The read loop parses incoming bytes. When it sees `CMD_DETECT` with payload
`0x46` (DETECT_RESP), it sets `self.detected = True`. The FW_VERSION,
PLATFORM, and MCU responses are also parsed and stored.

**Step 6: Validate firmware version**

Required minimum: major >= 1, minor >= 52 (for single-interface).
Required minimum: major >= 1, minor >= 74 (for multi-interface).

**Step 7: Configure radio parameters**

Sends these commands in sequence:
1. `CMD_FREQUENCY` with 4-byte big-endian frequency in Hz
2. `CMD_BANDWIDTH` with 4-byte big-endian bandwidth in Hz
3. `CMD_TXPOWER` with 1-byte TX power in dBm
4. `CMD_SF` with 1-byte spreading factor (5-12)
5. `CMD_CR` with 1-byte coding rate (5-8)
6. `CMD_ST_ALOCK` with 2-byte short-term airtime limit (if configured)
7. `CMD_LT_ALOCK` with 2-byte long-term airtime limit (if configured)
8. `CMD_RADIO_STATE` with `0x01` (RADIO_STATE_ON)

For multi-interface: each command is preceded by `CMD_SEL_INT` with the
subinterface index, and configurations are sent per-subinterface.

**Step 8: Validate radio state**

Wait 250ms (serial) / 1.0s (BLE) / 1.5s (TCP), then compare the
device-reported values (`r_frequency`, `r_bandwidth`, etc.) against the
configured values. Frequency must match within 100 Hz.

**Step 9: Mark interface online**

Wait 300ms, then set `self.online = True`.

### 1.4 Data Transfer

#### Outgoing (Host -> Device)

A Reticulum packet is wrapped as:

```
[FEND 0xC0] [CMD_DATA 0x00] [KISS-escaped packet bytes] [FEND 0xC0]
```

For multi-interface, data is sent as:

```
[FEND] [CMD_SEL_INT 0x1F] [interface_index] [FEND] [FEND] [CMD_DATA 0x00] [KISS-escaped packet bytes] [FEND]
```

The packet bytes are the raw Reticulum packet (header + payload), with NO
additional metadata. RSSI/SNR are not included in outgoing packets.

#### Incoming (Device -> Host)

The device sends received packets as:

```
[FEND 0xC0] [CMD_DATA 0x00] [KISS-escaped packet bytes] [FEND 0xC0]
```

For multi-interface, the device uses interface-specific data commands:
```
[FEND 0xC0] [CMD_INTn_DATA] [KISS-escaped packet bytes] [FEND 0xC0]
```
where `CMD_INTn_DATA` indicates which radio interface received the packet.

**Accompanying metadata (separate KISS frames, sent before the data frame):**

The device sends RSSI and SNR as separate KISS frames before or after
the data frame:

- `CMD_STAT_RSSI` (0x23): 1 byte, unsigned. Actual RSSI = value - 157 dBm
- `CMD_STAT_SNR` (0x24): 1 byte, signed. Actual SNR = value * 0.25 dB

These are stored on the interface object and cleared after `process_incoming`
delivers the packet:
```python
self.r_stat_rssi = None
self.r_stat_snr = None
```

#### Periodically reported statistics (unsolicited, from device)

The device periodically sends these frames without host request:

**CMD_STAT_CHTM (0x25) -- Channel Time, 11 bytes:**
```
Bytes 0-1:  airtime_short (BE u16, /100 = percent)
Bytes 2-3:  airtime_long (BE u16, /100 = percent)
Bytes 4-5:  channel_load_short (BE u16, /100 = percent)
Bytes 6-7:  channel_load_long (BE u16, /100 = percent)
Byte  8:    current_rssi (unsigned, -157 offset)
Byte  9:    noise_floor (unsigned, -157 offset)
Byte  10:   interference (unsigned, -157 offset; 0xFF = no interference)
```

Note: For multi-interface (RNodeMultiInterface), CMD_STAT_CHTM is only
8 bytes (no RSSI/noise_floor/interference fields):
```
Bytes 0-1:  airtime_short (BE u16, /100 = percent)
Bytes 2-3:  airtime_long (BE u16, /100 = percent)
Bytes 4-5:  channel_load_short (BE u16, /100 = percent)
Bytes 6-7:  channel_load_long (BE u16, /100 = percent)
```

**CMD_STAT_PHYPRM (0x26) -- Physical Parameters, 12 bytes (single) / 10 bytes (multi):**
```
Bytes 0-1:   symbol_time (BE u16, /1000 = milliseconds)
Bytes 2-3:   symbol_rate (BE u16, baud)
Bytes 4-5:   preamble_symbols (BE u16)
Bytes 6-7:   preamble_time (BE u16, milliseconds)
Bytes 8-9:   csma_slot_time (BE u16, milliseconds)
Bytes 10-11: difs_time (BE u16, milliseconds)  -- ONLY in single-interface
```

**CMD_STAT_CSMA (0x28) -- CSMA Parameters, 3 bytes (single-interface only):**
```
Byte 0: contention_window_band
Byte 1: contention_window_min
Byte 2: contention_window_max
```

**CMD_STAT_BAT (0x27) -- Battery Status, 2 bytes:**
```
Byte 0: battery_state (0x00=unknown, 0x01=discharging, 0x02=charging, 0x03=charged)
Byte 1: battery_percent (0-100, clamped)
```

**CMD_STAT_TEMP (0x29) -- CPU Temperature, 1 byte:**
```
Byte 0: temperature + 120 (actual temp = value - 120 Celsius)
Valid range: -30 to +90 Celsius
```

### 1.5 Flow Control

The RNode protocol implements **software flow control** via the CMD_READY
mechanism:

1. When `flow_control=True` is configured, the host sets `interface_ready = False`
   after sending each packet.
2. The device sends a `CMD_READY` frame when it has finished transmitting
   and is ready for the next packet.
3. Upon receiving `CMD_READY`, the host calls `process_queue()`:
   - If packets are queued, pops the first one and sends it.
   - If no packets are queued, sets `interface_ready = True`.
4. If `interface_ready` is False when `process_outgoing()` is called, the
   packet is appended to `packet_queue` instead of being sent immediately.

When `flow_control=False` (the default), `interface_ready` starts True and
is never set to False by the host. The CMD_READY frames from the device
still trigger `process_queue()`, which is a no-op if the queue is empty.

The packet queue is a simple FIFO list with **no maximum size** and **no
priority**. Overflow is not explicitly handled.

---

## 2. Radio Configuration

### 2.1 Parameters and Encoding

**Frequency (CMD_FREQUENCY, 0x01)**
- 4 bytes, big-endian unsigned integer
- Unit: Hertz
- Example: 868.0 MHz = `0x33B13B40`
- Encoding: `[freq >> 24, (freq >> 16) & 0xFF, (freq >> 8) & 0xFF, freq & 0xFF]`
- KISS-escaped after encoding

**Bandwidth (CMD_BANDWIDTH, 0x02)**
- 4 bytes, big-endian unsigned integer
- Unit: Hertz
- Encoding identical to frequency
- Valid range: 7,800 Hz to 1,625,000 Hz

**TX Power (CMD_TXPOWER, 0x03)**
- 1 byte, unsigned for single-interface (0-37 dBm)
- 1 byte, signed for multi-interface (-9 to +37 dBm). Encoded as
  `txpower.to_bytes(1, signed=True)` on send; decoded as
  `byte - 256 if byte > 127 else byte` on receive.
- Unit: dBm

**Spreading Factor (CMD_SF, 0x04)**
- 1 byte, unsigned
- Valid range: 5-12

**Coding Rate (CMD_CR, 0x05)**
- 1 byte, unsigned
- Valid range: 5-8
- Represents 4/5 through 4/8

**Short-term Airtime Limit (CMD_ST_ALOCK, 0x0B)**
- 2 bytes, big-endian unsigned integer
- Encoding: `int(percent * 100)` -- so 50.0% becomes 5000
- KISS-escaped after encoding

**Long-term Airtime Limit (CMD_LT_ALOCK, 0x0C)**
- Same encoding as ST_ALOCK

**Radio State (CMD_RADIO_STATE, 0x06)**
- 1 byte
- `0x00` = OFF, `0x01` = ON, `0xFF` = ASK (query current state)

### 2.2 Hardware Variants

The Python code identifies devices by platform and MCU:

**Platforms:**
| Platform | Value | Has Display | Notes |
|----------|-------|-------------|-------|
| AVR | `0x90` | No | Original Arduino-based RNode |
| ESP32 | `0x80` | Yes | Most common modern RNode |
| NRF52 | `0x70` | Yes | Nordic-based RNode |

**Radio chips (Multi-Interface only):**
| Chip Family | Frequency Range | Notes |
|-------------|----------------|-------|
| SX127X (SX1276/SX1278) | 137 MHz - 1 GHz | Sub-GHz LoRa |
| SX126X (SX1262) | 137 MHz - 1 GHz | Sub-GHz LoRa, newer |
| SX128X (SX1280) | 2.2 GHz - 2.6 GHz | 2.4 GHz LoRa |

**Frequency validation:**
- Single-interface: 137 MHz to 3 GHz (broad range, device validates further)
- Multi-interface SX127X/SX126X: 137 MHz to 1 GHz
- Multi-interface SX128X: 2.2 GHz to 2.6 GHz

**Hardware MTU:** Fixed at 508 bytes for all RNode variants.

### 2.3 Firmware Detection

The host queries firmware version as part of the detect sequence:

```
FEND CMD_FW_VERSION 0x00 FEND
```

Response is 2 KISS-escaped bytes: `[major, minor]`.

**Required minimum versions:**
- RNodeInterface (single radio): 1.52
- RNodeMultiInterface (multi radio): 1.74

If firmware is below minimum, Python calls `RNS.panic()` with instructions
to update via `rnodeconf`.

**Validation logic:**
```python
if maj_version > REQUIRED_MAJ:
    firmware_ok = True
elif maj_version >= REQUIRED_MAJ and min_version >= REQUIRED_MIN:
    firmware_ok = True
```

### 2.4 Transport Variants: USB, TCP, BLE

The RNode can be accessed over three transport types. The serial protocol
is identical over all three; only the physical transport differs.

**USB Serial (default)**
- Baud: 115200, 8N1
- Uses pyserial `Serial` object directly
- Read timeout: 100ms
- Detect wait: 200ms

**TCP (port specified as `tcp://hostname`)**
- Port: 7633 (TCPConnection.TARGET_PORT)
- Uses raw TCP socket with `TCP_NODELAY`
- Has keepalive mechanism: sends detect command every 3.5s
  (ACTIVITY_KEEPALIVE = ACTIVITY_TIMEOUT - 2.5 = 3.5s)
- Read timeout: 1500ms
- Detect wait: 5.0s
- TCP keepalive probes: every 2s, after 5s idle, 12 probes, 24s user timeout

**BLE (port specified as `ble://` or `ble://name` or `ble://AA:BB:CC:DD:EE:FF`)**
- Uses Nordic UART Service (NUS) over BLE GATT:
  - Service: `6E400001-B5A3-F393-E0A9-E50E24DCCA9E`
  - RX Char: `6E400002-B5A3-F393-E0A9-E50E24DCCA9E` (host writes to device)
  - TX Char: `6E400003-B5A3-F393-E0A9-E50E24DCCA9E` (device notifies host)
- Requires device to be bonded (paired)
- Read timeout: 1250ms
- Detect wait: 5.0s
- Uses `bleak` Python library for BLE access
- Write chunk size limited to `max_write_without_response_size`

---

## 3. Medium Access

### 3.1 CSMA/CA

**CSMA is handled entirely by the RNode firmware, not the host.**

The host has no CSMA logic. It simply sends packets to the device.
The device reports its CSMA parameters back to the host for informational/
monitoring purposes via:

- `CMD_STAT_PHYPRM` (0x26): symbol time, symbol rate, preamble symbols,
  preamble time, CSMA slot time, DIFS time
- `CMD_STAT_CSMA` (0x28): contention window band, min, max

The host stores these values but does not use them for transmission
decisions. All carrier sensing, backoff, and collision avoidance is
performed by the RNode firmware.

### 3.2 Airtime Calculation

**On-air bitrate calculation (host-side, for capacity planning):**

```python
bitrate = sf * (4.0 / cr) / (2**sf / (bandwidth/1000)) * 1000
```

Where:
- `sf` = spreading factor (5-12)
- `cr` = coding rate (5-8, representing 4/5 through 4/8)
- `bandwidth` = channel bandwidth in Hz

This gives the effective data rate in bits per second.

**Channel utilization (device-reported):**

The device periodically sends `CMD_STAT_CHTM` with:
- `r_airtime_short`: Short-term airtime percentage (own TX)
- `r_airtime_long`: Long-term airtime percentage (own TX)
- `r_channel_load_short`: Short-term channel load (all observed activity)
- `r_channel_load_long`: Long-term channel load (all observed activity)

The host does NOT compute channel utilization itself. It relies entirely
on the device's reporting.

**Airtime limiting:**

If configured, the host sends airtime limits to the device:
- `CMD_ST_ALOCK`: Short-term airtime limit percentage
- `CMD_LT_ALOCK`: Long-term airtime limit percentage

The device enforces these limits in firmware.

### 3.3 Timing and Jitter

**The RNode interface applies NO send-side jitter or timing delays.**

Unlike Transport's `PATHFINDER_RW` (0.5s random window for announce
rebroadcasts), the RNode interface has no equivalent jitter mechanism.
All timing is either:

1. **Device-side CSMA** (carrier sensing in firmware)
2. **Transport-layer announce scheduling** (handled by Transport, not the
   interface)
3. **Announce rate cap** (in base Interface class, based on bitrate):
   ```python
   tx_time = (len(packet) * 8) / self.bitrate
   wait_time = tx_time / announce_cap
   ```
   Default `announce_cap` = 2% of interface bandwidth.

The 80ms sleep in the read loop idle path is purely to prevent busy-waiting
when no data is available, not a timing mechanism.

**Callsign beaconing:**

If `id_interval` and `id_callsign` are configured, the interface
periodically transmits the callsign as raw packet data (not a Reticulum
packet). The timer resets on each TX. The first_tx timestamp records
when the first actual (non-callsign) packet was transmitted.

---

## 4. Queue Management

### 4.1 TX Pipeline

```
process_outgoing(data)
    |
    |-- Is interface online?
    |   No -> drop silently
    |
    |-- Is interface_ready?
    |   No -> queue(data) -> append to self.packet_queue
    |   Yes:
    |       |-- If flow_control: set interface_ready = False
    |       |-- KISS-escape the data
    |       |-- Build frame: FEND + CMD_DATA(0x00) + escaped_data + FEND
    |       |-- serial.write(frame)
    |       |-- Increment txb counter
```

**Key observations:**
- `packet_queue` is an unbounded Python list (no max size)
- FIFO ordering, no priority
- `interface_ready` starts as False, set to True only after successful
  device configuration (in configure_device)
- With flow_control=False (default), interface_ready is always True once
  online, so the queue is never used
- With flow_control=True, the queue drains one packet at a time via
  CMD_READY callbacks

### 4.2 RX Pipeline

```
readLoop() [background thread]
    |
    |-- Read 1 byte from serial
    |-- Parse KISS frame state machine:
    |   |-- FEND: start new frame, reset command
    |   |-- First byte after FEND: set as command byte
    |   |-- CMD_DATA (0x00): accumulate into data_buffer with KISS unescaping
    |   |-- CMD_* (config): accumulate into command_buffer, parse when complete
    |   |-- FEND while in CMD_DATA frame: frame complete
    |
    |-- On complete CMD_DATA frame:
    |   process_incoming(data_buffer)
    |       |-- Increment rxb counter
    |       |-- self.owner.inbound(data, self)  [delivers to Transport]
    |       |-- Clear r_stat_rssi and r_stat_snr
    |
    |-- On complete CMD_* frame:
    |   Parse and store in corresponding r_* fields
    |
    |-- Timeout handling:
    |   If partial frame and no data for > self.timeout ms:
    |       Clear buffer, reset state machine
```

**Buffer size limit:** `self.HW_MTU` (508 bytes). If `data_buffer` reaches
this size, additional bytes are silently dropped until the next FEND.

### 4.3 Threading Model

**Single-interface (RNodeInterface):**

```
Main thread:                Read loop thread:
    |                           |
    configure_device() -->  readLoop() [daemon]
    |                           |
    process_outgoing() ----     | <-- serial.read(1)
    setFrequency()    ----     | --> parse KISS frames
    setBandwidth()    ----     | --> update r_* fields
    ...               ----     | --> process_incoming() -> owner.inbound()
                               | --> process_queue() [on CMD_READY]
                               |
                               | [80ms sleep when no data]
```

Both threads access `self.serial` (the pyserial object). There is NO
explicit locking between the write path (main thread) and the read path
(readLoop thread). pyserial's internal buffering provides some safety,
but this is technically a race condition in the Python implementation.

For BLE and TCP transports, separate TX/RX queues with locks are used:
- `ble_rx_lock` / `ble_tx_lock`
- `tcp_rx_lock` / `tcp_tx_lock`

**Multi-interface (RNodeMultiInterface):**

Same model, but the read loop dispatches to the correct sub-interface
based on `CMD_INTn_DATA` command bytes. The `CMD_SEL_INT` command in the
read loop updates `self.selected_index`, which determines which
sub-interface receives configuration confirmations.

---

## 5. Interface Lifecycle

### 5.1 INI Configuration

The `[[RNode Interface]]` section accepts these config keys:

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `name` | string | Yes | -- | Interface name |
| `port` | string | Yes | -- | Serial port path, `tcp://host`, or `ble://...` |
| `frequency` | int | Yes | 0 | Operating frequency in Hz |
| `bandwidth` | int | Yes | 0 | Channel bandwidth in Hz |
| `txpower` | int | No | 22 (board maximum) | TX power in dBm. Absent asks for the board maximum, not the reference's 0 — see the [pinned deviation](concepts/python-rns-compatibility.md). An explicit `0` still means 0. |
| `spreadingfactor` | int | Yes | 0 | Spreading factor (5-12) |
| `codingrate` | int | Yes | 0 | Coding rate (5-8) |
| `flow_control` | bool | No | False | Enable TX flow control |
| `id_interval` | int | No | None | Callsign beacon interval in seconds |
| `id_callsign` | string | No | None | Callsign for beaconing (max 32 bytes UTF-8) |
| `airtime_limit_short` | float | No | None | Short-term TX airtime limit (0-100%) |
| `airtime_limit_long` | float | No | None | Long-term TX airtime limit (0-100%) |

**RNodeMultiInterface adds:**

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `port` | string | Yes | -- | Serial port path |

Each sub-interface is defined as a nested section with:

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `interface_enabled` | bool | No | (inherits parent `enabled`) | Enable this sub-interface |
| `vport` | int | Yes | -- | Virtual port index on device |
| `frequency` | int | Yes | -- | Frequency in Hz |
| `bandwidth` | int | Yes | -- | Bandwidth in Hz |
| `txpower` | int | No | 22 (board maximum) | TX power in dBm, resolved per subinterface the same way as above |
| `spreadingfactor` | int | Yes | -- | Spreading factor |
| `codingrate` | int | Yes | -- | Coding rate |
| `flow_control` | bool | No | False | TX flow control |
| `airtime_limit_short` | float | No | None | Short-term airtime limit |
| `airtime_limit_long` | float | No | None | Long-term airtime limit |
| `outgoing` | bool | No | True | Whether TX is allowed |

### 5.2 Connection Management

**Startup:**
1. Validate configuration parameters
2. Open serial port
3. If open succeeds: configure_device (detect, init radio, validate)
4. If open fails: start reconnect_port thread

**Reconnection:**
- `reconnect_port()` runs in a loop:
  - Sleep 5 seconds (`RECONNECT_WAIT`)
  - Try to open port and configure device
  - Repeat until `online` or `detached`
- The readLoop also triggers reconnection when it catches an exception
  (serial port error, device reset, etc.)
- ESP32 devices send `CMD_RESET 0xF8` when they reset while online,
  which the host treats as an error triggering reconnection.

**Shutdown (detach):**
1. Set `self.detached = True`
2. Disable external framebuffer
3. Set radio state to OFF
4. Send CMD_LEAVE
5. Close BLE/TCP connections if applicable

**Ingress limiting:**

RNodeInterface **overrides** `should_ingress_limit()` to always return
`False`. This means RNode interfaces never throttle incoming announces
at the interface level (Transport still applies its own limiting).

### 5.3 Statistics

The interface tracks and exposes:

**Counters (host-maintained):**
- `rxb`: Total bytes received (incremented in process_incoming)
- `txb`: Total bytes transmitted (incremented in process_outgoing)

**Device-reported:**
- `r_stat_rx`: Total device RX packet count (4-byte)
- `r_stat_tx`: Total device TX packet count (4-byte)
- `r_stat_rssi`: Last packet RSSI in dBm (byte - 157)
- `r_stat_snr`: Last packet SNR in dB (signed_byte * 0.25)
- `r_stat_q`: Signal quality percentage (computed from SNR and SF)
- `r_airtime_short` / `r_airtime_long`: TX airtime percentages
- `r_channel_load_short` / `r_channel_load_long`: Channel load percentages
- `r_battery_state` / `r_battery_percent`: Battery info
- `r_temperature` / `cpu_temp`: CPU temperature in Celsius

**Signal quality calculation:**
```python
q_snr_min = Q_SNR_MIN_BASE - (sf - 7) * Q_SNR_STEP  # where BASE=-9, STEP=2
q_snr_max = Q_SNR_MAX  # 6
q_snr_span = q_snr_max - q_snr_min
quality = clamp(((snr - q_snr_min) / q_snr_span) * 100, 0, 100)
```

**RSSI decoding:**
All RSSI values use the same offset: `actual_dBm = raw_byte - 157`.

---

## 6. Physical Device Info

### Connected Device

```
Device:     /dev/ttyACM0
USB Vendor: 1a86 (QinHeng Electronics)
USB Model:  USB Single Serial (55d4)
USB Serial: 5896004228
Driver:     cdc_acm
Symlinks:   /dev/serial/by-id/usb-1a86_USB_Single_Serial_5896004228-if00
```

### Probe Results

```
Detection:       Successful (DETECT_RESP = 0x46)
Firmware:        1.85
Platform:        ESP32 (0x80)
MCU:             0x81
Battery report:  Received (CMD_STAT_BAT 0x27, state=0x00 unknown, percent=0%)
```

The device also sent an unsolicited `CMD_STAT_BAT` frame during the detect
sequence, which is expected -- the device reports battery status
periodically.

The QinHeng Electronics CH340/CH9102 USB-serial chip (VID 1a86, PID 55d4)
is commonly used on ESP32 development boards, specifically the Heltec and
LilyGO T-Beam variants commonly used for RNode.

---

## 7. Implementation Notes for Rust

### 7.1 What Maps to Our Interface Trait (Send Side)

The outgoing path is straightforward: `process_outgoing(data)` takes a raw
Reticulum packet and wraps it in a KISS frame. This maps to our Interface
trait's send method. The KISS framing (FEND + CMD_DATA + escape + FEND) is
a simple transformation.

The flow control queue (interface_ready / packet_queue) is host-side state
that should live on the interface struct. When flow_control is enabled,
the interface buffers packets until the device signals CMD_READY.

### 7.2 What Needs Its Own Async Task (Receive Side, Serial I/O)

The Python implementation uses a daemon thread for `readLoop()`. In our
async Rust architecture, this maps to an async task that:

1. Reads bytes from the serial port (async serial I/O)
2. Parses the KISS frame state machine
3. Dispatches complete frames:
   - CMD_DATA -> feed to NodeCore via `handle_packet()`
   - CMD_STAT_* -> update interface metadata
   - CMD_READY -> trigger queue drain
   - CMD_ERROR -> handle errors

The serial port read should use `tokio-serial` or similar async serial
crate. The KISS deframer runs in the same task (no separate thread needed).

The read loop is the only path that needs to be truly async. All writes
(config commands, data packets) can be synchronous or fire-and-forget
since there's no write-side acknowledgment protocol.

### 7.3 Where Send-Side Jitter Fits

There is **no send-side jitter in the RNode interface itself**. All
timing is handled by:

1. **RNode firmware**: CSMA/CA with carrier sensing
2. **Transport layer**: Announce rebroadcast random window (`PATHFINDER_RW = 0.5s`)
3. **Interface base class**: Announce rate cap (`announce_cap = 2%`)

The announce rate cap and queue management from the base Interface class
should be implemented in the transport/driver layer, not in the RNode
interface itself. The interface is a dumb pipe -- it takes packets from
the send queue and KISS-frames them to the serial port.

### 7.4 State the Interface Needs to Maintain

**Configuration (set once):**
- frequency, bandwidth, txpower, sf, cr
- st_alock, lt_alock
- flow_control flag
- id_callsign, id_interval
- port path, transport type (USB/TCP/BLE)

**Device-reported (updated from read loop):**
- r_frequency, r_bandwidth, r_txpower, r_sf, r_cr, r_state, r_lock
- r_stat_rssi, r_stat_snr (per-packet, cleared after delivery)
- r_airtime_short, r_airtime_long, r_channel_load_short, r_channel_load_long
- r_symbol_time_ms, r_symbol_rate, r_preamble_symbols, r_preamble_time_ms
- r_csma_slot_time_ms, r_csma_difs_ms
- r_csma_cw_band, r_csma_cw_min, r_csma_cw_max
- r_battery_state, r_battery_percent, r_temperature
- detected, firmware_ok, maj_version, min_version
- platform, mcu, display

**Runtime (host-managed):**
- online flag
- interface_ready flag (for flow control)
- packet_queue (if flow_control enabled)
- rxb, txb counters
- first_tx timestamp (for callsign beaconing)

### 7.5 Relationship to Existing KISS Framing in leviculum-core

**The existing framing code in `leviculum-core/src/framing/hdlc.rs` is
HDLC framing, NOT KISS framing. They are different protocols.**

Key differences:

| Property | HDLC (existing) | KISS (needed for RNode) |
|----------|----------------|------------------------|
| Flag byte | `0x7E` | `0xC0` (FEND) |
| Escape byte | `0x7D` | `0xDB` (FESC) |
| Escape method | XOR with `0x20` | Substitution: `0xDC` (TFEND) or `0xDD` (TFESC) |
| Command byte | None | First byte after FEND is command |
| Used for | TCP interfaces | Serial RNode interface |

**We need a new `framing/kiss.rs` module** alongside the existing `hdlc.rs`.
The module structure should be:

```
framing/
    mod.rs       -- re-exports both
    hdlc.rs      -- existing, for TCP
    kiss.rs      -- new, for RNode serial
```

The KISS module needs:
- Constants: FEND, FESC, TFEND, TFESC
- `fn kiss_escape(data: &[u8]) -> Vec<u8>`
- `fn kiss_frame(cmd: u8, data: &[u8]) -> Vec<u8>`
- `struct KissDeframer` with state machine for parsing incoming bytes
  (tracking command byte, escape state, buffer)

The KissDeframer should yield `(command: u8, data: Vec<u8>)` tuples,
not raw byte buffers like the HDLC Deframer.

### 7.6 Is This Standard KISS or a Superset?

**It is a KISS superset.** Standard KISS TNC protocol (as used in amateur
radio) defines:

| Standard KISS | RNode Extension |
|--------------|-----------------|
| `CMD_DATA` (0x00) | Same |
| `CMD_TXDELAY` (0x01) | Repurposed as `CMD_FREQUENCY` |
| `CMD_P` (0x02) | Repurposed as `CMD_BANDWIDTH` |
| `CMD_SLOTTIME` (0x03) | Repurposed as `CMD_TXPOWER` |
| `CMD_TXTAIL` (0x04) | Repurposed as `CMD_SF` |
| `CMD_FULLDUPLEX` (0x05) | Repurposed as `CMD_CR` |
| `CMD_SETHARDWARE` (0x06) | Repurposed as `CMD_RADIO_STATE` |
| `CMD_RETURN` (0xFF) | Not used by RNode |
| (none) | 0x07-0x0F: RNode-specific config commands |
| (none) | 0x21-0x29: RNode statistics |
| (none) | 0x30-0x55: RNode system commands |
| (none) | 0x66: Display read |
| (none) | 0x71, 0x1F: Multi-interface commands |
| (none) | 0x90: Error reporting |

The framing layer (FEND/FESC/TFEND/TFESC) is identical to standard KISS.
The command bytes 0x01-0x06 overlap with standard KISS but have completely
different semantics (frequency vs. txdelay, etc.).

This means our KISS framing module should implement the framing layer
generically, and the RNode command interpretation should be in a separate
module (e.g., `interfaces/rnode.rs` in leviculum-std).

### 7.7 Architecture Mapping

```
leviculum-core/src/framing/kiss.rs    -- KISS framing (FEND/FESC escaping)
                                         Layer 0, no_std compatible
                                         Pure data transformation, no I/O

leviculum-std/src/interfaces/rnode.rs  -- RNode interface implementation
                                         Owns serial port (async I/O)
                                         KISS command interpretation
                                         Radio configuration state machine
                                         Flow control queue management

leviculum-std/src/driver/              -- Existing driver integrates RNode
                                         interface alongside TCP
```

The KISS framing module belongs in leviculum-core because it's a pure
data transformation (like HDLC). The RNode interface logic belongs in
leviculum-std because it performs I/O (serial port access).

### 7.8 Implementation Priority

For a minimal working RNode interface:

1. KISS framing module (framing/kiss.rs) -- escape/unescape, frame/deframe
2. RNode command parser -- interpret command bytes and payloads
3. Initialization sequence -- detect, query, configure, validate
4. Data path -- TX: KISS-frame packets; RX: deframe and deliver
5. Statistics -- parse RSSI/SNR/channel stats from device
6. Flow control -- CMD_READY queue management
7. Reconnection -- handle disconnect/reconnect
8. Multi-interface support -- CMD_SEL_INT, CMD_INTn_DATA

BLE and TCP transport support for RNode can be deferred; USB serial is
the primary use case.
