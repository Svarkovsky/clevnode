<sub>*I'm d̶o̶n̶e̶ not even playing the game by rules I can't win at.*</sub> 

<br>
<br>

<p align="center">
  <strong>High-performance standalone Reticulum node & NomadNet server for embedded Linux devices, routers, SBCs, and servers</strong>
</p>

---

<p align="center">
  <a href="#english"><img src="https://img.shields.io/badge/English-available-blue?style=for-the-badge" alt="English"></a>
  <a href="#ukrainian"><img src="https://img.shields.io/badge/Українська-доступно-yellow?style=for-the-badge" alt="Українська"></a>
  <a href="#russian"><img src="https://img.shields.io/badge/Русский-доступно-red?style=for-the-badge" alt="Русский"></a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-AGPL--3.0--or--later-green?style=flat-square" alt="License">
  <img src="https://img.shields.io/badge/platform-MIPS%20|%20ARM%20|%20x86-orange?style=flat-square" alt="Platform">
  <img src="https://img.shields.io/badge/RAM-3.7_MB_start_/_18.5_--_22.0_MB_load-brightgreen?style=flat-square" alt="RAM">
  <img src="https://img.shields.io/badge/binary-2.43_MB-blue?style=flat-square" alt="Binary">
  <img src="https://img.shields.io/badge/uptime-50+_hours_continuous-brightgreen?style=flat-square" alt="Uptime">
</p>

---

<a id="english"></a>

`clevnode` is an optimized standalone hybrid monolith (pure C + static Rust library) designed for continuous 24/7 operation as a gateway and transit router of the Reticulum network, as well as a lightweight NomadNet (Micron) page server on resource-constrained embedded routers, single-board computers (SBCs), and Linux servers (MIPS, MIPSEL, ARMv7, ARM64, x86_64, i686).

<table align="center">
  <tr>
    <td valign="middle">
      <img src="https://github.com/user-attachments/assets/68f675a5-60c2-457f-89ec-b89c8f65ff65" alt="clevnode_logo" width="118" height="118">
    </td>
    <td valign="middle">
      <h1>- clevnode -</h1>
    </td>
  </tr>
</table>

**Etymology:** **C-Lev-Node** = **C** (high-performance C wrapper) + **Lev**iculum (network stack) + **Node** (standalone node). Additional wordplay: **Clever Node**.

#### Supported Features in the Current Codebase:
- **Full Reticulum Stack:** Packet routing, link establishment (Links), announces, and path building.
- **Asynchronous Rust Core (`leviculum`):** Cross-compiled with static `musl libc`, featuring dedicated low-level optimizations for 32-bit embedded architectures without an FPU (`-msoft-float`), as well as full native support for modern ARM and x86 systems.
- **C Monolith (`clevnode.c`):** Asynchronous worker, task queue for request processing, and pre-caching of MsgPack pages in RAM.
- **Traffic Transports:** Operates as a transit node and gateway via the persistent I2P SAM Bridge (port 7656).
- **NomadNet / Micron:** Serves lightweight pages (requests for `/page/index.mu`) with automatic fallback to multi-part Resource transfers when the page size exceeds the packet MTU.

---

### 1. Origin, Authorship, and License

The project is distributed under the terms of the **AGPL-3.0-or-later** license (GNU Affero General Public License v3 or any later version).

* **Base parent project:** [Leviculum](https://codeberg.org/Lew_Palm/leviculum)
* **Original core author:** Lew Palm
* **Source commit of the parent repository:** `808908b12b802e76e08b4a9067137061087a0d05` (Sept 10, 2026)
* **Author of the architectural adaptation and C monolith:** Ivan Svarkovsky ([ivansvarkovsky@gmail.com](mailto:ivansvarkovsky@gmail.com))

> **Note on source code compatibility:**
> Significant architectural changes, optimizations, and patches have been introduced into the original `leviculum` codebase to make it run on 32-bit embedded MIPS processors without an FPU. As a result, **the core source code is no longer backward compatible with the current master branch of the Leviculum repository**. The modified Rust core sources are shipped directly as part of this repository (the `leviculum/` directory) together with the C wrapper `clevnode.c` and build scripts.
>
> **Latest Updates in Source Code:**  
> The most recent, cutting-edge fixes, memory optimizations, and stability patches are maintained directly in the repository source tree (`main` branch). Pre-compiled binaries in GitHub Releases represent stable milestones and may lag behind the continuous development branch. For the freshest improvements and targeted hardware tuning, compiling directly from source via `./build_mips_be.sh` is recommended.

---

### 2. Real Resource Consumption Figures & Long-Term Telemetry

Note: System metrics in the tables below were measured directly on real MIPS hardware: a physical **ASUS RT-AC57U V3** router (Qualcomm Atheros QCA9563 SoC, 32-bit MIPS 74Kc 775 MHz, 128 MB RAM, OpenWrt ath79, Linux kernel 3.4.103-rt119) operating 24/7 under sustained multi-peer Reticulum, clearnet, and I2P traffic with 5,000 active network identities and 18,000+ deduplication hashes.

#### Long-Term Stability Telemetry (Continuous Uptime):
* **Process Uptime:** 198,000 seconds (over 55.0 hours of uninterrupted execution).
* **System Uptime:** 200,000 seconds (~55.6 hours).
* **Average CPU Utilization:** **~22.83%** under regular mesh packet forwarding, background I2P SAM sessions, and live NomadNet resource requests. Zero CPU lockups or runaway threads.
* **Reliability:** Confirmed absence of memory leaks, zero Out-of-Memory (OOM) kills, zero `SIGBUS` exceptions, and zero Rust runtime panics.

| Metric | Measured Value | Description |
|:---|:---|:---|
| **Exact Executable Size** | **~2.43&nbsp;MB** | Static binary with `musl libc` (2,432,596 bytes / 2.32 MiB) |
| **VmRSS (Cold Start)** | **3.7&nbsp;MB** | Measured right after network initialization |
| **VmRSS (Under Continuous 24/7 Load)** | **18.5&nbsp;&#8209;&nbsp;22.0&nbsp;MB** | Stabilized resident memory under 20-30 peers and 5,000 destinations |
| **VmSize / VSZ (Virtual Memory)** | **19.8&nbsp;&#8209;&nbsp;25.0&nbsp;MB** | Total virtual memory address space (reduced from 33.2 MB) |
| **VmPeak (Peak Memory Spike)** | **27.5&nbsp;&#8209;&nbsp;30.0&nbsp;MB** | Peak memory during periodic 15-min database flushes (reduced from 47.8 MB) |
| **VmSwap (Swap File Usage)** | **0&nbsp;&#8209;&nbsp;3.5&nbsp;MB** | Minimal paging under continuous multi-peer mesh routing |

#### Operating System Thread Distribution:
The system maintains **3 active threads** (plus transient background DNS resolver tasks):
1. **Main thread (C):** Runtime environment initialization, interface registry, and the non-blocking Reticulum event dispatch loop (`Event Loop`).
2. **Worker thread (C, `pthread`):** Asynchronous task queue, lock-safe assembly, and serialization of NomadNet page requests (stack bounded to 128 KB).
3. **Unified Tokio runtime worker thread (Rust core, `reticulum-node`):** Single cooperatively scheduled worker for all network I/O, timers, announce propagation, persistent I2P SAM tunnels, and FFI event bridging (stack bounded to 128 KB).

#### Live Global Network Topology Map:
Real-time geographic visualization of mesh nodes and links discovered by this router is available at:
* **Interactive Live Map:** [https://reticulum-topology-map.ivansvarkovsky.workers.dev/](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)

> **Note on Architecture & Scope:**  
> The interactive web map demonstrates live telemetry gathered by the running node. The external telemetry parser script and Cloudflare Worker infrastructure are custom, individual tools and are **not distributed or included as part of this core repository**.

---

### 3. Minimal Deployment Package & Storage Footprint

Running `clevnode` permanently and autonomously on a home router requires only **3 elements**:

1. **`clevnode` binary:** The statically linked executable.
2. **`.reticulum` directory:** Located in the same directory as the executable, containing `config`.
3. **Init startup script:** (e.g. `/opt/etc/init.d/S90clevnode` under Entware) for automatic background launch on router boot.

> **Storage Requirement:**  
> During prolonged operation (months of mesh uptime, route caching, and dedup lists), the node and its runtime state require **at least 15 MB of free storage space** on the USB drive or router flash memory.

> **Directory Naming on Deployment:**  
> The build script `build_mips_be.sh` produces a default configuration in `clevnode/reticulum/config` for local repository convenience. When copying files to the router (e.g. into `/tmp/mnt/sda1/home/lblogd/`), this directory must be renamed to **`.reticulum`** (with a leading dot). This matches the binary's runtime lookup and keeps the working directory clean.

---

### 4. Architectural Concept

```text
┌────────────────────────────────────────────────────────┐
│               C Wrapper (clevnode.c)                   │
│  - Process isolation & setsid() system call            │
│  - Mutex-guarded task queue (pthread worker)           │
│  - Pre-caching of index.mu in RAM (MsgPack binary)     │
│  - Request-to-Resource association mapping             │
└───────────────────────────┬────────────────────────────┘
                            │ FFI Boundary (leviculum.h)
┌───────────────────────────▼────────────────────────────┐
│              Rust Core (libleviculum.a)                │
│  - Full Reticulum network stack (Packet, Link, Route)  │
│  - Tokio asynchronous runtime (-Zbuild-std)            │
│  - Cryptography: Curve25519, Ed25519, AES-128          │
│  - Static linking against musl libc (+soft-float)      │
└────────────────────────────────────────────────────────┘
```

---

### 5. Key System Optimizations

1. **Removal of Bluetooth (BLE) and D-Bus:**  
   `bluer` and `dbus` dependencies are disabled in `Cargo.toml`. The `ble` module in `interfaces/mod.rs` is excluded using `#[cfg(any())]`. Saves ~6.6 MB RAM and eliminates three idle background threads.
2. **Exclusion of Serial, RNode, and KISS:**  
   `tokio-serial` is disabled in `Cargo.toml`. A lightweight mock module `tokio_serial.rs` satisfies type references in the interface driver.
3. **RAM Log Buffer Capping:**  
   A ring buffer capped at **1000 lines** per active descriptor was implemented in `event_log.rs`. When full, older lines are evicted via `buf.remove(0)`.
4. **Modular Feature Flags in Cargo:**  
   `clap`, `serde-pickle`, and `toml` are set to optional. Configured flags: `cli`, `rpc`, and `toml-config` (disabled by default; only compression remains active). The INI parser remains fully intact.
5. **Safe RPC Dummy Stub (`remote_mgmt_dummy.rs`):**  
   The original `remote_mgmt.rs` is gated behind `#[cfg(feature = "rpc")]`. The fallback `remote_mgmt_dummy.rs` returns `None` for remote management queries without triggering panics.
6. **Zero-Overhead Traffic Counters (`CounterAtomic`):**  
   Atomic hardware bus locks on 32-bit MIPS are avoided by replacing interface byte counters with no-op dummy structures when RPC is disabled.  
   *Implementation detail:* Only physical interface breakdown counters (`lev_interface_stats_t`) are zeroed. Global node-wide transport counters (`lev_transport_stats`) and link metrics (`lev_link_stats`) remain active inside the Rust core.
7. **Process Daemonization & Signal Tracking:**  
   The binary issues `setsid()` at startup, detaching from the controlling terminal. An advanced `sigaction` handler records incoming signals alongside the sender's process ID (PID). `SIGPIPE` is ignored to guard against dropped peer connections.
8. **Asynchronous Background DNS Resolution & Unconditional Boot:**  
   - *Problem:* Synchronous DNS resolution (`to_socket_addrs()`) during interface startup (`build_client` / `driver/mod.rs`) caused the daemon to abort with `configuration error (-6)` if any remote peer (e.g. `rns.wdgwars.pl` or DDNS hosts) failed to resolve at boot or if WAN networking was not yet ready.  
   - *Solution:* Hostname resolution was removed from the synchronous startup path and moved inside the background asynchronous reconnect loop (`tcp_client_reconnect_loop` using `tokio::net::lookup_host`).  
   - *Benefits:*  
     - **Unconditional startup:** The node always boots reliably, bringing up local Wi-Fi, I2P, and working TCP interfaces even with no internet access.  
     - **Dynamic background reconnect:** Unresolved interfaces retry DNS queries every 5 seconds and connect automatically as soon as uplink connectivity is restored.  
     - **Dynamic IP tracking (DDNS):** Hostnames are re-resolved before each connection attempt, automatically adapting to changing peer IP addresses without process restarts.
9. **Structural Buffer & Queue Optimization (Memory Capping):**  
   - **Dedup hashlist cap (`FILE_STORAGE_PACKET_HASH_CAP`):** Reduced from 100,000 to 25,000 entries (saves ~3.5 MB RAM; retains enough history for 2-3 hours of transit traffic, fully preventing routing loops).  
   - **FFI event bridge queues (`node.rs`):** Capacities reduced from 512/256 to `DEFAULT_CONTROL_CAP = 64` and `DEFAULT_DATA_CAP = 32`. The C event loop drains events every 500 ms, keeping queues virtually empty.  
   - **Pathfinder lifespan & tags (`constants.rs`):** Route lifetime (`PATHFINDER_EXPIRY_SECS`) reduced from 7 days to 2 days, and path request tags (`MAX_PATH_REQUEST_TAGS`) from 32,000 to 8,000. Stale paths are evicted faster, while Reticulum reactive discovery re-queries active routes on demand.  
   - **Interface announce queues (`MAX_QUEUED_ANNOUNCES_PER_INTERFACE`):** Reduced from 16,384 to 512. Prevents megabytes of RAM from buffering delayed announces on throttled interfaces.
10. **Storage Lifecycle & Database Retention Optimizations (`known_destinations` & `ratchets`):**  
    - **Elimination of perpetual timestamp refresh:** Fixed an issue in `storage.rs` where the 15-minute flush interval (`flush_interval = 900`) unconditionally updated the timestamps of all known destinations to `now` (`e.timestamp = timestamp`), preventing entries from ever expiring and ballooning the database to 17,368 entries (2.24 MB).  
    - **True event timestamps:** Timestamps now update only when an actual announce arrives (`set_identity`), mirroring the reference Python Reticulum design (`Identity.remember`).  
    - **Elimination of disk recombine on flush:** Removed legacy on-disk recombination (`kd_store.load_all()`) in `flush_off_lock`. The memory snapshot is written atomically directly to disk without reviving expired entries, matching modern Python Reticulum behavior where disk recombining is deprecated.  
    - **Automatic retention & capacity caps:** Enforced `MAX_KNOWN_DESTINATIONS = 5,000` and `DEFAULT_IDENTITY_CAP = 5,000` both in memory and during flush snapshotting. Destinations inactive for > 30 days are pruned at flush and boot.  
    - **Ratchet memory protection:** Startup ratchet loading is capped to the 1,000 most recent ratchets (`MAX_LOADED_RATCHETS = 1,000`), and expired files (> 30 days) are pruned from disk on boot, preventing thousands of files from exhausting RAM.
11. **Dynamic Multi-Page Serving & Zero-Downtime Hot Reloading (NomadNet):**  
    - **On-demand `mtime` cache:** Page content is no longer frozen in memory at boot. An LRU cache (16 slots) checks file modification time (`stat()`) on every request. If a `.mu` file was edited or uploaded via SCP, it is reloaded and packaged into MessagePack on the fly without restarting the daemon.  
    - **Arbitrary page naming & microblog support:** The C wrapper dynamically maps any requested subpath under `/page/` (e.g. `/page/blog-post.mu`, `/page/about.mu`) to the corresponding file in `posts/`, with strict path-traversal sanitization.  
    - **Automatic background registration:** The event loop scans `posts/` every 30 seconds, automatically registering new `.mu` files with the Reticulum core without network interruption.
12. **Unified Tokio Runtime & Single-Threaded Core Scheduling:**  
    - **Elimination of duplicated Tokio runtime:** Consolidated the FFI event bridge and driver network I/O into a single shared Tokio runtime instance (`reticulum-node`). Eliminated the redundant second runtime (`tokio-runtime-w`), dropping an idle OS worker thread, a duplicate epoll instance, and redundant timer wheels.  
    - **Gated traffic counter thread:** Gated `spawn_traffic_counter` behind `#[cfg(feature = "rpc")]`, removing the 1-second waking loop and dedicated OS thread when RPC is disabled.  
    - **Clamped thread stacks:** Bounded the `reticulum-node` worker thread stack to 128 KB via `.thread_stack_size(128 * 1024)`. Overall system threads dropped to 3, virtual memory address space (`VmSize`) dropped to 19.8 - 25.0 MB, and peak memory spike (`VmPeak`) dropped from 47.8 MB to ~27.5 MB (saving over 20 MB of peak RAM).
13. **MIPS Hardware Acceleration & Zero-Overhead Packet Deduplication:**  
    - **Curve25519 unrolled loops (`opt-level = 3`):** Configured package-level release profile overrides for `curve25519-dalek`, `ed25519-dalek`, and `x25519-dalek`. LLVM unrolls 32-bit field multiplications, boosting link handshakes and packet verification by 30-40% on MIPS without FPU.  
    - **Direct 8-byte digest hasher (`FastHashBuilder`):** Replaced standard SipHash-1-3 in `packet_cache` with a transparent hasher that takes the first 8 bytes of the SHA-256 digest in a single instruction. Cuts transit packet deduplication overhead from ~250 CPU cycles to zero.  
    - **Dead-weight dependency elimination:** Removed regex engine dependencies (`regex-automata`, `regex-syntax`) and optionalized `serde_json`, shrinking the binary by over 114 KB.

---

### 6. Hardware Support & Architectures

Official standalone static release archives are compiled and verified via QEMU for **6 hardware architectures**:

* **MIPS Big-Endian (`mips-unknown-linux-musl`):** Qualcomm Atheros AR9xxx, QCA95xx, QCA55xx (OpenWrt `ath79`).
* **MIPSEL Little-Endian (`mipsel-unknown-linux-musl`):** MediaTek / Ralink MT7620, MT7621, MT7628 (OpenWrt/Keenetic `ramips`).
* **ARMv7-A 32-bit (`armv7-unknown-linux-musleabihf`):** Cortex-A7/A9, Raspberry Pi 2 / Zero 2W, Orange Pi (hard-float).
* **ARM64 / AArch64 (`aarch64-unknown-linux-musl`):** Cortex-A53/A72, Raspberry Pi 3/4/5, modern ARM routers and VPS.
* **x86_64 (`x86_64-unknown-linux-musl`):** 64-bit Intel / AMD servers, PC, and VPS.
* **i686 (`i686-unknown-linux-musl`):** 32-bit x86 legacy hardware and thin clients.
* **Hardware FPU:** Not required on MIPS/MIPSEL; soft-float math is statically linked (`-msoft-float`).
* **Kernel Compatibility:** Compiled with `musl libc`, which requires a minimum of **Linux 2.6.39**. While 2.6.39 has not been validated on physical hardware, it is expected to function thanks to the Linux syscall ABI stability. **Fully verified on Linux kernel 3.4.103-rt119**. Modern kernels (4.x, 5.x, 6.x) are supported natively.

> **Automated Multi-Architecture Releases:**  
> Standalone static release archives with pre-release QEMU execution verification are automatically compiled and published for all 6 target architectures in GitHub Releases.

---

### 7. Cryptographic Identity & Ecosystem Architecture

The node uses three critical 64-byte key files:

1. **`identities/lblogd`** (64 bytes): Permanent keypair for the NomadNet site.
2. **`identities/site.id`** (64 bytes): Secondary deployment identity reference.
3. **`.reticulum/storage/transport_identity`** (64 bytes): Master transport identity for the Reticulum router. **Must never be deleted.**

#### Base32 Key Structure (Masked Example):
```text
WDHJGSJFCZ3E72YE6L7LQKDKW46BHA6Q56Q*************************************************RQTWUQ=
```

#### Linked Personal Node Ecosystem:
* **Identity Hash:** `c1cfe6b1620ff4515f48********fa8c`
* **NomadNet Router URL:** `nomad://eeccf2849599909527d4********141`
* **Personal Messaging Address (LXMF):** `lxmf:bff56bcc8ec6e1b49759********16b0`

As long as the same `lblogd` key file is preserved, your NomadNet site and your LXMF messaging address remain permanent across all devices.

---

### 8. Network Configuration (`.reticulum/config`)

```ini
[reticulum]
# Node name broadcast across the network.
# If omitted, defaults to "Clevnode" (capitalized).
node_name = Router Node

# Enable forwarding of packets for other mesh nodes.
enable_transport = yes

# Expose local IPC socket for utilities (rnstatus, rnpath).
share_instance = yes

# TCP port for local IPC instances.
shared_instance_port = 37428

# Interval for writing state to disk in seconds (15 minutes).
flush_interval = 900

[logging]
# Static logging level (1 = LEV_LOG_INFO). Supported by parser.
loglevel = 1

[interfaces]
  # Local Wi-Fi / Ethernet server interface for client connections.
  [[Local Wi-Fi Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  # Outgoing TCP link to community clearinghouse hub.
  [[WDGWars Node]]
    type = TCPClientInterface
    enabled = yes
    target_host = rns.wdgwars.pl
    target_port = 4242
    bootstrap_only = yes
    name = WDGWars Node

  # I2P bridge interface (OPTIONAL - comment out if not using I2P).
  [[I2P Bridge Interface]]
    type = I2PInterface
    enabled = yes
    connectable = yes
    discoverable = yes
    discovery_name = Router Node I2P
    reconnect_wait = 120
    peers = wnbroeyo5kotdkq4qpitosoch4f64fiwv4uyrk7tp57w2ubthatq.b32.i2p,
            fc22dfljkpt2beq3itqjkz2k7oizuythif7aa3m73okq7mqrg66q.b32.i2p
```

---

### 9. I2P Integration Mechanics

#### Key Generation and `i2p_iface_0.i2p`:
Reticulum communicates with I2P via SAM API (port `7656`). On startup, it checks `.reticulum/storage/i2p/i2p_iface_0.i2p`. If absent, Reticulum generates a new private destination key (`DEST GENERATE`), writes it to `i2p_iface_0.i2p`, and computes its persistent `.b32.i2p` address.

#### Clearnet <-> I2P Bridging:
With `enable_transport = yes`, the router bridges packets between regular TCP networks and anonymous I2P tunnels. To prevent cross-network routing:
* Set `enable_transport = no` (endpoint-only mode), or
* Add `mode = boundary` to interface definitions to restrict announce forwarding.

#### Public Bootstrap Peers:
The `peers` listed in the config are public community beacons. Sharing them is safe and encouraged; however, do not share private `.b32.i2p` addresses if your service is meant to stay unlisted.

---

### 10. Network Warm-Up & Convergence

Reticulum builds path tables reactively via cryptographically signed announces. On a fresh startup, route discovery takes **5–15 minutes**. Allow the node to warm up before expecting immediate responses from distant destinations.

---

### 11. Build Instructions (`build_mips_be.sh`)

```bash
# Install prerequisites on Ubuntu/Debian x86_64 host
sudo apt update && sudo apt install -y git curl wget gcc make tar xz-utils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly

# Standard incremental build (~2-3 seconds)
./build_mips_be.sh

# Build with extreme binary stripping
./build_mips_be.sh --sstrip

# Complete clean build
./build_mips_be.sh --clean
```

---

### 12. Router Deployment

```bash
# Transfer binary and config (masked example)
scp -P 63*** clevnode/clevnode admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/
scp -P 63*** clevnode/reticulum/config admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/.reticulum/config
```

Sample init script `/opt/etc/init.d/S90clevnode`:
```sh
#!/bin/sh
ENABLED=yes
PROG="clevnode"
DIR="/tmp/mnt/sda1/home/lblogd"
BIN="$DIR/$PROG"
LOG="$DIR/clevnode.log"

start() {
    [ "$ENABLED" != "yes" ] && return 0
    pidof "$PROG" >/dev/null && return 0
    echo -n "Starting $PROG... "
    cd "$DIR" || exit 1
    "$BIN" > "$LOG" 2>&1 &
    sleep 1
    pidof "$PROG" >/dev/null && echo "OK" || echo "FAILED"
}

stop() {
    echo -n "Stopping $PROG... "
    killall "$PROG" 2>/dev/null
    for i in 1 2; do
        sleep 1
        ! pidof "$PROG" >/dev/null && echo "OK" && return 0
    done
    killall -9 "$PROG" 2>/dev/null
    echo "Force killed"
}

case "$1" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  pidof "$PROG" >/dev/null && echo "Running" || echo "Stopped" ;;
    *)       echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
```

---

### 13. Storage Maintenance (`.reticulum/storage`)

* **`transport_identity`:** Router private key. **Do not delete.**
* **`packet_hashlist`:** Automatic ring cache (~3.6 MB cap). Self-managing.
* **`known_destinations`:** Route and identity directory. Can be safely deleted if disk space is low:
  ```sh
  /opt/etc/init.d/S90clevnode stop
  rm -f /tmp/mnt/sda1/home/lblogd/.reticulum/storage/known_destinations
  /opt/etc/init.d/S90clevnode start
  ```

---

### 14. Disclaimer

All paths, names, and network addresses are illustrative. Systems vary depending on vendor firmware and storage setups. Code and configurations are provided as-is without warranty.

---

### 15. Useful Resources

* [Live Reticulum Topology Map (Ivan Svarkovsky)](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)
* [Official Reticulum Interfaces Manual](https://reticulum.network/manual/interfaces.html) [1.1.2]
* [Reticulum Implementations Comparison](https://reticulum.miraheze.org/wiki/Implementations)
* [Awesome Reticulum Directory](https://reticulum.miraheze.org/wiki/Awesome_Reticulum)
* [Reticulum Network on Dead.md](https://dead.md/docs/ham/reticulum-network) [1.4.3]
* [Reticulum via MeshChat & Yggdrasil Guide](https://devzone.org.ua/post/reticulum-vstanovlennia-na-prykladi-meshchat-z-pidkliuchenniam-cherez-yggdrasil) [1.1.2]
* [I2P Router Setup with Yggdrasil Guide](https://devzone.org.ua/post/vstanovlennia-routera-i2p-z-pidkliuchenniam-cherez-yggdrasil) [1.2.1]
* [Official Reticulum GitHub Repository](https://github.com/markqvist/reticulum)
* [Leviculum Rust Core on Codeberg](https://codeberg.org/Lew_Palm/Leviculum)
* [LXMF Protocol Specification](https://github.com/markqvist/LXMF)
* [Sideband LXMF Client](https://github.com/markqvist/sideband)
* [Reticulum Articles on DevZone](https://devzone.org.ua/topic/tag/reticulum)

---

<a id="ukrainian"></a>

### Високопродуктивний автономний вузол Reticulum та сервер NomadNet для вбудованих пристроїв, роутерів та серверів

`clevnode` - це оптимізований автономний гібридний моноліт (чистий C + статична бібліотека Rust), розроблений для цілодобової роботи як шлюз і транзитний маршрутизатор мережі Reticulum, а також легковаговий сервер сторінок NomadNet (Micron) на вбудованих роутерах, одноплатних комп'ютерах (SBC) та серверах з обмеженими ресурсами (MIPS, MIPSEL, ARMv7, ARM64, x86_64, i686).

**Етимологія:** **C-Lev-Node** = **C** (швидка C-оболонка) + **Lev**iculum (мережеве ядро) + **Node** (автономний вузол). Гра слів: **Clever Node** («Розумний вузол»).

#### Підтримувані можливості в поточній кодовій базі:
- **Повноцінний стек Reticulum:** маршрутизація пакетів, встановлення з'єднань (Links), анонси та побудова шляхів.
- **Асинхронне Rust-ядро (`leviculum`):** статично скомпільоване з `musl libc`, із глибокими низькорівневими оптимізаціями для 32-бітних архітектур без FPU (`-msoft-float`) та повною підтримкою сучасних платформ ARM та x86.
- **C-моноліт (`clevnode.c`):** асинхронний воркер, черга задач для обробки записів, прекешування MsgPack-сторінок в ОЗУ.
- **Носії трафіку (Транспорт):** робота як транзитного вузла та шлюзу через I2P SAM Bridge (порт 7656).
- **NomadNet / Micron:** обслуговування легковагових сторінок (запити `/page/index.mu`) з автоматичною передачею великих сторінок у вигляді складених ресурсів (Resources) при перевищенні MTU.

---

### 1. Походження, авторство та ліцензія

Проєкт поширюється на умовах ліцензії **AGPL-3.0-or-later** (GNU Affero General Public License v3 або будь-якої пізнішої версії).

* **Базовий батьківський проєкт:** [Leviculum](https://codeberg.org/Lew_Palm/leviculum)
* **Автор оригінального ядра:** Lew Palm
* **Вихідний коміт батьківського репозиторію:** `808908b12b802e76e08b4a9067137061087a0d05`
* **Автор архітектурної адаптації та C-моноліту:** Іван Сварковський ([ivansvarkovsky@gmail.com](mailto:ivansvarkovsky@gmail.com))

> **Зауваження щодо сумісності вихідного коду:**  
> До оригінальної кодової бази `leviculum` було внесено суттєві архітектурні зміни та оптимізації для роботи на 32-бітних процесорах MIPS без апаратного FPU. Через це **вихідний код ядра більше не є сумісним із поточною гілкою master батьківського репозиторію Leviculum**. Модифіковане ядро Rust постачається безпосередньо у складі цього репозиторію (каталог `leviculum/`) разом із C-оболонкою `clevnode.c` та складальними скриптами.
>
> **Актуальність вихідного коду:**  
> Найновіші та найактуальніші виправлення, оптимізації пам'яті та патчі стабільності підтримуються безпосередньо у вихідному коді репозиторію (гілка `main`). Готові скомпільовані бінарні файли в релізах GitHub фіксують окремі контрольні етапи та можуть відставати від поточного стану коду. Для отримання максимальної швидкодії та найсвіжіших виправлень рекомендується пряма компіляція з вихідних текстів за допомогою `./build_mips_be.sh`.

---

### 2. Реальні показники споживання ресурсів і тривала телеметрія

Примітка: Усі показники пам'яті, процесора та системних ресурсів у таблицях нижче виміряні безпосередньо на реальному MIPS-обладнанні - фізичному роутері **ASUS RT-AC57U V3** (процесор Qualcomm Atheros QCA9563, 32-бітний MIPS 74Kc 775 МГц, 128 МБ RAM, OpenWrt ath79, ядро Linux 3.4.103-rt119) під безперервним цілодобовим навантаженням 24/7 (20-30 пірів Reticulum, клірнет та I2P, 5 000 ідентичностей у базі та 18 000+ дедуплікаційних хешів).

#### Телеметрія тривалої стабільності (Uptime):
* **Час безперервної роботи процесу:** 198 000 секунд (понад 55,0 годин безперервного виконання).
* **Аптайм самого роутера:** 200 000 секунд (~55,6 годин).
* **Середнє навантаження на процесор (CPU):** **~22.83%** під час обробки транзитного трафіку, сесій I2P SAM та видачі ресурсів NomadNet.
* **Надійність:** Повна відсутність витоків пам'яті, збоїв OOM (Out of Memory), апаратних винятків `SIGBUS` чи панік рантайму Rust.

| Метрика | Виміряне значення | Опис |
|:---|:---|:---|
| **Точний розмір бінарного файлу** | **~2.43&nbsp;МБ** | Статичний бінарник зі збіркою під `musl libc` (2 432 596 байт / 2.32 MiB) |
| **VmRSS (холодний старт)** | **3.7&nbsp;МБ** | Зафіксовано одразу після ініціалізації стека |
| **VmRSS (під безперервним навантаженням 24/7)** | **18.5&nbsp;&#8209;&nbsp;22.0&nbsp;МБ** | Стабілізована резидентна пам'ять під навантаженням 20-30 пірів і 5 000 вузлів |
| **VmSize / VSZ (віртуальна пам'ять)** | **19.8&nbsp;&#8209;&nbsp;25.0&nbsp;МБ** | Загальний простір віртуальних адрес (скорочено з 33.2 МБ) |
| **VmPeak (піковий сплеск споживання)** | **27.5&nbsp;&#8209;&nbsp;30.0&nbsp;МБ** | Максимальне значення під час періодичного скидання бази (скорочено з 47.8 МБ) |
| **VmSwap (використання swap)** | **0&nbsp;&#8209;&nbsp;3.5&nbsp;МБ** | Мінімальне звернення до накопичувача під транзитним навантаженням |

#### Розподіл потоків у системі:
У системі постійно працюють **3 постійних активних потоки** (плюс тимчасові сервісні завдання резолвінгу DNS):
1. **Головний потік (C):** Ініціалізація, конфігурація та неблокуючий цикл обробки мережевих подій (`Event Loop`).
2. **Потік воркера (C, `pthread`):** Асинхронна черга завдань та генерація відповідей на запити сторінок NomadNet (стек обмежено до 128 КБ).
3. **Єдиний робочий потік Tokio (Rust, `reticulum-node`):** Кооперативний планувальник для сокетів вводу-виводу, таймерів анонсів, постійного тунелю I2P SAM Bridge та FFI-моста (стек обмежено до 128 КБ).

#### Інтерактивна карта глобальної топології мережі:
Географічна візуалізація вузлів та з'єднань мережі в реальному часі, зафіксованих цим роутером, доступна за посиланням:
* **Онлайн-карта топології:** [https://reticulum-topology-map.ivansvarkovsky.workers.dev/](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)

> **Примітка щодо реалізації та меж проєкту:**  
> Веб-карта слугує демонстрацією живої телеметрії та масштабу мережі, що проходить крізь роутер. Зовнішній скрипт експорту та інфраструктура Cloudflare Worker є індивідуальними авторськими утилітами моніторингу і **не входять до складу та не поширюються в межах кодової бази цього репозиторію**.

---

### 3. Мінімальний комплект та вимоги до місця

Для автономної роботи вузла на роутері потрібні лише **3 елементи**:

1. **Виконуваний файл `clevnode`:** Скомпільований моноліт.
2. **Папка `.reticulum`:** Розташована поруч із бінарником, містить файл `config`.
3. **Скрипт запуску:** (наприклад, `/opt/etc/init.d/S90clevnode` для Entware) для автозапуску демона під час старту системи.

> **Вимоги до пам'яті накопичувача:**  
> Під час тривалої роботи (місяці безперервної маршрутизації, збереження хешів дедуплікації та шляхів) для робочих файлів вузла необхідно **щонайменше 15 МБ вільного місця** на USB-флешці або диску роутера.

> **Важливий нюанс із назвою каталогу:**  
> Під час збірки скрипт `build_mips_be.sh` створює конфігурацію в папці `clevnode/reticulum/config`. Під час перенесення на роутер (наприклад, у каталог `/tmp/mnt/sda1/home/lblogd/`) ця папка обов'язково має бути перейменована на **`.reticulum`** (із крапкою на початку).

---

### 4. Архітектура

```text
┌────────────────────────────────────────────────────────┐
│               C-оболонка (clevnode.c)                  │
│  - Керування процесом та системний виклик setsid()     │
│  - Черга завдань на м'ютексах (pthread worker)         │
│  - Попереднє кешування index.mu в ОЗП (MsgPack)        │
│  - Зв'язування запитів: Request -> Resource            │
└───────────────────────────┬────────────────────────────┘
                            │ FFI-межа (leviculum.h)
┌───────────────────────────▼────────────────────────────┐
│              Rust-ядро (libleviculum.a)                │
│  - Мережевий стек Reticulum (Packet, Link, Route)      │
│  - Асинхронний рантайм Tokio (-Zbuild-std)             │
│  - Криптографія: Curve25519, Ed25519, AES-128          │
│  - Статична лінковка з musl libc (+soft-float)         │
└────────────────────────────────────────────────────────┘
```

---

### 5. Ключові оптимізації

1. **Видалення Bluetooth (BLE) та D-Bus:**  
   Бібліотеки `bluer` та `dbus` вимкнено в `Cargo.toml`. Модуль `ble` ізольовано через `#[cfg(any())]`. Заощаджено ~6.6 МБ ОЗП та вимкнено 3 зайві фонові потоки.
2. **Вилучення Serial, RNode та KISS:**  
   Залежність `tokio-serial` прибрано. Легковаговий модуль-заглушка `tokio_serial.rs` забезпечує сумісність типів у драйвері.
3. **Обмеження буферів логів в ОЗП:**  
   У `event_log.rs` встановлено жорсткий ліміт у **1000 рядків** на дескриптор. Старі рядки видаляються через `buf.remove(0)`, що запобігає переповненню пам'яті.
4. **Модульна система Cargo Feature Flags:**  
   Бібліотеки `clap`, `serde-pickle` та `toml` зроблені опціональними. Створено прапорці `cli`, `rpc` та `toml-config`. Робота з форматом INI збережена повністю.
5. **Безпечна заглушка RPC (`remote_mgmt_dummy.rs`):**  
   Модуль `remote_mgmt.rs` вимкнено прапорцем `#[cfg(feature = "rpc")]`. Заглушка `remote_mgmt_dummy.rs` повертає `None` для запитів віддаленого керування, усуваючи паніки.
6. **Оптимізація лічильників (`CounterAtomic`, Zero-Overhead):**  
   Усунуто атомарні блокування шини на 32-бітній архітектурі MIPS.  
   *Нюанс:* Вирізано **лише деталізацію по фізичних інтерфейсах** (`lev_interface_stats_t`). Загальна статистика вузла (`lev_transport_stats`) та каналів (`lev_link_stats`) працює всередині ядра, але в C-коді навмисно не виводиться для швидкодії.
7. **Демонізація та перехоплення сигналів:**  
   Вбудовано виклик `setsid()`, що повністю від'єднує процес від TTY-терміналу та унеможливлює випадкове завершення по `Ctrl+C`. Обробник `sigaction` фіксує **PID процесу-ініціатора** сигналу `SIGTERM`. Сигнал `SIGPIPE` ігнорується.
8. **Асинхронне розпізнавання DNS та безперервний запуск:**  
   - *Проблема:* Синхронний резолвінг DNS (`to_socket_addrs()`) на етапі ініціалізації інтерфейсів призводив до аварійного завершення роботи демона з помилкою `configuration error (-6)`, якщо віддалений пір (наприклад, `rns.wdgwars.pl` або DDNS) був недоступний при старті або WAN-інтерфейс ще не піднявся.  
   - *Рішення:* Резолвінг доменних імен перенесено з етапу ініціалізації всередину фонового асинхронного циклу повторного підключення (`tcp_client_reconnect_loop` через `tokio::net::lookup_host`).  
   - *Переваги:*  
     - **Безперервний запуск:** Нода гарантовано стартує завжди, успішно піднімаючи всі доступні локальні інтерфейси (Wi-Fi, I2P, локальні TCP) навіть без інтернету на роутері.  
     - **Динамічне перепідключення:** Проблемні інтерфейси резолвлять свої домени у фоні кожні 5 секунд і автоматично підключаються, щойно мережа відновлюється.  
     - **Відстеження зміни IP (DDNS):** DNS резолвиться перед кожною спробою з'єднання, автоматично реагуючи на зміну динамічної IP-адреси віддаленого піра.
9. **Оптимізація структурних буферів та черг в ОЗП:**  
   - **Кеш дедуплікації пакетів (`FILE_STORAGE_PACKET_HASH_CAP`):** Зменшено зі 100 000 до 25 000 записів (економія ~3.5 МБ ОЗП; зберігає історію за 2-3 години транзиту для повного захисту від петель).  
   - **Черги моста подій FFI (`node.rs`):** Місткість черг C-Rust моста зменшено з 512/256 до `DEFAULT_CONTROL_CAP = 64` та `DEFAULT_DATA_CAP = 32`. C-цикл обробляє події кожні 500 мс, тому черги залишаються практично порожніми.  
   - **Час життя шляхів і тегів (`constants.rs`):** Термін придатності маршруту (`PATHFINDER_EXPIRY_SECS`) скорочено з 7 діб до 2 діб, а ліміт тегів (`MAX_PATH_REQUEST_TAGS`) - з 32 000 до 8 000.  
   - **Черги анонсів інтерфейсів (`MAX_QUEUED_ANNOUNCES_PER_INTERFACE`):** Зменшено з 16 384 до 512, що усуває буферизацію тисяч анонсів на шейпованих каналах.
10. **Оптимізація життєвого циклу сховища та ліміти бази (`known_destinations` та `ratchets`):**  
    - **Виправлення бага нескінченного оновлення:** У `storage.rs` ліквідовано цикл у `take_flush_snapshot`, який кожні 15 хвилин (`flush_interval = 900`) перезаписував таймстампи абсолютно всіх записів на поточний час (`e.timestamp = now`), через що база розросталася без обмежень (досягаючи 17 368 записів і 2.24 МБ).  
    - **Справжні таймстампи подій:** Таймстамп фіксується лише при фактичному надходженні анонсу (`set_identity`), строго відповідно до еталону Python Reticulum (`Identity.remember`).  
    - **Ліквідація злиття з диском при скиданні:** Видалено рудимент перечитування диска (`kd_store.load_all()`) у `flush_off_lock`. Снапшот пам'яті записується на диск напряму й атомарно без повторного воскресіння видалених записів, що відповідає актуальній архітектурі Python Reticulum.  
    - **Автоматична очистка та ліміти:** Запроваджено ліміт `MAX_KNOWN_DESTINATIONS = 5 000` та `DEFAULT_IDENTITY_CAP = 5 000` в пам'яті та безпосередньо при створенні снапшота для диска. Записи, старіші за 30 днів, автоматично відсікаються при збереженні та старті.  
    - **Кешування ратчетів (`MAX_LOADED_RATCHETS = 1 000`):** При старті в пам'ять завантажуються не більше 1 000 найсвіжіших ратчетів, а застарілі файли (> 30 днів) автоматично видаляються з накопичувача.
11. **Динамічне обслуговування багатьох сторінок та гаряче оновлення без перезапуску (NomadNet):**  
    - **Ледачий кеш за часом модифікації (`mtime`):** Вміст сторінок більше не фіксується в ОЗП намертво при старті. LRU-кэш (16 слотів) перевіряє системний `stat()` при кожному реальному зверненні. Якщо файл на диску було змінено, він автоматично перечитується на льоту без перезапуску демона.  
    - **Підтримка мікроблогу та довільних імен сторінок:** C-оболонка динамічно зіставляє будь-які запити в межах `/page/` (наприклад, `/page/blog-post.mu`, `/page/about.mu`) з файлами в папці `posts/` із захистом від path-traversal.  
    - **Фонова автореєстрація нових сторінок:** Цикл подій кожні 30 секунд сканує каталог `posts/` і автоматично реєструє нові `.mu` файли в ядрі Reticulum без розриву транзитних з'єднань.
12. **Об'єднання рантаймів Tokio та ліквідація зайвих потоків ОС:**  
    - **Ліквідація дублюючого екземпляра Tokio:** FFI-міст подій та мережевий ввід/вивід драйвера об'єднані в єдиний рантайм Tokio (`reticulum-node`). Усунуто окремий потік воркера (`tokio-runtime-w`), дублюючий epoll та зайві таймерні колеса.  
    - **Відключення фонового лічильника швидкості:** Запуск потоку `spawn_traffic_counter` ізольовано під `#[cfg(feature = "rpc")]`, усуваючи щосекундні перемикання контексту процесора.  
    - **Фіксація стека воркера:** Стек воркера `reticulum-node` обмежено до 128 КБ (`thread_stack_size(128 * 1024)`). Кількість постійних потоків процесу скорочено до 3, віртуальне адресне середовище (`VmSize`) впало до 19.8 - 25.0 МБ, а піковий сплеск пам'яті (`VmPeak`) скоротився з 47.8 МБ до ~27.5 МБ (економія понад 20 МБ пікового ОЗП).
13. **Апаратна оптимізація MIPS та миттєва дедуплікація пакетів:**  
    - **Розгортання циклів Curve25519 (`opt-level = 3`):** Для пакетів `curve25519-dalek`, `ed25519-dalek` та `x25519-dalek` увімкнено максимальну оптимізацію в профілі `release`. Компілятор розгортає 32-бітну арифметику полів, прискорюючи рукостискання Link та перевірку підписів на 30-40% на MIPS без FPU.  
    - **Прямий 8-байтний хешер (`FastHashBuilder`):** Стандартний SipHash-1-3 у `packet_cache` замінено на прозорий хешер, що зчитує перші 8 байт SHA-256 за 1 інструкцію. Накладні витрати CPU на дедуплікацію транзитних пакетів зведено до нуля.  
    - **Вирізання мертвого коду:** Повністю видалено бібліотеки регулярних виразів (`regex-automata`, `regex-syntax`) та зроблено опціональним `serde_json`, заощадивши понад 114 КБ розміру бінарника.

---

### 6. Апаратна платформа та підтримувані архітектури

Офіційні автономні статичні релізні архіви збираються та тестуються через QEMU для **6 апаратних архітектур**:

* **MIPS Big-Endian (`mips-unknown-linux-musl`):** Qualcomm Atheros AR9xxx, QCA95xx, QCA55xx (таргет OpenWrt `ath79`).
* **MIPSEL Little-Endian (`mipsel-unknown-linux-musl`):** MediaTek / Ralink MT7620, MT7621, MT7628 (OpenWrt/Keenetic `ramips`).
* **ARMv7-A 32-bit (`armv7-unknown-linux-musleabihf`):** Cortex-A7/A9, Raspberry Pi 2 / Zero 2W, Orange Pi (hard-float).
* **ARM64 / AArch64 (`aarch64-unknown-linux-musl`):** Cortex-A53/A72, Raspberry Pi 3/4/5, сучасні ARM-роутери та VPS.
* **x86_64 (`x86_64-unknown-linux-musl`):** 64-бітні сервери Intel / AMD, ПК та VPS.
* **i686 (`i686-unknown-linux-musl`):** 32-бітні x86 платформи та тонкі клієнти.
* **Апаратний FPU:** Не потрібен на MIPS/MIPSEL; розрахунки ведуться програмно завдяки `-msoft-float`.
* **Сумісність з ядрами Linux:** Бінарник зібраний із `musl libc`, яка потребує ядра **Linux 2.6.39 або новішого**. На версії 2.6.39 робота прямо не перевірялася, але має забезпечуватися стабільністю системних викликів. **Підтверджено бездоганну роботу на ядрі 3.4.103-rt119**. Ядра 4.x, 5.x та 6.x підтримуються архітектурно.

> **Автоматичні мультиплатформенні релізи:**  
> Автономні статичні релізні архіви із верифікацією запуску через QEMU автоматично збираються та публікуються для всіх 6 цільових архітектур у розділі GitHub Releases.

---

### 7. Криптографічні ключі та персональна екосистема

1. **`identities/lblogd`** (64 байти): Постійний ключ вузла сайту NomadNet.
2. **`identities/site.id`** (64 байти): Службовий ідентифікатор сайту.
3. **`.reticulum/storage/transport_identity`** (64 байти): Головний ключ маршрутизатора (паспорт вузла). **Видаляти категорично заборонено.**

#### Приклад ключа `lblogd` у Base32 (із маскуванням):
```text
WDHJGSJFCZ3E72YE6L7LQKDKW46BHA6Q56Q*************************************************RQTWUQ=
```

#### Пов'язана структура персональної адресації:
* **Крипто-паспорт (Identity Hash):** `c1cfe6b1620ff4515f48********fa8c`
* **Адреса сайту в NomadNet:** `nomad://eeccf2849599909527d4********141`
* **Персональна пошта/месенджер (LXMF):** `lxmf:bff56bcc8ec6e1b49759********16b0`

Доки ви зберігаєте один і той самий файл `lblogd`, адреса сайту та поштова адреса LXMF залишатимуться постійними на всіх ваших пристроях.

---

### 8. Налаштування мережі (`clevnode/reticulum/config`)

```ini
[reticulum]
# Назва вузла в мережі.
# Якщо рядок відсутній, застосовується дефолтна назва з коду: "Clevnode" (з великої літери).
node_name = Router Node

# Дозвіл транзитної маршрутизації пакетів сторонніх вузлів.
enable_transport = yes

# Дозвіл локальним утилітам підключатися до активного стека.
share_instance = yes

# Порт TCP для IPC-взаємодії утиліт rnstatus/rnpath.
shared_instance_port = 37428

# Періодичність збереження стану на диск у секундах (15 хвилин).
flush_interval = 900

[logging]
# Базовий рівень логів (1 = LEV_LOG_INFO).
loglevel = 1

[interfaces]
  # Локальний серверний інтерфейс для домашньої мережі.
  [[Local Wi-Fi Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  # Вихідне TCP-з'єднання з публічним хабом спільноти.
  [[WDGWars Node]]
    type = TCPClientInterface
    enabled = yes
    target_host = rns.wdgwars.pl
    target_port = 4242
    bootstrap_only = yes
    name = WDGWars Node

  # Інтерфейс мережі I2P (ОПЦІЙНИЙ - закоментуйте, якщо I2P не потрібен).
  [[I2P Bridge Interface]]
    type = I2PInterface
    enabled = yes
    connectable = yes
    discoverable = yes
    discovery_name = Router Node I2P
    reconnect_wait = 120
    peers = wnbroeyo5kotdkq4qpitosoch4f64fiwv4uyrk7tp57w2ubthatq.b32.i2p, fc22dfljkpt2beq3itqjkz2k7oizuythif7aa3m73okq7mqrg66q.b32.i2p
```

---

### 9. Механіка взаємодії з I2P

#### Ключі та файл `i2p_iface_0.i2p`:
Reticulum спілкується з I2P через SAM API (порт `7656`). Під час старту перевіряється `.reticulum/storage/i2p/i2p_iface_0.i2p`. Якщо файлу немає, створюється новий закритий ключ (`DEST GENERATE`), зберігається на диск, і обчислюється постійна адреса `.b32.i2p`.

#### Шлюз клірнет <-> I2P:
Параметр `enable_transport = yes` робить роутер транзитним мостом між відкритим інтернетом та тунелями I2P. Для повної ізоляції мереж:
* Виставте `enable_transport = no` (режим виключно кінцевого пристрою), або
* Додайте `mode = boundary` до налаштувань інтерфейсу, щоб зупинити проходження анонсів.

#### Публічні піри (`peers`):
Адреси пірів у конфігурації - це публічні хаби спільноти, їх поширення безпечне. Проте не публікуйте власну адресу `.b32.i2p`, якщо ваш вузол призначений лише для приватного користування.

---

### 10. Прогрівання мережі (Network Warm-up & Convergence)

Побудова карти мережі в Reticulum здійснюється реактивно на базі криптографічних анонсів. Під час холодного старту таблиця маршрутів порожня, а побудова тунелів I2P триває кілька хвилин. Надайте роутеру **5–15 хвилин після запуску** для первинної синхронізації та стабілізації маршрутів.

---

### 11. Складання проєкту (`build_mips_be.sh`)

```bash
# Встановлення вимог на хості Ubuntu/Debian x86_64
sudo apt update && sudo apt install -y git curl wget gcc make tar xz-utils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly

# Швидке інкрементальне складання (~2-3 секунди)
./build_mips_be.sh

# Складання з екстремальним стрипінгом sstrip
./build_mips_be.sh --sstrip

# Повне очищення та збирання з нуля
./build_mips_be.sh --clean
```

---

### 12. Розгортання на роутері

```bash
# Перенесення бінарника та конфігурації (приклад із маскуванням)
scp -P 63*** clevnode/clevnode admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/
scp -P 63*** clevnode/reticulum/config admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/.reticulum/config
```

Скрипт служби автозапуску `/opt/etc/init.d/S90clevnode`:
```sh
#!/bin/sh
ENABLED=yes
PROG="clevnode"
DIR="/tmp/mnt/sda1/home/lblogd"
BIN="$DIR/$PROG"
LOG="$DIR/clevnode.log"

start() {
    [ "$ENABLED" != "yes" ] && return 0
    pidof "$PROG" >/dev/null && return 0
    echo -n "Starting $PROG... "
    cd "$DIR" || exit 1
    "$BIN" > "$LOG" 2>&1 &
    sleep 1
    pidof "$PROG" >/dev/null && echo "OK" || echo "FAILED"
}

stop() {
    echo -n "Stopping $PROG... "
    killall "$PROG" 2>/dev/null
    for i in 1 2; do
        sleep 1
        ! pidof "$PROG" >/dev/null && echo "OK" && return 0
    done
    killall -9 "$PROG" 2>/dev/null
    echo "Force killed"
}

case "$1" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  pidof "$PROG" >/dev/null && echo "Running" || echo "Stopped" ;;
    *)       echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
```

---

### 13. Обслуговування сховища (`.reticulum/storage`)

* **`transport_identity`:** Головний ключ роутера. **Не видаляти.**
* **`packet_hashlist`:** Автоматичний кільцевий кеш (~3.6 МБ). Очищення не потребує.
* **`known_destinations`:** База адрес і маршрутів. Якщо на флешці бракує місця, можна скинути:
  ```sh
  /opt/etc/init.d/S90clevnode stop
  rm -f /tmp/mnt/sda1/home/lblogd/.reticulum/storage/known_destinations
  /opt/etc/init.d/S90clevnode start
  ```

---

### 14. Відмова від відповідальності

Усі шляхи до каталогів, назви та мережеві параметри є індивідуальними для конкретного тестового стенда. Інформація надається виключно в ознайомчих цілях без гарантій будь-якого роду.

---

### 15. Корисні посилання

* [Онлайн-карта топології Reticulum (Іван Сварковський)](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)
* [Офіційне керівництво з інтерфейсів Reticulum](https://reticulum.network/manual/interfaces.html) [1.1.2]
* [Таблиця реалізацій стека Reticulum](https://reticulum.miraheze.org/wiki/Implementations)
* [Каталог проєктів Awesome Reticulum](https://reticulum.miraheze.org/wiki/Awesome_Reticulum)
* [Документація з Reticulum на Dead.md](https://dead.md/docs/ham/reticulum-network) [1.4.3]
* [Налаштування Reticulum через MeshChat та Yggdrasil](https://devzone.org.ua/post/reticulum-vstanovlennia-na-prykladi-meshchat-z-pidkliuchenniam-cherez-yggdrasil) [1.1.2]
* [Налаштування I2P роутера через мережу Yggdrasil](https://devzone.org.ua/post/vstanovlennia-routera-i2p-z-pidkliuchenniam-cherez-yggdrasil) [1.2.1]
* [Офіційний репозиторій Reticulum на GitHub](https://github.com/markqvist/reticulum)
* [Репозиторій ядра Leviculum на Codeberg](https://codeberg.org/Lew_Palm/Leviculum)
* [Специфікація протоколу LXMF](https://github.com/markqvist/LXMF)
* [Клієнт Sideband для LXMF](https://github.com/markqvist/sideband)
* [Статті про Reticulum на DevZone](https://devzone.org.ua/topic/tag/reticulum)

---

<a id="russian"></a>

### Высокопроизводительный автономный узел Reticulum и сервер NomadNet для встраиваемых устройств, роутеров и серверов

`clevnode` - это оптимизированный автономный гибридный монолит (чистый Си + статическая библиотека на Rust), созданный для постоянной круглосуточной работы в качестве шлюза и транзитного маршрутизатора сети Reticulum, а также легковесного сервера страниц NomadNet (Micron) на встраиваемых роутерах, микрокомпьютерах (SBC) и серверах (MIPS, MIPSEL, ARMv7, ARM64, x86_64, i686).

*Этимология:* **C-Lev-Node** = **C** (производительная Си-оболочка) + **Lev**iculum (сетевой стек) + **Node** (автономный узел). Дополнительная смысловая игра слов: **Clever Node** («Умный узел»).

#### Поддерживаемые возможности в текущей кодовой базе:
- **Полноценный стек Reticulum:** маршрутизация пакетов, установление соединений (Links), анонсы и построение путей.
- **Асинхронное Rust-ядро (`leviculum`):** статически скомпилированное с `musl libc`, с глубокими низкоуровневыми оптимизациями для 32-битных встраиваемых платформ без FPU (`-msoft-float`) и полноценной поддержкой современных процессоров ARM и x86.
- **C-монолит (`clevnode.c`):** асинхронный воркер, очередь задач для обработки запросов, прекэширование MsgPack-страниц в ОЗУ.
- **Носителели трафика (Транспорт):** работа в качестве транзитного узла и шлюза через I2P SAM Bridge (порт 7656).
- **NomadNet / Micron:** обслуживание легковесных страниц (запросы `/page/index.mu`) с автоматической передачей больших страниц в виде составных ресурсов (Resources) при превышении MTU.

---

### 1. Происхождение, авторство и лицензия

Проект распространяется на условиях лицензии **AGPL-3.0-or-later** (GNU Affero General Public License v3 или более поздней версии).

* **Базовый родительский проект:** [Leviculum](https://codeberg.org/Lew_Palm/leviculum)
* **Автор оригинального ядра:** Lew Palm
* **Исходный коммит родительского репозитория:** `808908b12b802e76e08b4a9067137061087a0d05`
* **Автор архитектурной адаптации и Си-монолита:** Иван Сварковский ([ivansvarkovsky@gmail.com](mailto:ivansvarkovsky@gmail.com))

> **Замечание о совместимости исходного кода:**  
> В оригинальную кодовую базу `leviculum` были внесены существенные архитектурные изменения, оптимизации и патчи для работы на 32-битных встраиваемых MIPS-процессорах без FPU. В связи с этим **исходный код ядра более не является обратно совместимым с актуальной веткой master репозитория Leviculum**. Модифицированные исходные коды Rust-ядра поставляются непосредственно в составе данного репозитория (каталог `leviculum/`) вместе с Си-оболочкой `clevnode.c` и скриптами сборки.
>
> **Актуальность исходного кода:**  
> Самые свежие и последние изменения, оптимизации памяти и патчи стабильности находятся непосредственно в исходном коде репозитория (ветка `main`). Готовые скомпилированные бинарники в релизах GitHub фиксируют контрольные версии и могут отставать от текущего состояния разработки. Для получения наилучшей производительности и самых свежих исправлений рекомендуется самостоятельная сборка из исходников с помощью `./build_mips_be.sh`.

---

### 2. Реальные показатели потребления ресурсов и длительная телеметрия

Примечание: Все показатели памяти, процессора и системных ресурсов в таблицах ниже измерены непосредственно на реальном MIPS-оборудовании - физическом роутере **ASUS RT-AC57U V3** (процессор Qualcomm Atheros QCA9563, 32-битный MIPS 74Kc 775 МГц, 128 МБ RAM, OpenWrt ath79, ядро Linux 3.4.103-rt119) под непрерывной круглосуточной нагрузкой 24/7 (20-30 пиров Reticulum, клирнет и I2P, 5 000 адресатов в базе и 18 000+ дедупликационных хешей).

#### Телеметрия непрерывной работы (Uptime):
* **Время работы процесса `clevnode`:** 198 000 секунд (свыше 55,0 часов непрерывного исполнения).
* **Общий аптайм роутера:** 200 000 секунд (~55,6 часов).
* **Средняя утилизация CPU:** **~22.83%** под сетевой нагрузкой маршрутизации, фоновым сессиям I2P SAM и раздаче ресурсов NomadNet. Полное отсутствие зависаний и фонового перегрева ядра.
* **Надежность:** Подтверждено полное отсутствие утечек памяти, сбоев OOM (Out of Memory), аппаратных исключений `SIGBUS` и паник рантайма Rust.

| Метрика | Измеренное значение | Описание |
|:---|:---|:---|
| **Точный размер исполняемого файла** | **~2.43&nbsp;МБ** | Статический бинарник со сборкой под `musl libc` (2 432 596 байт / 2.32 MiB) |
| **VmRSS (холодный старт)** | **3.7&nbsp;МБ** | Зафиксировано сразу после инициализации стека |
| **VmRSS (под непрерывной нагрузкой 24/7)** | **18.5&nbsp;&#8209;&nbsp;22.0&nbsp;МБ** | Стабилизированная резидентная память под нагрузкой 20-30 пиров и 5 000 узлов |
| **VmSize / VSZ (объем виртуальной памяти)** | **19.8&nbsp;&#8209;&nbsp;25.0&nbsp;МБ** | Общее виртуальное адресное пространство процесса (снижено с 33.2 МБ) |
| **VmPeak (пиковый всплеск потребления)** | **27.5&nbsp;&#8209;&nbsp;30.0&nbsp;МБ** | Максимальное значение во время периодического сброса базы (снижено с 47.8 МБ) |
| **VmSwap (использование файла подкачки)** | **0&nbsp;&#8209;&nbsp;3.5&nbsp;МБ** | Минимальное обращение к свопу под постоянной транзитной нагрузкой |

#### Распределение потоков в операционной системе:
В системе постоянно активно **3 постоянных потока** (плюс временные сервисные задачи резолвинга DNS):
1. **Основной поток (C):** Инициализация структур, регистрация интерфейсов и главный неблокирующий цикл опроса сетевых событий (`Event Loop`).
2. **Поток воркера (C, `pthread`):** Асинхронная очередь задач, потокобезопасная компоновка и отдача ответов на входящие запросы страниц NomadNet (стек ограничен до 128 КБ).
3. **Единый рабочий поток рантайма Tokio (Rust-ядро, `reticulum-node`):** Кооперативный планировщик для сетевого ввода-вывода (I/O) сокетов, таймеров анонсов, постоянного туннеля I2P SAM Bridge и FFI-моста событий (стек ограничен до 128 КБ).

#### Интерактивная карта глобальной топологии сети:
Географическая визуализация узлов и соединений сети в реальном времени, зафиксированных данным роутером, доступна по ссылке:
* **Онлайн-карта топологии:** [https://reticulum-topology-map.ivansvarkovsky.workers.dev/](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)

> **Примечание об архитектуре и границах проекта:**  
> Веб-карта служит наглядной демонстрацией телеметрии и масштаба сети, проходящей сквозь данный роутер. Внешний скрипт экспорта данных и инфраструктура Cloudflare Worker являются индивидуальными пользовательскими утилитами мониторинга и **не входят в состав и не распространяются в рамках кодовой базы этого репозитория**.

---

### 3. Минимальный комплект и требования к месту на накопителе

Для полноценной и автономной работы узла на роутере пользователю необходим минимальный комплект из **3 элементов**:

1. **Бинарный файл `clevnode`:** Сам исполняемый файл.
2. **Папка `.reticulum` с файлом `config` внутри:** Лежащая в той же директории, что и бинарник.
3. **Скрипт запуска:** (например, `/opt/etc/init.d/S90clevnode` для Entware) для автоматического старта в фоне при включении роутера.

> **Требования к объему свободного места:**  
> В условиях продолжительной работы (многомесячная маршрутизация, сохранение маршрутов, кэш дедупликации) для всех рабочих файлов узла на флешке роутера необходимо **не менее 15 МБ свободного места**.

> **Важный нюанс по именованию директорий:**  
> При сборке скрипт `build_mips_be.sh` создает дефолтный конфиг в открытой папке `clevnode/reticulum/config` для локального удобства. При переносе на роутер (в рабочую директорию, например, `/tmp/mnt/sda1/home/lblogd/`) папка должна быть переименована в скрытую **`.reticulum`** (с точкой в начале), что полностью соответствует ожиданиям бинарника и не захламляет домашний каталог.

---

### 4. Архитектурная концепция

```text
┌────────────────────────────────────────────────────────┐
│               Си-оболочка (clevnode.c)                 │
│  - Управление процессами, вызовы setsid()              │
│  - Потокобезопасная очередь задач (pthread worker)     │
│  - Предварительное кэширование index.mu в ОЗУ (MsgPack)│
│  - Связывание входящих запросов: Request -> Resource   │
└───────────────────────────┬────────────────────────────┘
                            │ FFI-граница (leviculum.h)
┌───────────────────────────▼────────────────────────────┐
│              Rust-ядро (libleviculum.a)                │
│  - Полный сетевой стек Reticulum (Packet, Link, Route) │
│  - Асинхронный рантайм Tokio (-Zbuild-std)             │
│  - Криптография: Curve25519, Ed25519, AES-128          │
│  - Статическая линковка musl libc (+soft-float)        │
└────────────────────────────────────────────────────────┘
```

---

### 5. Ключевые оптимизации системы

1. **Полное удаление зависимостей Bluetooth (BLE) и D-Bus:**  
   Библиотеки `bluer` и `dbus` закомментированы в `Cargo.toml`. Модуль `ble` в `interfaces/mod.rs` изолирован через `#[cfg(any())]`. Освобождено ~6.6 МБ оперативной памяти и исключен запуск трех фоновых потоков опроса шин.
2. **Исключение Serial, RNode и KISS:**  
   Зависимость `tokio-serial` отключена в `Cargo.toml`. Написан мок-модуль `tokio_serial.rs` для сохранения строгой совместимости типов в драйвере. Исключены системные прерывания на опрос TTY-портов.
3. **Лимитирование буферов логов в оперативной памяти:**  
   В `event_log.rs` внедрен кольцевой буфер с жестким лимитом в **1000 строк** на активный дескриптор. При превышении лимита старые записи вытесняются методом `buf.remove(0)`, защищая роутер от Out-of-Memory при длительной непрерывной работе.
4. **Модульная система Cargo-фич (Feature Flags):**  
   Библиотеки `clap`, `serde-pickle` и `toml` сделаны опциональными. Созданы флаги `cli`, `rpc` и `toml-config` (по умолчанию отключены, в default оставлена только компрессия). Парсер INI-конфигураций оставлен полностью функциональным.
5. **Безопасная заглушка RPC (`remote_mgmt_dummy.rs`):**  
   Оригинальный модуль `remote_mgmt.rs` изолирован флагом `#[cfg(feature = "rpc")]`. Добавлен модуль `remote_mgmt_dummy.rs`, возвращающий безопасный `None` на входящие запросы управления. Это устранило паники рантайма при сканировании ноды внешними утилитами.
6. **Оптимизация счетчиков трафика (`CounterAtomic`, Zero-Overhead):**  
   Исключены накладные расходы на блокирующие атомарные синхронизации шины памяти на 32-битной архитектуре MIPS.  
   *Важная деталь реализации:* Вырезана **только детализация трафика по физическим интерфейсам** (`lev_interface_stats_t`). При этом глобальный подсчет трафика всего узла (`lev_transport_stats`) и статистика каналов/соединений (`lev_link_stats`) продолжают полноценно функционировать внутри ядра библиотеки. В Си-коде `clevnode.c` промежуточный сбор и вывод этой статистики намеренно не производятся ради максимального снижения накладных расходов.
7. **Пошаговое логирование, демонизация и отвязка от TTY:**  
   В `clevnode.c` встроен системный вызов `setsid()`: процесс отвязывается от терминала и запускается в новой независимой сессии, что исключает падение узла по сигналу `SIGINT` (`Ctrl+C`) при просмотре логов. Обработчик `sigaction` (`SA_SIGINFO`) перехватывает сигнал `SIGTERM` и выводит в лог точный **PID процесса-отправителя команды**. Сигнал `SIGPIPE` игнорируется.
8. **Асинхронное разрешение DNS и непрерывный запуск:**  
   - *Проблема:* Синхронный резолвинг DNS (`to_socket_addrs()`) на этапе инициализации интерфейсов (`build_client` / `driver/mod.rs`) приводил к падению всего демона с ошибкой `configuration error (-6)`, если хотя бы один удаленный пир (например, `rns.wdgwars.pl` или DDNS) был временно недоступен или сетевой стек роутера еще не поднял интернет на старте.  
   - *Решение:* Резолвинг доменных имен перенесен из этапа инициализации внутрь фонового асинхронного цикла переподключения (`tcp_client_reconnect_loop` через `tokio::net::lookup_host`).  
   - *Преимущества:*  
     - **Непрерывный запуск:** Нода гарантированно стартует всегда, поднимая все локальные и работающие интерфейсы (Wi-Fi, I2P, локальные TCP), даже при полном отсутствии интернета на роутере.  
     - **Динамическое переподключение:** Проблемные интерфейсы пытаются резолвить свои домены в фоне каждые 5 секунд и подключаются автоматически, как только сеть восстанавливается.  
     - **Корректная обработка смены IP (DDNS):** Так как DNS резолвится заново перед каждой попыткой соединения, узел корректно обрабатывает смену динамического IP удаленного узла (например, `zer0bitz.ddns.net`) и не зависает на старом адресе.
9. **Оптимизация структурных буферов и очередей (Memory Capping):**  
   - **Лимит кэша дедупликации пакетов (`FILE_STORAGE_PACKET_HASH_CAP`):** Снижен со 100 000 до 25 000 записей. Экономит ~3.5 МБ ОЗУ; 25k записей при транзитном трафике обеспечивают 2-3 часа полной защиты от петель маршрутизации.  
   - **Очереди событий FFI (`node.rs`):** Емкость очередей моста C-Rust снижена с 512/256 до `DEFAULT_CONTROL_CAP = 64` и `DEFAULT_DATA_CAP = 32`. Си-цикл немедленно вычитывает события каждые 500 мс, поэтому в очередях редко накапливается более 5-10 элементов.  
   - **Время жизни путей (`PATHFINDER_EXPIRY_SECS`):** Сокращено с 7 суток до 2 суток, а лимит тегов запросов путей (`MAX_PATH_REQUEST_TAGS`) - с 32 000 до 8 000. В динамичной сети пути старше 2 суток часто недействительны, а реактивный протокол Reticulum штатно обновляет их через `PATH_REQUEST`.  
   - **Очереди анонсов интерфейсов (`MAX_QUEUED_ANNOUNCES_PER_INTERFACE`):** Снижены с 16 384 до 512. Устраняет буферизацию тысяч устаревающих анонсов на шейпированных каналах (2% полосы).
10. **Оптимизация жизненного цикла хранилища и лимиты базы (`known_destinations` & `ratchets`):**  
    - **Ликвидация бага бесконечного омоложения:** В `storage.rs` устранен ошибочный цикл в `take_flush_snapshot`, который каждые 15 минут (`flush_interval = 900`) перезаписывал таймстамп абсолютно всех записей на текущий (`e.timestamp = now`), из-за чего адреса никогда не старели, а база раздулась до 17 368 записей (2.24 МБ).  
    - **Истинные таймстампы событий:** Таймстамп теперь фиксируется строго в момент реального приема анонса (`set_identity`), полностью соответствуя поведению эталонного Python Reticulum (`Identity.remember`).  
    - **Ликвидация слияния с диском при сбросе:** Удален устаревший вызов перечитывания диска (`kd_store.load_all()`) в `flush_off_lock`. Снапшот памяти сохраняется на диск напрямую и атомарно без воскрешения старых записей, в точном соответствии с актуальным Python Reticulum, где слияние с кэшем на диске объявлено устаревшим.  
    - **Автоматическая очистка и лимиты:** Введен лимит `MAX_KNOWN_DESTINATIONS = 5 000` и `DEFAULT_IDENTITY_CAP = 5 000` в памяти и при формировании снапшота на диск. Записи старше 30 дней автоматически вычищаются при старте и сохранении.  
    - **Кэширование ратчетов (`MAX_LOADED_RATCHETS = 1 000`):** При старте в память загружаются не более 1 000 свежих ратчетов, а просроченные файлы (> 30 дней) автоматически удаляются с накопителя при загрузке.
11. **Динамическая раздача страниц и горячее обновление без перезапуска (NomadNet):**  
    - **Ленивый кэш по времени модификации (`mtime`):** Контент страниц больше не фиксируется в ОЗУ намертво при старте. LRU-кэш (16 слотов) проверяет системный `stat()` при каждом реальном запросе клиента. Если файл на диске был изменен, он автоматически перечитывается на лету без перезапуска демона.  
    - **Поддержка микроблога и любых имен страниц:** Си-оболочка динамически сопоставляет любые входящие запросы в пределах префикса `/page/` (например, `/page/blog-post.mu`, `/page/about.mu`) с файлами в папке `posts/` с защитой от path-traversal.  
    - **Фоновая авторегистрация новых страниц:** Главный цикл каждые 30 секунд сканирует каталог `posts/` и автоматически регистрирует новые `.mu` файлы в ядре Reticulum без разрыва транзитных соединений.
12. **Объединение рантаймов Tokio и ликвидация лишних системных потоков:**  
    - **Ликвидация дублирующего экземпляра Tokio:** FFI-мост событий и сетевой ввод/вывод драйвера объединены в единый рантайм Tokio (`reticulum-node`). Устранен отдельный поток воркера (`tokio-runtime-w`), дублирующий epoll и лишние таймерные колеса.  
    - **Отключение фонового счетчика скорости:** Запуск потока `spawn_traffic_counter` изолирован под `#[cfg(feature = "rpc")]`, устраняя ежесекундные холостые переключения контекста процессора.  
    - **Ограничение стека воркера:** Стек рабочего потока `reticulum-node` ограничен до 128 КБ (`thread_stack_size(128 * 1024)`). Количество постоянных потоков процесса снижено до 3, виртуальное адресное пространство (`VmSize`) упало до 19.8 - 25.0 МБ, а пиковый всплеск памяти (`VmPeak`) снизился с 47.8 МБ до ~27.5 МБ (экономия более 20 МБ пикового ОЗУ).
13. **Аппаратная оптимизация MIPS и мгновенная дедупликация пакетов:**  
    - **Разворачивание циклов Curve25519 (`opt-level = 3`):** Для пакетов `curve25519-dalek`, `ed25519-dalek` и `x25519-dalek` включена максимальная оптимизация в профиле `release`. Компилятор разворачивает 32-битную арифметику полей, ускоряя рукопожатия Link и проверку подписей на 30-40% на MIPS без FPU.  
    - **Прямой 8-байтный хешер (`FastHashBuilder`):** Стандартный SipHash-1-3 в `packet_cache` заменен на прозрачный хешер, считывающий первые 8 байт SHA-256 за 1 процессорную инструкцию. Накладные расходы CPU на дедупликацию транзитных пакетов сведены к нулю.  
    - **Вырезание мертвого кода:** Полностью удалены библиотеки регулярных выражений (`regex-automata`, `regex-syntax`) и сделан опциональным `serde_json`, сэкономив более 114 КБ размера бинарника.

---

### 6. Аппаратная платформа и поддерживаемые архитектуры

Официальные автономные статические релизные архивы собираются и проверяются через QEMU для **6 аппаратных архитектур**:

* **MIPS Big-Endian (`mips-unknown-linux-musl`):** Qualcomm Atheros AR9xxx, QCA95xx, QCA55xx (таргет OpenWrt `ath79`).
* **MIPSEL Little-Endian (`mipsel-unknown-linux-musl`):** MediaTek / Ralink MT7620, MT7621, MT7628 (OpenWrt/Keenetic `ramips`).
* **ARMv7-A 32-bit (`armv7-unknown-linux-musleabihf`):** Cortex-A7/A9, Raspberry Pi 2 / Zero 2W, Orange Pi (hard-float).
* **ARM64 / AArch64 (`aarch64-unknown-linux-musl`):** Cortex-A53/A72, Raspberry Pi 3/4/5, современные ARM-роутеры и VPS.
* **x86_64 (`x86_64-unknown-linux-musl`):** 64-битные серверы Intel / AMD, ПК и VPS.
* **i686 (`i686-unknown-linux-musl`):** 32-битные платформы x86 и тонкие клиенты.
* **Аппаратный FPU:** Не требуется на MIPS/MIPSEL; вычисления проводятся программно благодаря `-msoft-float`.
* **Совместимость с ядрами Linux:**  
  Бинарный файл скомпилирован со статической библиотекой `musl libc`, минимальным системным требованием которой является ядро **Linux 2.6.39**.  
  *На ядрах 2.6.39 работа напрямую не проверялась*, однако теоретически бинарник должен запускаться на любых ядрах от 2.6.39 и новее благодаря правилу ядра Linux «never break userspace» и стабильности ABI системных вызовов.  
  **Фактически подтверждена стабильная работа на ядре 3.4.103-rt119 (ASUS RT-AC57U V3)**. На ядрах 4.x, 5.x и 6.x работа гарантируется архитектурой статической сборки.

> **Автоматические мультиплатформенные релизы:**  
> Автономные статические релизные архивы с предварительной верификацией запуска через QEMU автоматически собираются и публикуются для всех 6 целевых архитектур в разделе GitHub Releases.

---

### 7. Криптографические ключи и экосистема узла

В каталоге ноды используются три критически важных 64-байтных файла ключей:

1. **`identities/lblogd`** (64 байта): Постоянный ключ узла NomadNet (блога/сайта). Определяет публичный адрес страницы в сети. **Терять и удалять нельзя**, иначе узел сменит адрес страницы.
2. **`identities/site.id`** (64 байта): Служебный идентификатор сайта, созданный при развертывании.
3. **`.reticulum/storage/transport_identity`** (64 байта): Мастер-ключ роутера (криптографический паспорт). **Удалять категорически запрещено**, иначе сменится Transport ID, а все связи и маршруты аннулируются.

#### Структура ключа `lblogd` в Base32 (пример с маскированием):
```text
WDHJGSJFCZ3E72YE6L7LQKDKW46BHA6Q56Q*************************************************RQTWUQ=
```

#### Связанная структура персональной адресации:
* **Единый крипто-паспорт (Identity Hash):** `c1cfe6b1620ff4515f48********fa8c`
* **Сайт в сети NomadNet на домашнем роутере:** `nomad://eeccf2849599909527d4********141`
* **Адрес личной почты и мессенджера (LXMF / Sideband / MeshChat):** `lxmf:bff56bcc8ec6e1b49759********16b0`

Пока вы сохраняете неизменным один и тот же файл ключа (`lblogd`), ваш почтовый адрес LXMF и адрес сайта в сети будут оставаться постоянными на всех ваших устройствах.

---

### 8. Конфигурация сети (`clevnode/reticulum/config`)

```ini
[reticulum]
# Имя ноды в глобальной сети.
# Если параметр не указан, применяется стандартное имя из C-кода: "Clevnode" (с большой буквы).
node_name = Router Node

# Разрешает сквозную маршрутизацию пакетов сторонних узлов (режим транзитного роутера).
enable_transport = yes

# Позволяет локальным утилитам подключаться к запущенному экземпляру стека.
share_instance = yes

# Порт TCP для локального межпроцессного взаимодействия (IPC) утилит rnstatus/rnpath.
shared_instance_port = 37428

# Интервал сброса кэша маршрутов и хэшей на накопитель в секундах (900 сек = 15 минут).
flush_interval = 900

[logging]
# Базовый уровень вывода логов. В FFI-слое жестко зафиксирован уровень 1 (LEV_LOG_INFO).
loglevel = 1

[interfaces]
  # Интерфейс локального сервера для устройств домашней сети (ПК, телефоны по Wi-Fi).
  [[Local Wi-Fi Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  # Исходящее постоянное TCP-подключение к публичному хабу сети Reticulum (Польша).
  [[WDGWars Node]]
    type = TCPClientInterface
    enabled = yes
    target_host = rns.wdgwars.pl
    target_port = 4242
    bootstrap_only = yes
    name = WDGWars Node

  # Мост в анонимную сеть I2P через локальный SAM API (порт 7656).
  # Интерфейс является ПОЛНОСТЬЮ ОПЦИОНАЛЬНЫМ и нужен далеко не каждому пользователю.
  # Если I2P не требуется, секцию ниже можно закомментировать символом #.
  [[I2P Bridge Interface]]
    type = I2PInterface
    enabled = yes
    connectable = yes
    discoverable = yes
    discovery_name = Router Node I2P
    reconnect_wait = 120
    peers = wnbroeyo5kotdkq4qpitosoch4f64fiwv4uyrk7tp57w2ubthatq.b32.i2p, fc22dfljkpt2beq3itqjkz2k7oizuythif7aa3m73okq7mqrg66q.b32.i2p
```

---

### 9. Анализ работы с сетью I2P

#### Получение идентификатора и файл `i2p_iface_0.i2p`:
Reticulum взаимодействует с I2P через SAM API (порт `7656`). При запуске проверяется файл `.reticulum/storage/i2p/i2p_iface_0.i2p`. Если его нет, генерируется новый Destination (`DEST GENERATE`), записывается на диск, и вычисляется постоянный скрытый адрес `.b32.i2p`.

#### Мост между клирнетом и I2P:
Параметр `enable_transport = yes` превращает роутер в межсетевой шлюз. Пакеты, пришедшие из открытого интернета (порт 4242), пересылаются в скрытые туннели I2P и наоборот. Чтобы изолировать сети друг от друга:
* Выставьте `enable_transport = no` (режим только конечного узла), либо
* Добавьте параметр `mode = boundary` в секцию интерфейса I2P для запрета прохождения анонсов.

#### Публичные пиры (`peers`):
Адреса пиров в шаблоне - это проверенные общеизвестные публичные хабы сообщества, делиться ими безопасно. Однако не публикуйте в открытом доступе свой личный сгенерированный адрес `.b32.i2p`, если ваш узел предназначен для закрытого использования.

---

### 10. Концепция прогрева сети (Network Warm-up & Convergence)

Построение карты сети Reticulum происходит реактивно через анонсы (Announces). При холодном старте таблица пуста, а построение туннелей в сети I2P занимает несколько минут. Рекомендуется дать узлу **5–15 минут после старта** для завершения маршрутной сходимости перед активной передачей данных.

---

### 11. Сборка проекта (`build_mips_be.sh`) и флаги компилятора

Сборка полностью автоматизирована скриптом `build_mips_be.sh` на хосте **Linux x86_64**.

```bash
# Установка базовых инструментов на хосте Ubuntu/Debian
sudo apt update && sudo apt install -y git curl wget gcc make tar xz-utils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly

# Быстрая инкрементальная сборка (~2-3 секунды)
./build_mips_be.sh

# Сборка со стриппингом секций sstrip
./build_mips_be.sh --sstrip

# Полная зачистка и пересборка с нуля
./build_mips_be.sh --clean
```

---

### 12. Развертывание на роутере

```bash
# Копирование бинарника и конфигурации (пример с маскированием)
scp -P 63*** clevnode/clevnode admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/
scp -P 63*** clevnode/reticulum/config admin@192.168.5.***:/tmp/mnt/sda1/home/lblogd/.reticulum/config
```

Служба автозапуска `/opt/etc/init.d/S90clevnode`:
```sh
#!/bin/sh
ENABLED=yes
PROG="clevnode"
DIR="/tmp/mnt/sda1/home/lblogd"
BIN="$DIR/$PROG"
LOG="$DIR/clevnode.log"

start() {
    [ "$ENABLED" != "yes" ] && return 0
    pidof "$PROG" >/dev/null && return 0
    echo -n "Starting $PROG... "
    cd "$DIR" || exit 1
    "$BIN" > "$LOG" 2>&1 &
    sleep 1
    pidof "$PROG" >/dev/null && echo "OK" || echo "FAILED"
}

stop() {
    echo -n "Stopping $PROG... "
    killall "$PROG" 2>/dev/null
    for i in 1 2; do
        sleep 1
        ! pidof "$PROG" >/dev/null && echo "OK" && return 0
    done
    killall -9 "$PROG" 2>/dev/null
    echo "Force killed"
}

case "$1" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  pidof "$PROG" >/dev/null && echo "Running" || echo "Stopped" ;;
    *)       echo "Usage: $0 {start|stop|restart|status}"; exit 1 ;;
esac
```

---

### 13. Обслуживание хранилища (`.reticulum/storage`)

* **`transport_identity`:** Мастер-ключ узла. **Удалять запрещено.**
* **`packet_hashlist`:** Двухпоколенный кэш (~3.6 МБ). Самоочищается автоматически.
* **`known_destinations`:** База известных адресов. Если на носителе заканчивается место, файл можно безопасно удалить:
  ```sh
  /opt/etc/init.d/S90clevnode stop
  rm -f /tmp/mnt/sda1/home/lblogd/.reticulum/storage/known_destinations
  /opt/etc/init.d/S90clevnode start
  ```

---

### 14. Отказ от ответственности

Все приведенные пути к директориям, порты и имена интерфейсов являются индивидуальными примерами для тестового стенда. Информация предоставляется «как есть» исключительно для ознакомления и технических экспериментов.

---

### 15. Полезные ресурсы и документация

* [Онлайн-карта топологии Reticulum (Иван Сварковский)](https://reticulum-topology-map.ivansvarkovsky.workers.dev/)
* [Официальное руководство по интерфейсам Reticulum](https://reticulum.network/manual/interfaces.html) [1.1.2]
* [Сводная таблица реализаций Reticulum](https://reticulum.miraheze.org/wiki/Implementations)
* [Каталог проектов Awesome Reticulum](https://reticulum.miraheze.org/wiki/Awesome_Reticulum)
* [Документация по сети Reticulum на Dead.md](https://dead.md/docs/ham/reticulum-network) [1.4.3]
* [Установка Reticulum на примере MeshChat через Yggdrasil](https://devzone.org.ua/post/reticulum-vstanovlennia-na-prykladi-meshchat-z-pidkliuchenniam-cherez-yggdrasil) [1.1.2]
* [Установка I2P роутера через сеть Yggdrasil](https://devzone.org.ua/post/vstanovlennia-routera-i2p-z-pidkliuchenniam-cherez-yggdrasil) [1.2.1]
* [Официальный репозиторий Reticulum на GitHub](https://github.com/markqvist/reticulum)
* [Репозиторий ядра Leviculum на Codeberg](https://codeberg.org/Lew_Palm/Leviculum)
* [Спецификация протокола LXMF](https://github.com/markqvist/LXMF)
* [Клиент Sideband для LXMF](https://github.com/markqvist/sideband)
* [Статьи о Reticulum на DevZone](https://devzone.org.ua/topic/tag/reticulum)

---

<p align="center">
  <img src="https://github.com/user-attachments/assets/a1d02e81-4416-4552-ba2d-2a4eba0bba26" alt="clevnode" width="750">
</p>
