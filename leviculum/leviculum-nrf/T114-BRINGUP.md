# T114 LNode Bringup Notes

Status as of 2026-03-30. Hardware: Heltec Mesh Node T114 (nRF52840, 256 KB RAM, 1 MB flash).

## What Works

The T114 runs a Reticulum endpoint node. Confirmed on real hardware:

- **Embassy executor** with RTC1 time driver, async tasks, Timer
- **USB CDC-ACM** composite device (port 0 = debug log, port 1 = transport)
- **Hardware RNG** via `embassy_nrf::rng::Rng::new_blocking(p.RNG)`
- **NodeCore** with `EmbeddedStorage` (heapless collections), identity generation, probe responder
- **Serial task** receives HDLC-framed packets from host, deframes correctly
- **Heartbeat LED** confirms main loop is alive (short flash every 5s)
- **Panic handler** blinks LED rapidly via raw GPIO register writes
- **Management announce** at 15s after boot — probe destination announced, 167-byte packet HDLC-framed and sent over USB transport port
- **Incoming packet processing** — lnsd sends announce via serial, T114 deframes it, channel delivers to main loop, `handle_packet()` processes it
- **Bidirectional announce exchange** — T114's announce reaches lnsd (`received announce dest=<...> iface=serial_0 hops=1`), lnsd's announce reaches T114 (`RX 167 bytes`)

## What Doesn't Work Yet

- **`rnprobe` not tested** — the Rust CLI has no probe command (probing is left to Python `rnprobe`), and Python Reticulum is not installed on the dev machine. Bidirectional announce exchange is confirmed but end-to-end probe/proof round-trip is unverified.

## Constraints Discovered

### 1. UF2 Flash Size Limit (~780 KB UF2 / ~390 KB text)

The VM USB passthrough truncates large UF2 writes. The Adafruit UF2 bootloader resets before the kernel finishes flushing the page cache for files > ~780 KB.

**Symptoms:** LED dark (firmware corrupt), no USB enumeration. Smaller firmware flashes fine.

**Fix:** Use `opt-level = "z"` + `lto = true` + `codegen-units = 1` in `[profile.release]`. Already in `Cargo.toml`.

**Root cause:** `cp` writes to the page cache, `sync` flushes asynchronously. For large files through VM USB passthrough, the flush is too slow and the bootloader's timeout expires.

**Note:** Flashing from the host directly (not through VM) works reliably at the current binary size (~189 KB text, ~378 KB UF2).

### 2. NodeCore Must Be Boxed (Stack Overflow)

**Root cause (2026-03-30):** NodeCore with EmbeddedStorage (~40 KB) in the async frame left only **708 bytes of stack free**. The Ed25519 signing call chain in management announces needs ~15 KB of stack, causing a stack overflow at 15s after boot.

**Previously misdiagnosed** as heap fragmentation. Heap measurement proved 89 KB free with successful `try_reserve(200)`. Stack painting (`0xDEAD_BEEF` canary) measured 708 bytes remaining.

**Fix:** `Box::new(NodeCoreBuilder::new()...build(...))` moves NodeCore to the heap.

| Metric | Before Fix | After Fix |
|---|---|---|
| BSS (async frame) | 145,912 | 105,304 |
| Heap used | 8,860 | 47,636 |
| Heap free | 89,444 | 50,668 |
| Stack free | 708 | 41,164 |
| Mgmt announce at 15s | PANIC | 1 action, 167-byte packet sent |

The previous "BTreeMap pre-allocation workaround" (Constraint #2 in old notes) was unnecessary — the empty `BTreeMap::new()` never allocates. The real issue was always stack overflow.

### 3. Boot Blink Must Use nop Delay

`Timer::after_millis(200).await` as the first await point after NodeCore build causes the LED to stay ON (Timer never returns). Using a nop delay loop works:

```rust
led.set_level(Level::Low);
for _ in 0..12_000_000u32 { cortex_m::asm::nop(); }  // ~200ms at 64MHz
led.set_level(Level::High);
```

Timer works fine inside the main loop (heartbeat uses Timer::after_millis(50)). The issue is specific to the first await after NodeCore construction.

### 4. Flash Script (uf2-runner.sh)

`cargo run --release` invokes `tools/uf2-runner.sh`. Works on both VM and host:

- `udisksctl` fails without polkit agent -> fallback to `sudo mount /dev/sdX /mnt`
- Deploy uses `sudo cp` + `sudo sync` + `sudo umount`
- Script finds UF2 drive by searching for `INFO_UF2.TXT` in `/media/$USER`, `/run/media/$USER`, `/mnt`

### 5. VM USB Passthrough

The VM passthrough matches devices by VID:PID. Our firmware uses `1209:0001` (leviculum). The Heltec stock firmware uses `239a:8071`. The bootloader uses `239a:0071`. All three must be in the VM passthrough rules.

Device enumeration after flashing can take 10-15s through the VM. The `uf2-runner.sh` boot verification timeout (10s) often misses it.

## Device Map (This Machine)

| Port | Device |
|------|--------|
| `/dev/ttyACM0` or `/dev/ttyACM2` | leviculum T114 — debug (VID 1209, iface 00) |
| `/dev/ttyACM1` or `/dev/ttyACM3` | leviculum T114 — transport (VID 1209, iface 02) |
| Other ACMs with VID 239a | Heltec T114 #2 (stock firmware) |
| Other ACMs with VID 1a86 | LilyGO RNode ESP32 (x2) |

Port numbers shift depending on enumeration order. Always identify by VID + interface number:
```bash
for dev in /dev/ttyACM*; do
    vid=$(udevadm info -q property "$dev" | grep '^ID_VENDOR_ID=' | cut -d= -f2)
    iface=$(udevadm info -q property "$dev" | grep '^ID_USB_INTERFACE_NUM=' | cut -d= -f2)
    echo "$dev  VID=$vid iface=$iface"
done
```

## What To Try Next

### Priority 1: Test rnprobe Round-Trip

Bidirectional announce exchange works. The next step is proving the full probe/proof round-trip: install Python Reticulum and use `rnprobe` against the firmware's probe responder.

### Priority 2: Investigate Boot Blink Timer Issue

The nop delay workaround (Constraint #3) works but is fragile. Understanding why `Timer::after_millis(200).await` hangs as the first await after NodeCore construction would be valuable. Possibly related to the Embassy time driver not being fully initialized at that point, or the boxed NodeCore's construction order.

## lnsd Test Config

```ini
[reticulum]
  enable_transport = no
  share_instance = yes
  respond_to_probes = yes

[interfaces]
  [[Serial T114]]
    type = SerialInterface
    enabled = yes
    port = /dev/ttyACM1
    speed = 115200
    databits = 8
    parity = N
    stopbits = 1
```

Run: `cargo run -p leviculum-cli --bin lnsd -- -c /tmp/t114-test -s /tmp/t114-test/storage -v`

## Flash Procedure

```bash
# 1. Double-tap reset on T114 to enter DFU mode
# 2. From leviculum-nrf/:
cargo run --release
# This builds, converts to UF2, finds the bootloader drive, and flashes.

# Manual flash (if cargo run times out):
sudo mount /dev/sdX /mnt        # X = whatever the bootloader enumerated as
sudo cp target/thumbv7em-none-eabihf/release/t114.uf2 /mnt/NEW.UF2
sudo sync
sudo umount /mnt
```

## Binary Size Budget

| Component | Size |
|-----------|------|
| Text (code + rodata) | 189 KB |
| Data (initialized) | 24 bytes |
| BSS (zero-init, includes 96 KB heap + async frame + heapless collections) | 105 KB |
| Stack (RAM_END - BSS_END) | ~132 KB |
| Heap used at steady state | ~48 KB of 96 KB |
| Stack free at steady state | ~41 KB (peak during Ed25519 signing: ~25 KB free) |
| **Total RAM** | **232 KB / 232 KB available** |
| **Total Flash** | **189 KB / 796 KB available** |

## Runtime Memory Diagnostics

The firmware includes runtime memory monitoring functions:
- `heap_stats()` — returns `(used, free)` from LlffHeap
- `paint_stack()` — paints stack with `0xDEAD_BEEF` canary at boot
- `stack_free()` — counts untouched canary words from BSS end upward

Boot log includes: `heap u=47636 f=50668 stack f=41164`
