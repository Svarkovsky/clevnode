#!/usr/bin/env python3
"""
Minimal RNS daemon for Rust interop testing.

This daemon provides:
1. A standalone Reticulum instance with a TCPServerInterface
2. A JSON-RPC command interface for querying internal state

Usage:
    python3 test_daemon.py --rns-port 4242 --cmd-port 9999

The daemon will:
- Create a temporary config directory
- Start Reticulum in standalone mode (no shared instance)
- Listen for Reticulum packets on the RNS port
- Accept JSON-RPC commands on the command port

JSON-RPC methods:
- get_path_table: Returns all known paths
- get_announces: Returns pending announces
- has_path: Check if path exists for a destination hash
- get_interfaces: List active interfaces
- register_destination: Create a destination that accepts links
- get_destinations: List registered destinations
- enable_ratchets: Enable ratchets for a destination
- get_ratchet_info: Get ratchet state for a destination
- add_client_interface: Connect to another daemon's TCPServerInterface
- get_transport_status: Get transport/routing status
- get_link_table: Get link table for relay verification
- rotate_ratchet: Force ratchet rotation for a destination
- close_link: Gracefully close a link by its hash
- get_link_status: Get detailed status of a link
- wait_for_link_state: Wait for a link to reach a specific state
- get_announce_table_detail: Get full announce table entries with rebroadcast info
- get_reverse_table: Get reverse table entries for reverse-path routing
- get_path_request_info: Get discovery path request state
- trigger_path_request: Initiate a path request for a destination hash
- get_announce_cache: Get cached announces for path response verification
- shutdown: Gracefully stop the daemon
"""

import argparse
import json
import os
import socket
import sys
import tempfile
import threading
import time


def find_reticulum_path():
    """Find Reticulum in order: env var, vendor submodule, system."""
    script_dir = os.path.dirname(os.path.abspath(__file__))
    project_root = os.path.dirname(script_dir)

    candidates = [
        os.environ.get("RETICULUM_PATH"),  # 1. Explicit override
        os.path.join(project_root, "reference", "Reticulum"),  # 2. Reference submodule
    ]

    for path in candidates:
        if path and os.path.isdir(path) and os.path.exists(os.path.join(path, "RNS")):
            return path

    return None  # Fall back to system-installed


RETICULUM_PATH = find_reticulum_path()
if RETICULUM_PATH:
    sys.path.insert(0, RETICULUM_PATH)


def find_lxmf_path():
    """Locate the vendored LXMF (needed by the real InterfaceAnnouncer).

    On-network interface discovery (Codeberg #32) builds proof-of-work stamps
    with LXMF.LXStamper. RNS.Discovery imports LXMF at InterfaceAnnouncer /
    InterfaceAnnounceHandler construction time, so the module must be importable
    before Reticulum starts. Prefer the vendored copy so the daemon stays
    byte-for-byte pinned to the tree the Rust stack ports from.
    """
    script_dir = os.path.dirname(os.path.abspath(__file__))
    project_root = os.path.dirname(script_dir)
    candidates = [
        os.environ.get("LXMF_PATH"),
        os.path.join(project_root, "reference", "LXMF"),
    ]
    for path in candidates:
        if path and os.path.isdir(path) and os.path.exists(os.path.join(path, "LXMF")):
            return path
    return None


LXMF_PATH = find_lxmf_path()
if LXMF_PATH:
    sys.path.insert(0, LXMF_PATH)

try:
    import RNS
    import RNS.Channel
    from RNS import Transport
    from RNS.Interfaces.TCPInterface import TCPClientInterface
    from RNS.Interfaces.BackboneInterface import BackboneClientInterface
except ImportError:
    print("ERROR: Reticulum (RNS) not found.")
    print("Options:")
    print("  1. Run: git submodule update --init reference/Reticulum")
    print("  2. Set RETICULUM_PATH=/path/to/Reticulum")
    print("  3. Install system-wide: pip install rns")
    sys.exit(1)


class RawBytesMessage(RNS.Channel.MessageBase):
    """Channel message type compatible with Rust's RawBytesMessage (MSGTYPE=0x0000)."""
    MSGTYPE = 0x0000

    def __init__(self):
        self.data = b""

    def pack(self) -> bytes:
        return self.data

    def unpack(self, raw: bytes):
        self.data = raw


class TestDaemon:
    def __init__(self, rns_port: int, cmd_port: int, verbose: bool = False,
                 udp_listen_port: int = None, udp_forward_port: int = None,
                 auto_interface: bool = False, group_id: str = None,
                 share_instance: bool = False, instance_name: str = None,
                 echo_channel: bool = False, respond_to_probes: bool = False,
                 enable_remote_management: bool = False,
                 remote_management_allowed: list = None,
                 ifac_netname: str = None, ifac_passphrase: str = None,
                 ifac_size: int = None,
                 serial_kind: str = None, serial_port: str = None,
                 serial_speed: int = 9600, ax25_callsign: str = None,
                 ax25_ssid: int = None, pipe_command: str = None,
                 discoverable: bool = False, discovery_name: str = None,
                 discovery_stamp_value: int = None, discovery_encrypt: bool = False,
                 discovery_port: int = None, discover_interfaces: bool = False,
                 network_identity: str = None, discovery_job_interval: float = None,
                 discovery_publish_ifac: bool = False,
                 required_discovery_value: int = None):
        # Interface auto-discovery (Codeberg #32): drive the REAL Python
        # RNS.Discovery.InterfaceAnnouncer / InterfaceDiscovery. `discoverable`
        # marks a TCPServer interface discoverable so the daemon emits announces
        # on `rnstransport.discovery.interface`; `discovery_port`, when set, adds
        # a SECOND discoverable TCPServer on that port (the main server then acts
        # as a bootstrap link the peer connects to to hear the announce, while
        # the announce advertises the second port -- the endpoint an auto-connect
        # targets). `discover_interfaces` starts the listener so the daemon
        # populates its own discovered-interface registry (reverse direction).
        # `network_identity` points at a raw 64-byte identity file shared with
        # the peer for encrypted discovery.
        self.discoverable = discoverable
        self.discovery_name = discovery_name or "Test Discoverable"
        self.discovery_stamp_value = discovery_stamp_value
        self.discovery_encrypt = discovery_encrypt
        self.discovery_port = discovery_port
        self.discovery_publish_ifac = discovery_publish_ifac
        self.discover_interfaces = discover_interfaces
        # `required_discovery_value` is the LISTENER's stamp gate, the
        # `[reticulum]` key RNS feeds to InterfaceDiscovery(required_value=...).
        # RNS 1.5.0 changed only the DEFAULT this falls back to (14 -> 16); the
        # verification code either side of that default is identical, so setting
        # it to 16 on the vendored 1.3.5 gives a faithful 1.5.0-strength listener
        # (Codeberg #328).
        self.required_discovery_value = required_discovery_value
        self.network_identity = network_identity
        self.discovery_job_interval = discovery_job_interval
        # Serial-family interface (Codeberg #102): a KISSInterface,
        # AX25KISSInterface or PipeInterface driven over a socat pty pair (or,
        # for Pipe, a subprocess bridge command). Off by default; when set, the
        # matching block is appended to the generated config so a Rust lnsd on
        # the other pty end can exchange announces with this live Python peer.
        self.serial_kind = serial_kind
        self.serial_port = serial_port
        self.serial_speed = serial_speed
        self.ax25_callsign = ax25_callsign
        self.ax25_ssid = ax25_ssid
        self.pipe_command = pipe_command
        self.rns_port = rns_port
        self.ifac_netname = ifac_netname
        self.ifac_passphrase = ifac_passphrase
        self.ifac_size = ifac_size
        self.cmd_port = cmd_port
        self.udp_listen_port = udp_listen_port
        self.udp_forward_port = udp_forward_port
        self.auto_interface = auto_interface
        self.group_id = group_id or "reticulum"
        self.share_instance = share_instance
        self.instance_name = instance_name or "default"
        self.echo_channel = echo_channel
        self.respond_to_probes = respond_to_probes
        self.enable_remote_management = enable_remote_management
        self.remote_management_allowed = remote_management_allowed or []
        self.verbose = verbose
        self.running = True
        self.destinations = {}  # hash -> (identity, destination)
        self.links = {}  # link_hash -> link
        self.received_packets = []  # [(timestamp, link, data)]
        self.received_single_packets = []  # [(timestamp, dest_hash, data_hex)]
        # TCP load-test sink tally. Load-test single packets carry a
        # b"LT" + u32_le(client_id) + u32_le(seq) plaintext header. They are
        # NOT appended to received_single_packets (which would grow unbounded
        # under soak); instead we fold them into a per-client seq set so the
        # generator can assert exact, gap-free, dup-free delivery in O(sources).
        self.loadtest_seqs = {}  # client_id:int -> set(seq:int)
        self.plain_destinations = {}  # hash -> destination (PLAIN type, no identity)
        self.received_plain_packets = []  # [(timestamp, dest_hash, data_hex)]
        self.client_interfaces = {}  # name -> TCPClientInterface
        self.inbound_trace = []  # [(timestamp, flags, packet_type, dest_hash_hex, transport_id_hex)]
        self.received_resources = []  # [{resource_hash, data, metadata, status}]
        self.resource_strategies = {}  # dest_hash -> "accept_all" | "accept_none"
        # Tracked PacketReceipts from send_on_link_tracked (interop batch I1):
        # the Rust side polls get_receipt_status to observe DELIVERED/FAILED,
        # i.e. whether its data proof reached this Python peer's PacketReceipt.
        self.packet_receipts = {}  # receipt_id -> RNS.PacketReceipt
        self.next_receipt_id = 1
        # Tracked RequestReceipts from send_link_request (Python as Link.request
        # client against a Rust responder).
        self.request_receipts = {}  # request_id -> RNS.RequestReceipt
        self.next_request_id = 1
        # LXMF state (interop batch I1): one LXMRouter per daemon, driven by
        # the vendored reference/LXMF pinned at 1.1.0.
        self.lxmf_router = None
        self.lxmf_identity = None
        self.lxmf_dest = None
        self.lxmf_received = []  # [dict] recorded by _on_lxmf_delivery
        self.lxmf_sent = {}  # message_hash_hex -> LXMessage

        # Create temp config directory
        self.config_dir = tempfile.mkdtemp(prefix="rns_test_")
        self._write_config()

        if self.verbose:
            print(f"Config dir: {self.config_dir}")
            print(f"RNS port: {self.rns_port}")
            print(f"CMD port: {self.cmd_port}")
            if self.udp_listen_port:
                print(f"UDP listen port: {self.udp_listen_port}")
                print(f"UDP forward port: {self.udp_forward_port}")
            if self.auto_interface:
                print(f"AutoInterface enabled, group_id: {self.group_id}")
            if self.share_instance:
                print(f"Shared instance enabled, name: {self.instance_name}")

        # Initialize Reticulum in standalone mode
        loglevel = RNS.LOG_DEBUG if self.verbose else RNS.LOG_WARNING
        self.rns = RNS.Reticulum(
            configdir=self.config_dir,
            loglevel=loglevel
        )

        if self.verbose:
            print("Reticulum initialized")

        # Speed up the REAL InterfaceAnnouncer background job for tests. The
        # config-parsed announce interval has a 5-minute floor and the job
        # thread sleeps a full JOB_INTERVAL (60 s) between passes, so without
        # this a discovery announce is minutes away. Dropping the interface's
        # per-announce interval to 0 and the job interval to a few seconds makes
        # the real announcer fire promptly; `emit_discovery_announce` remains a
        # deterministic on-demand trigger driving the same announcer code.
        if self.discoverable and self.discovery_job_interval is not None:
            announcer = getattr(Transport, "interface_announcer", None)
            if announcer is not None:
                announcer.job_interval = self.discovery_job_interval
            for iface in Transport.interfaces:
                if getattr(iface, "discoverable", False):
                    iface.discovery_announce_interval = 0
                    iface.last_discovery_announce = 0

        # Print probe destination hash if respond_to_probes is enabled.
        # Reticulum auto-creates the probe destination when the config option
        # is set, so we read it from Transport.probe_destination directly.
        self.probe_dest_hash = None
        if self.respond_to_probes:
            probe_dest = getattr(Transport, 'probe_destination', None)
            if probe_dest is not None:
                self.probe_dest_hash = probe_dest.hash.hex()
                # Print before READY so the harness can parse it from stdout
                print(f"PROBE_DEST:{self.probe_dest_hash}", flush=True)

        # Start JSON-RPC command server
        self._start_cmd_server()

    def _page_content(self):
        """Deterministic content for the NomadNet-style page node.

        Fixed and reproducible so a client can assert byte-identity. `small`
        stays a few hundred bytes (well under the link MDU, so it comes back as
        a single RESPONSE packet); `large` exceeds the maximum negotiable link
        MDU so RNS always auto-upgrades the response to an is_response Resource.

        The size floor matters: over TCP, RNS link-MTU discovery raises the link
        MTU up to the interface HW_MTU (262144 B), so a "several KB" page still
        fits ONE packet and never becomes a resource. `large` is sized above
        that ceiling to force the resource path on any link. Cached on first use
        so every handler and `get_page_content` return the exact same bytes.
        """
        if not hasattr(self, "_page_content_cache"):
            small = b"`c`!Nomad small page`!\n\n" + bytes(i % 251 for i in range(200))
            large = b"`c`!Nomad large page`!\n\n" + bytes(i % 251 for i in range(300000))
            self._page_content_cache = {
                "/page/small.mu": small,
                "/page/large.mu": large,
            }
        return self._page_content_cache

    def _discovery_keys(self):
        """Discovery config keys appended to a discoverable TCPServer block.

        `reachable_on` is the address peers dial to auto-connect; `announce_interval`
        is in minutes and clamped by Reticulum to a 5-minute floor, so tests drive
        the announce out-of-band (short `discovery_job_interval` and/or the
        `emit_discovery_announce` RPC) rather than waiting for the cadence.
        """
        keys = """    discoverable = yes
    reachable_on = 127.0.0.1
    announce_interval = 5
"""
        keys += f"    discovery_name = {self.discovery_name}\n"
        if self.discovery_stamp_value is not None:
            keys += f"    discovery_stamp_value = {self.discovery_stamp_value}\n"
        if self.discovery_encrypt:
            keys += "    discovery_encrypt = yes\n"
        if self.discovery_publish_ifac:
            # Publish the block's network_name/passphrase in the discovery
            # announce (Reticulum.py `publish_ifac`, default off) -- Codeberg
            # #151: an auto-connecting peer derives the IFAC from the record.
            keys += "    publish_ifac = yes\n"
        return keys

    def _write_config(self):
        """Write minimal Reticulum config."""
        share = "yes" if self.share_instance else "no"
        probes = "yes" if self.respond_to_probes else "no"
        config = f"""[reticulum]
  enable_transport = yes
  share_instance = {share}
  respond_to_probes = {probes}
  panic_on_interface_error = no
"""
        if self.share_instance:
            config += f"  instance_name = {self.instance_name}\n"

        if self.enable_remote_management:
            config += "  enable_remote_management = yes\n"
            if self.remote_management_allowed:
                allowed = ", ".join(self.remote_management_allowed)
                config += f"  remote_management_allowed = {allowed}\n"

        # Interface discovery (Codeberg #32). A shared network_identity keys
        # encrypted discovery; discover_interfaces starts the listener that
        # populates the discovered-interface registry from received announces.
        if self.network_identity is not None:
            config += f"  network_identity = {self.network_identity}\n"
        if self.discover_interfaces:
            config += "  discover_interfaces = yes\n"
        if self.required_discovery_value is not None:
            config += f"  required_discovery_value = {self.required_discovery_value}\n"

        config += f"""
[interfaces]
  [[Test TCP Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 127.0.0.1
    listen_port = {self.rns_port}
    mode = gateway
"""
        # Make the main server discoverable in place unless a dedicated
        # discovery_port is requested (then the main server is a plain bootstrap
        # link and the second block below is the discoverable endpoint).
        if self.discoverable and self.discovery_port is None:
            config += self._discovery_keys()
        elif self.discoverable and self.discovery_port is not None:
            config += f"""
  [[Discoverable Backbone]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 127.0.0.1
    listen_port = {self.discovery_port}
    mode = gateway
"""
            config += self._discovery_keys()
        # IFAC (Interface Access Code) on the TCP server. Python derives the
        # ifac_identity/ifac_key natively from these keys (Reticulum.py:747-766,
        # 923-935), so this daemon is the byte-for-byte reference for the Rust
        # IFAC implementation.
        if self.ifac_netname is not None:
            config += f"    network_name = {self.ifac_netname}\n"
        if self.ifac_passphrase is not None:
            config += f"    passphrase = {self.ifac_passphrase}\n"
        if self.ifac_size is not None:
            config += f"    ifac_size = {self.ifac_size}\n"
        if self.udp_listen_port and self.udp_forward_port:
            config += f"""
  [[Test UDP Interface]]
    type = UDPInterface
    enabled = yes
    listen_ip = 127.0.0.1
    listen_port = {self.udp_listen_port}
    forward_ip = 127.0.0.1
    forward_port = {self.udp_forward_port}
"""

        if self.auto_interface:
            config += f"""
  [[Default Interface]]
    type = AutoInterface
    enabled = yes
    group_id = {self.group_id}
"""

        # Serial-family interface for the #102 socat interop tests. KISS and
        # AX25KISS point `port` at one pty end (the Rust lnsd holds the other);
        # Pipe spawns a bridge subprocess instead of touching a serial port.
        if self.serial_kind in ("kiss", "ax25kiss"):
            itype = "KISSInterface" if self.serial_kind == "kiss" else "AX25KISSInterface"
            config += f"""
  [[Test Serial]]
    type = {itype}
    enabled = yes
    port = {self.serial_port}
    speed = {self.serial_speed}
    databits = 8
    parity = none
    stopbits = 1
    flow_control = off
    preamble = 150
    txtail = 10
    persistence = 200
    slottime = 20
"""
            if self.serial_kind == "ax25kiss":
                config += f"    callsign = {self.ax25_callsign}\n"
                config += f"    ssid = {self.ax25_ssid}\n"
        elif self.serial_kind == "pipe":
            config += f"""
  [[Test Pipe]]
    type = PipeInterface
    enabled = yes
    command = {self.pipe_command}
    respawn_delay = 0.2
"""

        config_path = os.path.join(self.config_dir, "config")
        with open(config_path, 'w') as f:
            f.write(config)

    def _start_cmd_server(self):
        """Start the JSON-RPC command server in a separate thread.

        The requested cmd_port races with other parallel tests between
        find_*_available_ports() releasing its bind(0) discovery listener
        and the moment this bind() runs. When that race is lost, fall back
        to a kernel-allocated port: the harness reads the actually-bound
        port back from the READY line's third field. No retries on the
        requested port — if it's taken, another process holds it for the
        rest of the test and retrying wastes budget.
        """
        self.cmd_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.cmd_socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self.cmd_socket.bind(('127.0.0.1', self.cmd_port))
        except OSError:
            self.cmd_socket.bind(('127.0.0.1', 0))
            self.cmd_port = self.cmd_socket.getsockname()[1]
        self.cmd_socket.listen(5)
        self.cmd_socket.settimeout(1.0)

        self.cmd_thread = threading.Thread(target=self._cmd_server_loop, daemon=True)
        self.cmd_thread.start()

        if self.verbose:
            print(f"Command server listening on port {self.cmd_port}")

    def _cmd_server_loop(self):
        """Accept and handle command connections."""
        while self.running:
            try:
                conn, addr = self.cmd_socket.accept()
                threading.Thread(
                    target=self._handle_cmd_connection,
                    args=(conn,),
                    daemon=True
                ).start()
            except socket.timeout:
                continue
            except Exception as e:
                if self.running:
                    print(f"Command server error: {e}")

    def _handle_cmd_connection(self, conn):
        """Handle a single command connection."""
        try:
            conn.settimeout(5.0)
            data = b""
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
                # Try to parse complete JSON
                try:
                    cmd = json.loads(data.decode('utf-8'))
                    response = self._handle_cmd(cmd)
                    conn.sendall(json.dumps(response).encode('utf-8'))
                    break
                except json.JSONDecodeError:
                    # Need more data
                    continue
        except Exception as e:
            try:
                conn.sendall(json.dumps({"error": str(e)}).encode('utf-8'))
            except:
                pass
        finally:
            conn.close()

    def _handle_cmd(self, cmd):
        """Handle a JSON-RPC command."""
        method = cmd.get("method")
        params = cmd.get("params", {})

        if method == "ping":
            return {"result": "pong"}

        elif method == "get_path_table":
            # Return path table as hex strings
            paths = {}
            for h, entry in Transport.path_table.items():
                # entry = [timestamp, next_hop, hops, expires, ...]
                next_hop = entry[1].hex() if len(entry) > 1 and hasattr(entry[1], 'hex') else (entry[1] if len(entry) > 1 else None)
                paths[h.hex()] = {
                    "timestamp": entry[0],
                    "next_hop": next_hop,
                    "hops": entry[2] if len(entry) > 2 else None,
                    "expires": entry[3] if len(entry) > 3 else None,
                }
            return {"result": paths}

        elif method == "get_announces":
            # Return announce table
            announces = {}
            for h, entry in Transport.announce_table.items():
                # entry = [timestamp, retransmit_timeout, retries, ...]
                announces[h.hex()] = {
                    "timestamp": entry[0] if len(entry) > 0 else None,
                }
            return {"result": announces}

        elif method == "has_path":
            h = bytes.fromhex(params.get("hash", ""))
            return {"result": Transport.has_path(h)}

        elif method == "get_interfaces":
            interfaces = []
            for iface in Transport.interfaces:
                # Codeberg #87: expose the ingress-control held-announce queue
                # length and burst flag so interop tests can A/B the hold-and-
                # release behavior against the Rust stack. held_announces is a
                # dict keyed by destination hash (Interface.py:131).
                held = getattr(iface, 'held_announces', None)
                interfaces.append({
                    "name": str(iface),
                    "online": getattr(iface, 'online', None),
                    "IN": getattr(iface, 'IN', None),
                    "OUT": getattr(iface, 'OUT', None),
                    "txb": getattr(iface, 'txb', None),
                    "rxb": getattr(iface, 'rxb', None),
                    "mode": getattr(iface, 'mode', None),
                    "ifac_identity": hasattr(iface, 'ifac_identity') and iface.ifac_identity is not None,
                    "held_announces": len(held) if held is not None else 0,
                    "burst_active": bool(getattr(iface, 'ic_burst_active', False)),
                    # Codeberg #93: the interface's effective bitrate. A
                    # configured bitrate overrides the medium default here
                    # (Reticulum.py:887).
                    "bitrate": getattr(iface, 'bitrate', None),
                })
            return {"result": interfaces}

        elif method == "register_destination":
            # Create a destination that accepts links
            app_name = params.get("app_name", "test")
            aspects = params.get("aspects", [])

            identity = RNS.Identity()
            dest = RNS.Destination(
                identity,
                RNS.Destination.IN,
                RNS.Destination.SINGLE,
                app_name,
                *aspects
            )

            # Set up link acceptance
            dest.set_link_established_callback(
                lambda link, d=dest: self._on_link_established(link, d)
            )

            # Set up single-packet receive callback
            dest.set_packet_callback(
                lambda data, pkt, d=dest: self._on_single_packet(d, data, pkt)
            )

            dest_hash = dest.hash.hex()
            self.destinations[dest_hash] = (identity, dest)

            # Return destination info
            pub_key = identity.get_public_key()
            return {
                "result": {
                    "hash": dest_hash,
                    "public_key": pub_key.hex(),
                    "signing_key": pub_key[32:64].hex(),
                }
            }

        elif method == "verify_signature":
            # Verify an Ed25519 signature made elsewhere (e.g. the C API) using
            # the public key. public_key, message and signature are hex.
            public_key = bytes.fromhex(params.get("public_key", ""))
            message = bytes.fromhex(params.get("message", ""))
            signature = bytes.fromhex(params.get("signature", ""))
            identity = RNS.Identity(create_keys=False)
            identity.load_public_key(public_key)
            valid = bool(identity.validate(signature, message))
            return {"result": {"valid": valid}}

        elif method == "group_encrypt":
            # Encrypt plaintext with a GROUP destination holding a shared key.
            # Exercises the real Python GROUP branch: Destination.encrypt ->
            # self.prv.encrypt (RNS Token, AES-256-CBC). key/plaintext are hex.
            key = bytes.fromhex(params.get("key", ""))
            plaintext = bytes.fromhex(params.get("plaintext", ""))
            dest = RNS.Destination(
                None, RNS.Destination.IN, RNS.Destination.GROUP, "levgroup", "interop"
            )
            dest.load_private_key(key)
            ciphertext = dest.encrypt(plaintext)
            return {"result": {"ciphertext": ciphertext.hex()}}

        elif method == "group_decrypt":
            # Decrypt a GROUP token with a shared key via a Python GROUP
            # destination. key/ciphertext are hex. Returns plaintext hex, or
            # null if decryption failed (bad HMAC / wrong key).
            key = bytes.fromhex(params.get("key", ""))
            ciphertext = bytes.fromhex(params.get("ciphertext", ""))
            dest = RNS.Destination(
                None, RNS.Destination.IN, RNS.Destination.GROUP, "levgroup", "interop"
            )
            dest.load_private_key(key)
            plaintext = dest.decrypt(ciphertext)
            return {
                "result": {
                    "plaintext": plaintext.hex() if plaintext is not None else None
                }
            }

        elif method == "register_echo_request_handler":
            # Register an echo request handler on a destination
            dest_hash = params.get("dest_hash")
            path = params.get("path", "/echo")
            if dest_hash not in self.destinations:
                return {"error": f"destination {dest_hash} not found"}
            _identity, dest = self.destinations[dest_hash]
            def echo_request_handler(path, data, request_id, link_id, remote_identity, requested_at):
                """Echo handler for request/response interop testing."""
                return data  # Echo back whatever was sent
            dest.register_request_handler(
                path,
                response_generator=echo_request_handler,
                allow=RNS.Destination.ALLOW_ALL,
            )
            return {"result": "ok"}

        elif method == "register_page_request_handler":
            # Register a minimal NomadNet-style page node on a destination.
            #
            # NomadNet serves pages over RAW RNS request/response (not LXMF):
            # `destination.register_request_handler("/page/<name>.mu", ...)`
            # (nomadnet/Node.py). A response that fits one packet returns as a
            # single RESPONSE packet; a response larger than the link MDU is
            # auto-upgraded by RNS to a Resource carrying is_response=true and
            # the request_id (RNS.Link.handle_request). This handler mirrors that
            # for three fixed pages:
            #   /page/small.mu -> a few hundred bytes  (single-packet path)
            #   /page/large.mu -> several KB           (is_response Resource path)
            #   /page/echo.mu  -> echoes the request data (query-field round trip)
            dest_hash = params.get("dest_hash")
            if dest_hash not in self.destinations:
                return {"error": f"destination {dest_hash} not found"}
            _identity, dest = self.destinations[dest_hash]

            pages = self._page_content()

            def small_page_handler(path, data, request_id, link_id, remote_identity, requested_at):
                return pages["/page/small.mu"]

            def large_page_handler(path, data, request_id, link_id, remote_identity, requested_at):
                return pages["/page/large.mu"]

            def echo_page_handler(path, data, request_id, link_id, remote_identity, requested_at):
                # Return the request data verbatim so a client can assert its
                # query fields round-tripped. `data` is already the unpacked
                # request payload (bytes for a msgpack bin request value).
                return data

            dest.register_request_handler(
                "/page/small.mu",
                response_generator=small_page_handler,
                allow=RNS.Destination.ALLOW_ALL,
            )
            dest.register_request_handler(
                "/page/large.mu",
                response_generator=large_page_handler,
                allow=RNS.Destination.ALLOW_ALL,
            )
            dest.register_request_handler(
                "/page/echo.mu",
                response_generator=echo_page_handler,
                allow=RNS.Destination.ALLOW_ALL,
            )
            return {"result": {
                "small_len": len(pages["/page/small.mu"]),
                "large_len": len(pages["/page/large.mu"]),
            }}

        elif method == "get_page_content":
            # Return the exact bytes the page node serves for a given path, so a
            # client can assert byte-identity against what it fetched.
            path = params.get("path")
            pages = self._page_content()
            if path not in pages:
                return {"error": f"no page content for {path}"}
            return {"result": {"content": pages[path].hex()}}

        elif method == "get_destinations":
            dests = {}
            for h, (identity, dest) in self.destinations.items():
                dests[h] = {
                    "app_name": getattr(dest, 'app_name', None),
                }
            return {"result": dests}

        elif method == "get_links":
            links = {}
            for h, link in self.links.items():
                links[h] = {
                    "status": str(link.status),
                    "activated_at": getattr(link, 'activated_at', None),
                }
            return {"result": links}

        elif method == "get_link_remote_identity":
            link_hash = params.get("link_hash")
            link = self.links.get(link_hash)
            if not link:
                return {"error": f"link {link_hash} not found"}
            remote_id = link.get_remote_identity()
            if remote_id is None:
                return {"result": None}
            return {"result": {"identity_hash": remote_id.hash.hex()}}

        elif method == "get_received_packets":
            # Return packets received over links
            packets = []
            for ts, link, data in self.received_packets:
                packets.append({
                    "timestamp": ts,
                    "link_hash": link.hash.hex() if link.hash else None,
                    "data": data.hex() if isinstance(data, bytes) else str(data),
                })
            return {"result": packets}

        elif method == "set_resource_strategy":
            dest_hash = params.get("dest_hash")
            strategy = params.get("strategy", "accept_none")
            self.resource_strategies[dest_hash] = strategy
            return {"result": "ok"}

        elif method == "send_resource":
            link_hash = params.get("link_hash")
            data_hex = params.get("data")
            metadata_hex = params.get("metadata")

            link = self.links.get(link_hash)
            if not link:
                return {"error": f"link {link_hash} not found"}

            data = bytes.fromhex(data_hex)
            metadata = bytes.fromhex(metadata_hex) if metadata_hex else None

            resource = RNS.Resource(data, link, metadata=metadata, advertise=True)
            return {"result": {"resource_hash": resource.hash.hex()}}

        elif method == "get_received_resources":
            return {"result": self.received_resources}

        elif method == "announce_destination":
            dest_hash = params.get("hash")
            app_data = params.get("app_data", b"")
            if isinstance(app_data, str):
                app_data = bytes.fromhex(app_data)

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]
            dest.announce(app_data=app_data)
            return {"result": "announced"}

        elif method == "enable_ratchets":
            # Enable ratchets for a destination (forward secrecy)
            dest_hash = params.get("hash")

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]

            # Create ratchet storage path - RNS expects this to be a file path
            # (it writes to <path> and creates <path>.tmp during writes)
            ratchet_dir = os.path.join(self.config_dir, "ratchets")
            os.makedirs(ratchet_dir, exist_ok=True)
            ratchet_path = os.path.join(ratchet_dir, dest_hash)

            try:
                dest.enable_ratchets(ratchet_path)
                return {"result": {
                    "enabled": True,
                    "ratchet_dir": ratchet_path,
                }}
            except Exception as e:
                return {"error": f"Failed to enable ratchets: {str(e)}"}

        elif method == "enforce_ratchets":
            dest_hash = params.get("hash")
            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}
            _, dest = self.destinations[dest_hash]
            try:
                result = dest.enforce_ratchets()
                return {"result": {"enforced": result}}
            except Exception as e:
                return {"error": f"Failed to enforce ratchets: {str(e)}"}

        elif method == "get_ratchet_info":
            # Get ratchet state for a destination
            dest_hash = params.get("hash")

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]

            # Check if ratchets are enabled
            ratchets_enabled = hasattr(dest, 'ratchets') and dest.ratchets is not None

            result = {
                "enabled": ratchets_enabled,
            }

            if ratchets_enabled:
                # Get ratchet count and latest ID
                ratchet_count = len(dest.ratchets) if hasattr(dest.ratchets, '__len__') else 0
                latest_id = dest.latest_ratchet_id.hex() if hasattr(dest, 'latest_ratchet_id') and dest.latest_ratchet_id else None
                result["count"] = ratchet_count
                result["latest_id"] = latest_id

            return {"result": result}

        elif method == "create_link":
            # Create a link to an external destination (Python as initiator)
            dest_hash = params.get("dest_hash")
            dest_key = params.get("dest_key")  # 64-byte public key hex
            timeout = params.get("timeout", 10)

            if not dest_hash or not dest_key:
                return {"error": "dest_hash and dest_key required"}

            try:
                dest_hash_bytes = bytes.fromhex(dest_hash)
                dest_key_bytes = bytes.fromhex(dest_key)

                # Create identity from public key (no private key)
                dest_identity = RNS.Identity(create_keys=False)
                dest_identity.load_public_key(dest_key_bytes)

                # Create outgoing destination
                dest = RNS.Destination(
                    dest_identity,
                    RNS.Destination.OUT,
                    RNS.Destination.SINGLE,
                    "rust",
                    "interop"
                )

                # Override the hash (for testing - destination was registered externally)
                dest.hash = dest_hash_bytes

                # Create link with timeout
                link = RNS.Link(dest)

                # Wait for link establishment
                start = time.time()
                while link.status != RNS.Link.ACTIVE and time.time() - start < timeout:
                    time.sleep(0.1)
                    if link.status == RNS.Link.CLOSED:
                        return {"error": "Link closed before establishment"}

                if link.status != RNS.Link.ACTIVE:
                    return {"error": "Link establishment timeout"}

                link_hash = link.hash.hex() if link.hash else "unknown"
                self.links[link_hash] = link

                # Set up callbacks
                link.set_packet_callback(lambda msg, pkt, l=link: self._on_packet(l, msg, pkt))
                link.set_link_closed_callback(lambda l: self._on_link_closed(l))

                # Set up channel handler for Rust channel messages
                try:
                    channel = link.get_channel()
                    channel.register_message_type(RawBytesMessage)
                    channel.add_message_handler(lambda msg, l=link: self._on_channel_message(l, msg))
                except Exception as e:
                    if self.verbose:
                        print(f"Failed to set up channel handler on initiator link: {e}")

                # Resource callbacks for initiator-side links
                if self.resource_strategies.get(dest_hash) == "accept_all":
                    link.set_resource_strategy(RNS.Link.ACCEPT_ALL)
                    link.set_resource_started_callback(
                        lambda resource: self._on_resource_started(resource)
                    )
                    link.set_resource_concluded_callback(
                        lambda resource: self._on_resource_concluded(resource)
                    )

                return {
                    "result": {
                        "link_hash": link_hash,
                        "status": str(link.status),
                    }
                }

            except Exception as e:
                return {"error": f"Failed to create link: {str(e)}"}

        elif method == "identify_link":
            # Identify an identity on an existing link (Python as initiator).
            # Creates a fresh identity with private keys for signing.
            link_hash = params.get("link_hash")
            if not link_hash:
                return {"error": "link_hash required"}
            link = self.links.get(link_hash)
            if not link:
                return {"error": f"link {link_hash} not found"}
            try:
                identity = RNS.Identity()
                link.identify(identity)
                return {"result": {"identity_hash": identity.hash.hex()}}
            except Exception as e:
                return {"error": f"identify failed: {str(e)}"}

        elif method == "send_on_link":
            # Send data on an existing link
            link_hash = params.get("link_hash")
            data = params.get("data")  # hex string

            if not link_hash or not data:
                return {"error": "link_hash and data required"}

            if link_hash not in self.links:
                return {"error": f"Link {link_hash} not found"}

            link = self.links[link_hash]
            data_bytes = bytes.fromhex(data)

            try:
                packet = RNS.Packet(link, data_bytes)
                packet.send()
                return {"result": "sent"}
            except Exception as e:
                return {"error": f"Send failed: {str(e)}"}

        elif method == "add_client_interface":
            # Connect this daemon to another daemon's TCPServerInterface
            target_ip = params.get("target_ip", "127.0.0.1")
            target_port = params.get("target_port")
            name = params.get("name")

            if not target_port:
                return {"error": "target_port is required"}

            if not name:
                name = f"TCPClient_{target_ip}_{target_port}"

            try:
                # Create TCPClientInterface using configuration dict
                # The new RNS API expects a configuration object
                config = {
                    "name": name,
                    "target_host": target_ip,
                    "target_port": target_port,
                }

                client_iface = TCPClientInterface(
                    RNS.Transport,
                    config,
                )

                # Enable inbound and outbound on the interface.
                # TCPClientInterface defaults OUT=False, but Transport.outbound()
                # checks `if interface.OUT:` as the first gate for every outgoing
                # packet. Without this, no announces or data get sent through it.
                client_iface.OUT = True
                client_iface.IN = True
                client_iface.mode = RNS.Interfaces.Interface.Interface.MODE_GATEWAY

                # Set announce rate attributes required by Transport.inbound's
                # announce processing (Transport.py:1692). Without these,
                # incoming announces on this interface cause an AttributeError.
                # When announce_rate_target is supplied (Codeberg #92 interop),
                # apply Python's config coupling: a configured target defaults an
                # unset penalty/grace to 0 (Reticulum.py:813-817).
                ar_target = params.get("announce_rate_target")
                if ar_target is not None:
                    client_iface.announce_rate_target = ar_target
                    client_iface.announce_rate_grace = params.get(
                        "announce_rate_grace", 0)
                    client_iface.announce_rate_penalty = params.get(
                        "announce_rate_penalty", 0)
                else:
                    client_iface.announce_rate_target = None
                    client_iface.announce_rate_grace = None
                    client_iface.announce_rate_penalty = None

                # Codeberg #93: honour a configured bitrate exactly as Reticulum
                # applies configured_bitrate (Reticulum.py:794-796,887): the value
                # overrides the interface's medium default only when it clears
                # MINIMUM_BITRATE.
                cfg_bitrate = params.get("bitrate")
                if cfg_bitrate is not None and \
                        int(cfg_bitrate) >= RNS.Reticulum.MINIMUM_BITRATE:
                    client_iface.bitrate = int(cfg_bitrate)

                # Register with Transport
                RNS.Transport.interfaces.append(client_iface)

                # Store reference
                self.client_interfaces[name] = client_iface

                # Wait briefly for connection
                time.sleep(0.5)

                return {
                    "result": {
                        "name": name,
                        "online": getattr(client_iface, 'online', False),
                        "target_ip": target_ip,
                        "target_port": target_port,
                    }
                }

            except Exception as e:
                return {"error": f"Failed to create client interface: {str(e)}"}

        elif method == "add_backbone_client_interface":
            # Connect this daemon to another daemon's TCP listener using the REAL
            # Python BackboneClientInterface (Codeberg #89 drop-in proof). Backbone
            # is wire-identical to TCP, so a BackboneClientInterface connects to
            # our lnsd's TCP server built from a `type = BackboneInterface` config.
            target_ip = params.get("target_ip", "127.0.0.1")
            target_port = params.get("target_port")
            name = params.get("name")

            if not target_port:
                return {"error": "target_port is required"}

            if not name:
                name = f"BackboneClient_{target_ip}_{target_port}"

            try:
                config = {
                    "name": name,
                    "target_host": target_ip,
                    "target_port": target_port,
                }

                client_iface = BackboneClientInterface(
                    RNS.Transport,
                    config,
                )

                # BackboneClientInterface defaults OUT=False; Transport.outbound()
                # gates on interface.OUT, so enable it (mirrors add_client_interface).
                client_iface.OUT = True
                client_iface.IN = True
                client_iface.mode = RNS.Interfaces.Interface.Interface.MODE_GATEWAY

                client_iface.announce_rate_target = None
                client_iface.announce_rate_grace = None
                client_iface.announce_rate_penalty = None

                RNS.Transport.interfaces.append(client_iface)
                self.client_interfaces[name] = client_iface

                # Wait briefly for the connection to establish.
                time.sleep(0.5)

                return {
                    "result": {
                        "name": name,
                        "online": getattr(client_iface, 'online', False),
                        "target_ip": target_ip,
                        "target_port": target_port,
                    }
                }

            except Exception as e:
                return {"error": f"Failed to create backbone client interface: {str(e)}"}

        elif method == "get_transport_status":
            # Get transport/routing status
            try:
                result = {
                    "enabled": RNS.Reticulum.transport_enabled(),
                    "identity_hash": Transport.identity.hash.hex() if Transport.identity else None,
                    "path_table_size": len(Transport.path_table),
                    "link_table_size": len(Transport.link_table) if hasattr(Transport, 'link_table') else 0,
                    "announce_table_size": len(Transport.announce_table),
                    "interface_count": len(Transport.interfaces),
                }
                return {"result": result}
            except Exception as e:
                return {"error": f"Failed to get transport status: {str(e)}"}

        elif method == "get_tunnels":
            # Get the transport tunnel table (Codeberg #64). Each entry proves
            # Python validated a synthesize and (re)established the tunnel.
            # Structure: Transport.tunnels[tunnel_id] = [tunnel_id, interface,
            # paths(dict), expires] (IDX_TT_* in Transport.py).
            try:
                tunnels = {}
                for tunnel_id, entry in Transport.tunnels.items():
                    interface = entry[1] if len(entry) > 1 else None
                    paths = entry[2] if len(entry) > 2 else {}
                    tunnels[tunnel_id.hex()] = {
                        "interface": str(interface) if interface is not None else None,
                        "path_count": len(paths),
                        "expires": entry[3] if len(entry) > 3 else None,
                    }
                return {"result": tunnels}
            except Exception as e:
                return {"error": f"Failed to get tunnels: {str(e)}"}

        elif method == "get_link_table":
            # Get link table for relay verification
            try:
                link_table = {}
                if hasattr(Transport, 'link_table'):
                    for link_id, entry in Transport.link_table.items():
                        # link_table entry format: [timestamp, next_hop, outbound_interface, remaining_hops, ...]
                        link_table[link_id.hex()] = {
                            "timestamp": entry[0] if len(entry) > 0 else None,
                            "interface": str(entry[2]) if len(entry) > 2 else None,
                            "hops": entry[3] if len(entry) > 3 else None,
                        }
                return {"result": link_table}
            except Exception as e:
                return {"error": f"Failed to get link table: {str(e)}"}

        elif method == "enable_inbound_trace":
            # Install monkey-patch on Transport.inbound to trace all packets
            original_inbound = Transport.inbound.__func__ if hasattr(Transport.inbound, '__func__') else Transport.inbound
            daemon_ref = self

            @staticmethod
            def traced_inbound(raw, interface=None):
                try:
                    if len(raw) > 2:
                        flags = raw[0]
                        packet_type = flags & 0x03
                        header_type = (flags >> 6) & 0x01
                        context_flag = (flags >> 5) & 0x01
                        dest_type = (flags >> 2) & 0x03
                        transport_id_hex = None
                        context_val = None
                        if header_type == 1:  # HEADER_2
                            transport_id_hex = raw[2:18].hex()
                            dest_hash_hex = raw[18:34].hex()
                            if len(raw) > 34:
                                context_val = raw[34]
                        else:  # HEADER_1
                            dest_hash_hex = raw[2:18].hex()
                            if len(raw) > 18:
                                context_val = raw[18]
                        # Compute packet hash (same algorithm as Packet.get_hash)
                        import hashlib
                        if header_type == 1:  # HEADER_2
                            hashable = bytes([flags & 0x0F]) + raw[18:]
                        else:  # HEADER_1
                            hashable = bytes([flags & 0x0F]) + raw[2:]
                        full_hash = hashlib.sha256(hashable).digest()
                        computed_hash_hex = full_hash[:16].hex()
                        daemon_ref.inbound_trace.append({
                            "time": time.time(),
                            "flags": f"0x{flags:02x}",
                            "header_type": header_type,
                            "packet_type": packet_type,
                            "dest_type": dest_type,
                            "context_flag": context_flag,
                            "context_val": context_val,
                            "dest_hash": dest_hash_hex,
                            "transport_id": transport_id_hex,
                            "interface": str(interface),
                            "raw_len": len(raw),
                            "hops": raw[1] if len(raw) > 1 else None,
                            "computed_packet_hash": computed_hash_hex,
                            "hashable_hex": hashable.hex(),
                            "raw_head_hex": raw[:20].hex() if len(raw) >= 20 else raw.hex(),
                        })
                except Exception as e:
                    daemon_ref.inbound_trace.append({"error": str(e)})
                return original_inbound(raw, interface=interface)

            Transport.inbound = traced_inbound
            return {"result": "trace enabled"}

        elif method == "enable_lrproof_trace":
            # Detailed tracing of LRPROOF handling inside Transport
            daemon_ref = self
            daemon_ref.lrproof_trace = []

            # Monkey-patch Transport.transmit to trace outgoing packets
            original_transmit = Transport.transmit.__func__ if hasattr(Transport.transmit, '__func__') else Transport.transmit
            @staticmethod
            def traced_transmit(interface, raw):
                try:
                    if len(raw) > 2:
                        flags = raw[0]
                        pt = flags & 0x03
                        daemon_ref.lrproof_trace.append({
                            "event": "transmit",
                            "interface": str(interface),
                            "flags": f"0x{flags:02x}",
                            "packet_type": pt,
                            "raw_len": len(raw),
                            "dest_hash": raw[2:18].hex() if (flags >> 6) & 0x01 == 0 else raw[18:34].hex(),
                        })
                except Exception as e:
                    daemon_ref.lrproof_trace.append({"event": "transmit_error", "error": str(e)})
                return original_transmit(interface, raw)
            Transport.transmit = traced_transmit

            # Log link table and path table state
            daemon_ref.lrproof_trace.append({
                "event": "tables_snapshot",
                "link_table_keys": [k.hex() if isinstance(k, bytes) else str(k) for k in Transport.link_table.keys()],
                "path_table_keys": [k.hex() if isinstance(k, bytes) else str(k) for k in Transport.path_table.keys()],
                "transport_enabled": RNS.Reticulum.transport_enabled(),
                "transport_identity": Transport.identity.hash.hex() if Transport.identity else None,
            })

            return {"result": "lrproof trace enabled"}

        elif method == "get_lrproof_trace":
            return {"result": getattr(self, 'lrproof_trace', [])}

        elif method == "enable_lrproof_drop":
            # Monkey-patch Transport.transmit to silently drop LRPROOF packets.
            # LRPROOF has context byte 0xFF. Context byte offset depends on
            # header type: HEADER_1 → raw[18], HEADER_2 → raw[34].
            daemon_ref = self
            daemon_ref.lrproof_drops = getattr(daemon_ref, 'lrproof_drops', [])

            # Save original transmit only once (avoid double-patching)
            if not hasattr(daemon_ref, '_original_transmit'):
                daemon_ref._original_transmit = (
                    Transport.transmit.__func__
                    if hasattr(Transport.transmit, '__func__')
                    else Transport.transmit
                )

            original = daemon_ref._original_transmit

            @staticmethod
            def dropping_transmit(interface, raw):
                try:
                    if len(raw) > 2:
                        flags = raw[0]
                        is_header_2 = (flags >> 6) & 0x01
                        context_offset = 34 if is_header_2 else 18
                        if len(raw) > context_offset and raw[context_offset] == 0xFF:
                            daemon_ref.lrproof_drops.append({
                                "time": time.time(),
                                "interface": str(interface),
                                "raw_len": len(raw),
                                "header_type": 2 if is_header_2 else 1,
                            })
                            return  # silently drop
                except Exception:
                    pass  # on error, fall through to original
                return original(interface, raw)

            Transport.transmit = dropping_transmit
            return {"result": "lrproof_drop_enabled"}

        elif method == "disable_lrproof_drop":
            # Restore original Transport.transmit saved during enable.
            if hasattr(self, '_original_transmit'):
                Transport.transmit = self._original_transmit
                del self._original_transmit
                return {"result": "lrproof_drop_disabled"}
            else:
                return {"result": "lrproof_drop_was_not_enabled"}

        elif method == "get_lrproof_drops":
            return {"result": getattr(self, 'lrproof_drops', [])}

        elif method == "get_link_table_detail":
            # Get full link table with all fields
            result = {}
            for k, v in Transport.link_table.items():
                key_hex = k.hex() if isinstance(k, bytes) else str(k)
                result[key_hex] = {
                    "timestamp": v[0],
                    "next_hop": v[1].hex() if isinstance(v[1], bytes) else str(v[1]),
                    "nh_interface": str(v[2]),
                    "remaining_hops": v[3],
                    "rcvd_interface": str(v[4]),
                    "taken_hops": v[5],
                    "dest_hash": v[6].hex() if isinstance(v[6], bytes) else str(v[6]),
                    "validated": v[7],
                    "proof_timeout": v[8],
                }
            return {"result": result}

        elif method == "get_inbound_trace":
            return {"result": self.inbound_trace}

        elif method == "rotate_ratchet":
            # Force ratchet rotation for a destination
            dest_hash = params.get("hash")

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]

            # Check if ratchets are enabled
            if not hasattr(dest, 'ratchets') or dest.ratchets is None:
                return {"error": "Ratchets not enabled for this destination"}

            try:
                # Rotate ratchet - this generates a new ratchet key pair
                dest.rotate_ratchets()

                # Get new state
                ratchet_count = len(dest.ratchets) if hasattr(dest.ratchets, '__len__') else 0
                latest_id = dest.latest_ratchet_id.hex() if hasattr(dest, 'latest_ratchet_id') and dest.latest_ratchet_id else None

                return {
                    "result": {
                        "rotated": True,
                        "ratchet_count": ratchet_count,
                        "new_ratchet_id": latest_id,
                    }
                }
            except Exception as e:
                return {"error": f"Failed to rotate ratchet: {str(e)}"}

        elif method == "close_link":
            # Gracefully close a link by its hash
            link_hash = params.get("link_hash")
            if not link_hash:
                return {"error": "link_hash is required"}

            if link_hash not in self.links:
                return {"error": f"Link {link_hash} not found", "status": "not_found"}

            link = self.links[link_hash]
            try:
                link.teardown()
                return {"result": {"status": "closed", "link_hash": link_hash}}
            except Exception as e:
                return {"error": f"Failed to close link: {str(e)}"}

        elif method == "get_link_status":
            # Get detailed status of a link
            link_hash = params.get("link_hash")
            if not link_hash:
                return {"error": "link_hash is required"}

            if link_hash not in self.links:
                return {"result": {"status": "not_found", "link_hash": link_hash}}

            link = self.links[link_hash]
            return {
                "result": {
                    "status": "found",
                    "link_hash": link_hash,
                    "state": str(link.status),
                    "is_initiator": getattr(link, 'initiator', None),
                    "rtt": getattr(link, 'rtt', None),
                    "established_at": getattr(link, 'activated_at', None),
                    "last_inbound": getattr(link, 'last_inbound', None),
                    "last_outbound": getattr(link, 'last_outbound', None),
                    "mtu": link.get_mtu() if hasattr(link, 'get_mtu') else None,
                    "mdu": link.get_mdu() if hasattr(link, 'get_mdu') else None,
                }
            }

        elif method == "wait_for_link_state":
            # Wait for a link to reach a specific state
            link_hash = params.get("link_hash")
            expected_state = params.get("state")
            timeout_secs = params.get("timeout", 10)

            if not link_hash or not expected_state:
                return {"error": "link_hash and state are required"}

            deadline = time.time() + timeout_secs

            while time.time() < deadline:
                if link_hash in self.links:
                    link = self.links[link_hash]
                    current_state = str(link.status)
                    if current_state == expected_state:
                        return {"result": {"status": "reached", "state": expected_state}}
                    # Also check for CLOSED state (link may be removed from dict)
                    if link.status == RNS.Link.CLOSED and expected_state == "CLOSED":
                        return {"result": {"status": "reached", "state": expected_state}}
                else:
                    # Link removed from dict means it's closed
                    if expected_state == "CLOSED":
                        return {"result": {"status": "reached", "state": expected_state}}

                time.sleep(0.1)

            # Timeout - return current state if available
            current = None
            if link_hash in self.links:
                current = str(self.links[link_hash].status)

            return {
                "result": {
                    "status": "timeout",
                    "expected": expected_state,
                    "current": current
                }
            }

        elif method == "set_proof_strategy":
            # Set proof strategy for a destination
            dest_hash = params.get("hash")
            strategy = params.get("strategy")

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]

            strategy_map = {
                "PROVE_NONE": RNS.Destination.PROVE_NONE,
                "PROVE_APP": RNS.Destination.PROVE_APP,
                "PROVE_ALL": RNS.Destination.PROVE_ALL,
            }

            if strategy not in strategy_map:
                return {"error": f"Unknown strategy: {strategy}. Use PROVE_NONE, PROVE_APP, or PROVE_ALL"}

            try:
                dest.set_proof_strategy(strategy_map[strategy])
                return {"result": {"strategy": strategy, "dest_hash": dest_hash}}
            except Exception as e:
                return {"error": f"Failed to set proof strategy: {str(e)}"}

        elif method == "get_proof_strategy":
            # Get proof strategy for a destination
            dest_hash = params.get("hash")

            if dest_hash not in self.destinations:
                return {"error": f"Destination {dest_hash} not registered"}

            _, dest = self.destinations[dest_hash]

            strategy_names = {
                RNS.Destination.PROVE_NONE: "PROVE_NONE",
                RNS.Destination.PROVE_APP: "PROVE_APP",
                RNS.Destination.PROVE_ALL: "PROVE_ALL",
            }

            return {"result": {
                "strategy": strategy_names.get(dest.proof_strategy, "unknown"),
                "strategy_value": dest.proof_strategy,
            }}

        elif method == "get_announce_table_detail":
            # Return full announce_table entries with rebroadcast info
            # Python format: announce_table[dest_hash] = [timestamp, retransmit_timeout,
            #   retries, received_from, announce_hops, packet, local_rebroadcasts,
            #   block_rebroadcasts, attached_interface]
            try:
                table = {}
                for h, entry in Transport.announce_table.items():
                    detail = {
                        "timestamp": entry[0] if len(entry) > 0 else None,
                        "retransmit_timeout": entry[1] if len(entry) > 1 else None,
                        "retries": entry[2] if len(entry) > 2 else None,
                        "received_from": entry[3].hex() if len(entry) > 3 and entry[3] is not None else None,
                        "hops": entry[4] if len(entry) > 4 else None,
                        "packet_length": len(entry[5].raw) if len(entry) > 5 and entry[5] is not None and hasattr(entry[5], 'raw') else None,
                        "local_rebroadcasts": entry[6] if len(entry) > 6 else None,
                        "block_rebroadcasts": entry[7] if len(entry) > 7 else None,
                    }
                    table[h.hex()] = detail
                return {"result": table}
            except Exception as e:
                return {"error": f"Failed to get announce table detail: {str(e)}"}

        elif method == "get_reverse_table":
            # Return reverse_table entries for reverse-path routing verification
            # Python format: reverse_table[dest_hash] = [received_from, outbound_interface,
            #   timestamp, ...]
            try:
                table = {}
                if hasattr(Transport, 'reverse_table'):
                    for h, entry in Transport.reverse_table.items():
                        detail = {
                            "received_from": entry[0].hex() if len(entry) > 0 and entry[0] is not None else None,
                            "interface": str(entry[1]) if len(entry) > 1 and entry[1] is not None else None,
                            "timestamp": entry[2] if len(entry) > 2 else None,
                        }
                        table[h.hex()] = detail
                return {"result": table}
            except Exception as e:
                return {"error": f"Failed to get reverse table: {str(e)}"}

        elif method == "get_path_request_info":
            # Return discovery_path_requests state
            try:
                requests = {}
                if hasattr(Transport, 'discovery_path_requests'):
                    for h, entry in Transport.discovery_path_requests.items():
                        detail = {
                            "timeout": entry[0] if len(entry) > 0 else None,
                            "request_tag": entry[1].hex() if len(entry) > 1 and entry[1] is not None else None,
                        }
                        requests[h.hex()] = detail
                return {"result": requests}
            except Exception as e:
                return {"error": f"Failed to get path request info: {str(e)}"}

        elif method == "trigger_path_request":
            # Initiate a path request for a given destination hash
            dest_hash = params.get("hash")
            if not dest_hash:
                return {"error": "hash is required"}
            try:
                dest_hash_bytes = bytes.fromhex(dest_hash)
                Transport.request_path(dest_hash_bytes)
                return {"result": "path_request_sent"}
            except Exception as e:
                return {"error": f"Failed to trigger path request: {str(e)}"}

        elif method == "get_announce_cache":
            # Return cached announces (destination_table) for path response verification
            try:
                cache = {}
                if hasattr(Transport, 'destination_table'):
                    for h, entry in Transport.destination_table.items():
                        detail = {
                            "timestamp": entry[0] if len(entry) > 0 else None,
                            "received_from": entry[1].hex() if len(entry) > 1 and entry[1] is not None else None,
                            "hops": entry[2] if len(entry) > 2 else None,
                            "expires": entry[3] if len(entry) > 3 else None,
                            "random_blobs": [b.hex() for b in entry[4]] if len(entry) > 4 and entry[4] is not None else None,
                        }
                        cache[h.hex()] = detail
                return {"result": cache}
            except Exception as e:
                return {"error": f"Failed to get announce cache: {str(e)}"}

        elif method == "get_received_single_packets":
            # Return single packets received at destinations (not via links)
            packets = []
            for ts, dest_hash, data_hex in self.received_single_packets:
                packets.append({
                    "timestamp": ts,
                    "dest_hash": dest_hash,
                    "data": data_hex,
                })
            return {"result": packets}

        elif method == "send_single_packet":
            # Send a single (non-link) packet to a remote destination
            dest_hash_hex = params.get("dest_hash")
            data_hex = params.get("data")

            if not dest_hash_hex or not data_hex:
                return {"error": "dest_hash and data required"}

            try:
                dest_hash_bytes = bytes.fromhex(dest_hash_hex)

                # Look up identity from RNS identity cache (populated by announces)
                recalled_identity = RNS.Identity.recall(dest_hash_bytes)
                if recalled_identity is None:
                    return {"error": f"No identity known for {dest_hash_hex} (no announce received?)"}

                # Create Destination.OUT with the recalled identity
                # We need app_name + aspects to match the dest hash, but for OUT
                # destinations we can just override the hash directly
                out_dest = RNS.Destination(
                    recalled_identity,
                    RNS.Destination.OUT,
                    RNS.Destination.SINGLE,
                    "unknown",
                    "app"
                )
                out_dest.hash = dest_hash_bytes

                # Build and send the packet
                data_bytes = bytes.fromhex(data_hex)
                packet = RNS.Packet(out_dest, data_bytes)
                packet.send()

                return {"result": "sent"}
            except Exception as e:
                return {"error": f"Failed to send single packet: {str(e)}"}

        elif method == "register_plain_destination":
            # Create a PLAIN destination (no identity, unencrypted broadcast)
            app_name = params.get("app_name", "test")
            aspects = params.get("aspects", [])

            dest = RNS.Destination(
                None,
                RNS.Destination.IN,
                RNS.Destination.PLAIN,
                app_name,
                *aspects
            )

            dest.set_packet_callback(
                lambda data, pkt, d=dest: self._on_plain_packet(d, data, pkt)
            )

            dest_hash = dest.hash.hex()
            self.plain_destinations[dest_hash] = dest

            return {
                "result": {
                    "hash": dest_hash,
                }
            }

        elif method == "send_plain_packet":
            # Send a plain broadcast packet (unencrypted, no identity)
            app_name = params.get("app_name", "test")
            aspects = params.get("aspects", [])
            data_hex = params.get("data", "")

            try:
                dest = RNS.Destination(
                    None,
                    RNS.Destination.OUT,
                    RNS.Destination.PLAIN,
                    app_name,
                    *aspects
                )

                data_bytes = bytes.fromhex(data_hex)
                packet = RNS.Packet(dest, data_bytes)
                packet.send()

                return {"result": {"sent": True, "hash": dest.hash.hex()}}
            except Exception as e:
                return {"error": f"Failed to send plain packet: {str(e)}"}

        elif method == "get_received_plain_packets":
            packets = []
            for ts, dest_hash, data_hex in self.received_plain_packets:
                packets.append({
                    "timestamp": ts,
                    "dest_hash": dest_hash,
                    "data": data_hex,
                })
            return {"result": packets}

        elif method == "get_loadtest_stats":
            # Per-client delivery tally for the TCP load test. For every source
            # client_id we return the number of distinct seqs received and their
            # min/max, so the generator can assert count==distinct==sent,
            # min==0, max==sent-1 (contiguous, no gaps, no duplicates). "total"
            # is the grand sum of distinct seqs across all sources.
            stats = {}
            total = 0
            for client_id, seqs in self.loadtest_seqs.items():
                distinct = len(seqs)
                total += distinct
                stats[str(client_id)] = {
                    "distinct": distinct,
                    "min": min(seqs) if seqs else None,
                    "max": max(seqs) if seqs else None,
                }
            return {"result": {"sources": stats, "total": total}}

        elif method == "emit_discovery_announce":
            # Drive the REAL InterfaceAnnouncer to emit one interface-discovery
            # announce immediately (Codeberg #32), bypassing the minutes-long
            # cadence. Uses the announcer's own get_interface_announce_data
            # (real PoW stamp via LXMF.LXStamper, real network-identity
            # encryption) and its discovery_destination.announce -- the same
            # code the background job runs.
            announcer = getattr(Transport, "interface_announcer", None)
            if announcer is None:
                return {"error": "InterfaceAnnouncer not running (daemon not discoverable?)"}
            disc = [i for i in Transport.interfaces
                    if getattr(i, "supports_discovery", False)
                    and getattr(i, "discoverable", False)]
            if not disc:
                return {"error": "no discoverable interface registered"}
            iface = disc[0]
            try:
                app_data = announcer.get_interface_announce_data(iface)
            except Exception as e:
                return {"error": f"get_interface_announce_data failed: {e}"}
            if not app_data:
                return {"error": "get_interface_announce_data returned no data"}
            announcer.discovery_destination.announce(app_data=app_data)
            return {"result": {
                "announced": True,
                "app_data_len": len(app_data),
                "discovery_dest_hash": announcer.discovery_destination.hash.hex(),
                "transport_id": Transport.identity.hash.hex() if Transport.identity else None,
                "network_id": announcer.discovery_destination.identity.hash.hex(),
                "bind_port": getattr(iface, "bind_port", None),
            }}

        elif method == "get_discovered_interfaces":
            # Return this daemon's own discovered-interface registry (the
            # `rnstatus -d` view), reading the shared storage via the real
            # RNS.Discovery.InterfaceDiscovery. Bytes fields are dropped/hex'd
            # so the result is JSON-safe.
            try:
                infos = RNS.Reticulum.discovered_interfaces()
            except Exception as e:
                return {"error": f"discovered_interfaces failed: {e}"}
            out = []
            for info in infos:
                out.append({
                    "type": info.get("type"),
                    "name": info.get("name"),
                    "transport_id": info.get("transport_id"),
                    "network_id": info.get("network_id"),
                    "reachable_on": info.get("reachable_on"),
                    "port": info.get("port"),
                    "value": info.get("value"),
                    "hops": info.get("hops"),
                    "status": info.get("status"),
                    "config_entry": info.get("config_entry"),
                })
            return {"result": out}

        elif method == "send_on_link_tracked":
            # Send RNS.Packet(link, data) and keep the PacketReceipt so the
            # peer's data proof can be observed via get_receipt_status.
            link_hash = params.get("link_hash")
            data = params.get("data")
            if not link_hash or data is None:
                return {"error": "link_hash and data required"}
            link = self.links.get(link_hash)
            if link is None:
                return {"error": f"Link {link_hash} not found"}
            try:
                packet = RNS.Packet(link, bytes.fromhex(data))
                receipt = packet.send()
                if receipt is None:
                    return {"error": "packet.send() returned no receipt"}
                receipt_id = str(self.next_receipt_id)
                self.next_receipt_id += 1
                self.packet_receipts[receipt_id] = receipt
                return {"result": {
                    "receipt_id": receipt_id,
                    "packet_hash": receipt.hash.hex(),
                }}
            except Exception as e:
                return {"error": f"send failed: {str(e)}"}

        elif method == "get_receipt_status":
            receipt_id = params.get("receipt_id")
            receipt = self.packet_receipts.get(receipt_id)
            if receipt is None:
                return {"error": f"receipt {receipt_id} not found"}
            # Transport's cull job drives timeouts on its own cadence; check
            # explicitly so a FAILED verdict is observable promptly.
            try:
                receipt.check_timeout()
            except Exception:
                pass
            names = {
                RNS.PacketReceipt.FAILED: "FAILED",
                RNS.PacketReceipt.SENT: "SENT",
                RNS.PacketReceipt.DELIVERED: "DELIVERED",
                RNS.PacketReceipt.CULLED: "CULLED",
            }
            status = receipt.get_status()
            return {"result": {"status": names.get(status, str(status))}}

        elif method == "send_link_request":
            # Python-side RNS.Link.request() against a (Rust) responder.
            # Non-blocking: returns a request id; poll get_request_status.
            link_hash = params.get("link_hash")
            path = params.get("path")
            data = params.get("data")  # hex, packed with umsgpack as bytes
            timeout = params.get("timeout")  # seconds or None
            if not link_hash or not path:
                return {"error": "link_hash and path required"}
            link = self.links.get(link_hash)
            if link is None:
                return {"error": f"Link {link_hash} not found"}
            try:
                receipt = link.request(
                    path,
                    data=bytes.fromhex(data) if data else None,
                    timeout=timeout,
                )
                if receipt is None:
                    return {"error": "link.request() returned no receipt"}
                request_id = str(self.next_request_id)
                self.next_request_id += 1
                self.request_receipts[request_id] = receipt
                return {"result": {"request_id": request_id}}
            except Exception as e:
                return {"error": f"request failed: {str(e)}"}

        elif method == "get_request_status":
            request_id = params.get("request_id")
            receipt = self.request_receipts.get(request_id)
            if receipt is None:
                return {"error": f"request {request_id} not found"}
            names = {
                RNS.RequestReceipt.FAILED: "FAILED",
                RNS.RequestReceipt.SENT: "SENT",
                RNS.RequestReceipt.DELIVERED: "DELIVERED",
                RNS.RequestReceipt.RECEIVING: "RECEIVING",
                RNS.RequestReceipt.READY: "READY",
            }
            status = receipt.get_status()
            response_hex = None
            if status == RNS.RequestReceipt.READY:
                response = receipt.get_response()
                if isinstance(response, bytes):
                    response_hex = response.hex()
                elif response is not None:
                    from RNS.vendor import umsgpack
                    response_hex = umsgpack.packb(response).hex()
            return {"result": {
                "status": names.get(status, str(status)),
                "response": response_hex,
            }}

        elif method == "lxmf_init":
            # Real LXMF client: LXMRouter + delivery identity/destination from
            # the vendored reference/LXMF (pinned 1.1.0).
            import LXMF
            if self.lxmf_router is not None:
                return {"error": "lxmf already initialized"}
            display_name = params.get("display_name", "test-daemon")
            stamp_cost = params.get("stamp_cost")  # None or int
            storage = os.path.join(self.config_dir, "lxmf")
            os.makedirs(storage, exist_ok=True)
            self.lxmf_identity = RNS.Identity()
            # LXMRouter.__init__ installs SIGINT/SIGTERM handlers
            # (LXMRouter.py:308-309), which only works on the main thread;
            # this RPC handler runs on a worker thread, so shim signal.signal
            # for the constructor call. The daemon manages shutdown itself.
            import signal as signal_module
            original_signal = signal_module.signal
            signal_module.signal = lambda *args, **kwargs: None
            try:
                self.lxmf_router = LXMF.LXMRouter(
                    identity=self.lxmf_identity, storagepath=storage
                )
            finally:
                signal_module.signal = original_signal
            self.lxmf_dest = self.lxmf_router.register_delivery_identity(
                self.lxmf_identity,
                display_name=display_name,
                stamp_cost=stamp_cost,
            )
            self.lxmf_router.register_delivery_callback(self._on_lxmf_delivery)
            self._install_receipt_probe()
            return {"result": {
                "delivery_hash": self.lxmf_dest.hash.hex(),
                "public_key": self.lxmf_identity.get_public_key().hex(),
            }}

        elif method == "lxmf_announce":
            if self.lxmf_router is None:
                return {"error": "lxmf not initialized"}
            self.lxmf_router.announce(self.lxmf_dest.hash)
            return {"result": "ok"}

        elif method == "lxmf_send":
            # Build and hand a real LXMessage to LXMRouter.handle_outbound.
            import LXMF
            if self.lxmf_router is None:
                return {"error": "lxmf not initialized"}
            dest_hash_hex = params.get("dest_hash")
            method_name = params.get("method", "direct")
            content = bytes.fromhex(params.get("content", ""))
            title = bytes.fromhex(params.get("title", ""))
            fields_hex = params.get("fields")  # msgpack map, hex, or None
            dest_hash_bytes = bytes.fromhex(dest_hash_hex)
            recalled = RNS.Identity.recall(dest_hash_bytes)
            if recalled is None:
                return {"error": f"no identity known for {dest_hash_hex}"}
            dest_out = RNS.Destination(
                recalled, RNS.Destination.OUT, RNS.Destination.SINGLE,
                "lxmf", "delivery",
            )
            methods = {
                "opportunistic": LXMF.LXMessage.OPPORTUNISTIC,
                "direct": LXMF.LXMessage.DIRECT,
                "propagated": LXMF.LXMessage.PROPAGATED,
            }
            if method_name not in methods:
                return {"error": f"unknown method {method_name}"}
            fields = None
            if fields_hex:
                from RNS.vendor import umsgpack
                fields = umsgpack.unpackb(bytes.fromhex(fields_hex))
            try:
                message = self._probe_lxmessage_class(LXMF)(
                    dest_out,
                    self.lxmf_dest,
                    content=content,
                    title=title,
                    fields=fields,
                    desired_method=methods[method_name],
                )
                self.lxmf_router.handle_outbound(message)
                message_hash = message.hash.hex()
                self.lxmf_sent[message_hash] = message
                return {"result": {"message_hash": message_hash}}
            except Exception as e:
                return {"error": f"lxmf_send failed: {str(e)}"}

        elif method == "lxmf_get_outbound_status":
            import LXMF
            message = self.lxmf_sent.get(params.get("message_hash"))
            if message is None:
                return {"error": "message not tracked"}
            names = self._lxm_state_names(LXMF)
            return {"result": {"state": names.get(message.state, str(message.state))}}

        elif method == "lxmf_get_outbound_probe":
            # Codeberg #156 latency probe: state plus every state transition
            # the message went through, wall-clock timestamped (epoch seconds).
            import LXMF
            message = self.lxmf_sent.get(params.get("message_hash"))
            if message is None:
                return {"error": "message not tracked"}
            names = self._lxm_state_names(LXMF)
            transitions = [
                {"t": t, "state": names.get(s, str(s))}
                for (t, s) in getattr(message, "probe_state_times", [])
            ]
            return {"result": {
                "state": names.get(message.state, str(message.state)),
                "transitions": transitions,
                "receipt_events": getattr(self, "probe_receipt_events", []),
                "now": time.time(),
            }}

        elif method == "lxmf_get_received":
            return {"result": self.lxmf_received}

        elif method == "lxmf_enable_propagation":
            # The lxmd propagation node in-process: lxmd wires an LXMRouter and
            # calls enable_propagation() (LXMF/Utilities/lxmd.py:444-452); this
            # drives the identical router code path on this daemon's router.
            if self.lxmf_router is None:
                return {"error": "lxmf not initialized"}
            self.lxmf_router.set_message_storage_limit(megabytes=5)
            self.lxmf_router.enable_propagation()
            return {"result": {
                "propagation_hash": self.lxmf_router.propagation_destination.hash.hex(),
            }}

        elif method == "lxmf_announce_propagation_node":
            if self.lxmf_router is None:
                return {"error": "lxmf not initialized"}
            self.lxmf_router.announce_propagation_node()
            return {"result": "ok"}

        elif method == "lxmf_set_propagation_node":
            if self.lxmf_router is None:
                return {"error": "lxmf not initialized"}
            dest_hash_bytes = bytes.fromhex(params.get("dest_hash"))
            if RNS.Identity.recall(dest_hash_bytes) is None:
                return {"error": "propagation node identity not known yet"}
            self.lxmf_router.set_outbound_propagation_node(dest_hash_bytes)
            return {"result": "ok"}

        elif method == "shutdown":
            self.running = False
            return {"result": "shutting_down"}

        else:
            return {"error": f"Unknown method: {method}"}

    def _install_receipt_probe(self):
        """Codeberg #156 latency probe: timestamp PacketReceipt proof
        validation and delivery-callback registration. LXMF arms the
        callback only after Packet.send() returns (LXMessage.py:469), so a
        proof that comes back faster than the sender thread gets rescheduled
        is validated with no callback armed - and set_delivery_callback does
        not fire retroactively. These events make that race observable."""
        if hasattr(self, "_probe_receipt_patch"):
            return
        self.probe_receipt_events = []
        daemon = self
        orig_validate = RNS.PacketReceipt.validate_proof
        def probed_validate(receipt, proof, proof_packet=None):
            had_callback = receipt.callbacks.delivery is not None
            result = orig_validate(receipt, proof, proof_packet)
            daemon.probe_receipt_events.append({
                "ev": "validate_proof", "t": time.time(),
                "hash": receipt.hash.hex(),
                "valid": bool(result),
                "had_callback": had_callback,
            })
            return result
        RNS.PacketReceipt.validate_proof = probed_validate
        orig_set_cb = RNS.PacketReceipt.set_delivery_callback
        def probed_set_cb(receipt, callback):
            daemon.probe_receipt_events.append({
                "ev": "set_delivery_callback", "t": time.time(),
                "hash": receipt.hash.hex(),
                "already_proved": bool(receipt.proved),
                "sent_at": receipt.sent_at,
                "timeout": receipt.timeout,
            })
            return orig_set_cb(receipt, callback)
        RNS.PacketReceipt.set_delivery_callback = probed_set_cb
        self._probe_receipt_patch = True

    def _lxm_state_names(self, LXMF):
        """LXMessage state constant -> name, shared by the status/probe RPCs."""
        return {
            LXMF.LXMessage.GENERATING: "GENERATING",
            LXMF.LXMessage.OUTBOUND: "OUTBOUND",
            LXMF.LXMessage.SENDING: "SENDING",
            LXMF.LXMessage.SENT: "SENT",
            LXMF.LXMessage.DELIVERED: "DELIVERED",
            LXMF.LXMessage.REJECTED: "REJECTED",
            LXMF.LXMessage.CANCELLED: "CANCELLED",
            LXMF.LXMessage.FAILED: "FAILED",
        }

    def _probe_lxmessage_class(self, LXMF):
        """Codeberg #156 latency probe: an LXMessage that timestamps every
        state transition (OUTBOUND -> SENDING -> SENT -> DELIVERED ...) so the
        interop test can attribute proof-round-trip latency to a segment.
        Wire behaviour is identical to the base class."""
        if not hasattr(self, "_probe_lxm_cls"):
            class ProbeLXMessage(LXMF.LXMessage):
                def __setattr__(self, name, value):
                    if name == "state":
                        self.__dict__.setdefault("probe_state_times", []).append(
                            (time.time(), value)
                        )
                    super().__setattr__(name, value)
            self._probe_lxm_cls = ProbeLXMessage
        return self._probe_lxm_cls

    def _on_lxmf_delivery(self, message):
        """LXMRouter delivery callback: record every asserted LXMessage field."""
        try:
            import LXMF
            from RNS.vendor import umsgpack
            methods = {0x01: "opportunistic", 0x02: "direct", 0x03: "propagated"}
            # What a client shows next to the message: the display name from
            # the last announce heard from this sender, the way Sideband and
            # Nomadnet resolve it. None until an announce has been heard --
            # which, for a message whose signature validated, it has been,
            # since validation recalls the same known_destinations entry.
            source_display_name = LXMF.display_name_from_app_data(
                RNS.Identity.recall_app_data(message.source_hash)
            )
            self.lxmf_received.append({
                "source_display_name": source_display_name,
                "message_hash": message.hash.hex(),
                "content": message.content.hex(),
                "title": message.title.hex(),
                "fields": umsgpack.packb(message.fields).hex(),
                "source_hash": message.source_hash.hex(),
                "destination_hash": message.destination_hash.hex(),
                "timestamp": message.timestamp,
                "signature_validated": bool(message.signature_validated),
                "stamp_valid": message.stamp_valid,
                "stamp_value": message.stamp_value,
                "method": methods.get(message.method, str(message.method)),
            })
        except Exception as e:
            # Surface callback bugs to the polling test instead of dying in
            # LXMRouter's silent callback exception handler.
            self.lxmf_received.append({"message_hash": f"callback-error: {e}"})
        if self.verbose:
            print(f"LXMF delivery: {message.hash.hex()}")

    def _on_single_packet(self, dest, data, packet):
        """Called when a single (non-link) packet is received at a destination."""
        if self.verbose:
            print(f"Single packet received at {dest.hash.hex()}: {len(data)} bytes")

        raw = data if isinstance(data, bytes) else data.encode()
        # TCP load-test packets: b"LT" + u32_le(client_id) + u32_le(seq).
        # Fold into the per-client seq set instead of the unbounded list.
        if len(raw) >= 10 and raw[0:2] == b"LT":
            client_id = int.from_bytes(raw[2:6], "little")
            seq = int.from_bytes(raw[6:10], "little")
            self.loadtest_seqs.setdefault(client_id, set()).add(seq)
            return

        self.received_single_packets.append((
            time.time(),
            dest.hash.hex(),
            data.hex() if isinstance(data, bytes) else data.encode().hex(),
        ))

    def _on_plain_packet(self, dest, data, packet):
        """Called when a plain broadcast packet is received."""
        if self.verbose:
            print(f"Plain packet received at {dest.hash.hex()}: {len(data)} bytes")

        self.received_plain_packets.append((
            time.time(),
            dest.hash.hex(),
            data.hex() if isinstance(data, bytes) else data.encode().hex(),
        ))

    def _on_link_established(self, link, dest):
        """Called when a link is established to one of our destinations."""
        if self.verbose:
            print(f"Link established: {link.hash.hex() if link.hash else 'unknown'}")

        link_hash = link.hash.hex() if link.hash else "unknown"
        self.links[link_hash] = link

        link.set_packet_callback(lambda msg, pkt: self._on_packet(link, msg, pkt))
        link.set_link_closed_callback(lambda l: self._on_link_closed(l))

        # Set up channel handler for Rust channel messages (RawBytesMessage)
        try:
            channel = link.get_channel()
            channel.register_message_type(RawBytesMessage)
            channel.add_message_handler(lambda msg: self._on_channel_message(link, msg))
        except Exception as e:
            if self.verbose:
                print(f"Failed to set up channel handler: {e}")

        # Resource callbacks: if this destination has accept_all strategy, set up
        # ACCEPT_ALL and concluded callback on the link
        dest_hash = dest.hash.hex()
        if self.resource_strategies.get(dest_hash) == "accept_all":
            link.set_resource_strategy(RNS.Link.ACCEPT_ALL)
            link.set_resource_started_callback(
                lambda resource: self._on_resource_started(resource)
            )
            link.set_resource_concluded_callback(
                lambda resource: self._on_resource_concluded(resource)
            )

    def _on_resource_started(self, resource):
        """Called when a resource transfer starts."""
        if self.verbose:
            print(f"Resource transfer started: {resource.hash.hex()}")

    def _on_resource_concluded(self, resource):
        """Called when a resource transfer concludes (complete or failed)."""
        if self.verbose:
            print(f"Resource concluded: {resource.hash.hex()}, status={resource.status}")
        if resource.status == RNS.Resource.COMPLETE:
            data = resource.data.read() if hasattr(resource.data, 'read') else bytes(resource.data)
            # resource.metadata is the DECODED value (after umsgpack.unpackb).
            # For bytes metadata, .hex() gives hex of the raw bytes.
            meta = None
            if resource.metadata is not None:
                if isinstance(resource.metadata, bytes):
                    meta = resource.metadata.hex()
                else:
                    # Non-bytes metadata (dict, string, etc) — encode back for RPC
                    from RNS.vendor import umsgpack
                    meta = umsgpack.packb(resource.metadata).hex()
            self.received_resources.append({
                "resource_hash": resource.hash.hex(),
                "data": data.hex(),
                "metadata": meta,
                "status": "complete",
            })
        else:
            self.received_resources.append({
                "resource_hash": resource.hash.hex() if resource.hash else "unknown",
                "data": "",
                "metadata": None,
                "status": str(resource.status),
            })

    def _on_channel_message(self, link, message):
        """Called when a channel message is received over a link."""
        if self.verbose:
            print(f"Channel message received: {message.data}")

        self.received_packets.append((time.time(), link, message.data))

        # Echo back via channel (only when --echo-channel is set)
        if self.echo_channel:
            try:
                echo = RawBytesMessage()
                echo.data = message.data
                link.get_channel().send(echo)
            except Exception as e:
                if self.verbose:
                    print(f"Failed to echo channel message: {e}")

        return True

    def _on_packet(self, link, message, packet):
        """Called when a packet is received over a link."""
        if self.verbose:
            print(f"Packet received: {message}")

        self.received_packets.append((time.time(), link, message))

        # Echo back
        if isinstance(message, str):
            message = message.encode('utf-8')
        RNS.Packet(link, message).send()

    def _on_link_closed(self, link):
        """Called when a link is closed."""
        if self.verbose:
            print(f"Link closed: {link.hash.hex() if link.hash else 'unknown'}")

        link_hash = link.hash.hex() if link.hash else None
        if link_hash and link_hash in self.links:
            del self.links[link_hash]

    def run(self):
        """Run the daemon until shutdown."""
        # Signal readiness by printing to stdout
        extra = f" {self.instance_name}" if self.share_instance else ""
        print(f"READY {self.rns_port} {self.cmd_port}{extra}", flush=True)

        try:
            while self.running:
                time.sleep(0.5)
        except KeyboardInterrupt:
            pass
        finally:
            self._cleanup()

    def _cleanup(self):
        """Clean up resources."""
        self.running = False
        try:
            self.cmd_socket.close()
        except:
            pass

        # Clean up temp directory
        try:
            import shutil
            shutil.rmtree(self.config_dir, ignore_errors=True)
        except:
            pass


def main():
    parser = argparse.ArgumentParser(description="RNS test daemon for Rust interop")
    parser.add_argument("--rns-port", type=int, required=True,
                        help="Port for Reticulum TCP interface")
    parser.add_argument("--cmd-port", type=int, required=True,
                        help="Port for JSON-RPC command interface")
    parser.add_argument("--udp-listen-port", type=int, default=None,
                        help="Port for UDP interface (listen)")
    parser.add_argument("--udp-forward-port", type=int, default=None,
                        help="Port for UDP interface (forward)")
    parser.add_argument("--auto-interface", action="store_true",
                        help="Enable AutoInterface for LAN discovery")
    parser.add_argument("--group-id", type=str, default=None,
                        help="Group ID for AutoInterface (default: reticulum)")
    parser.add_argument("--share-instance", action="store_true",
                        help="Enable shared instance (local Unix socket)")
    parser.add_argument("--instance-name", type=str, default=None,
                        help="Instance name for shared instance (default: default)")
    parser.add_argument("--echo-channel", action="store_true",
                        help="Echo received channel messages back via channel")
    parser.add_argument("--respond-to-probes", action="store_true",
                        help="Enable respond_to_probes (prints PROBE_DEST:<hex> on startup)")
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="Enable verbose output")
    parser.add_argument("--mgmt-announce-interval-seconds", type=int, default=None,
                        help="Override Transport.mgmt_announce_interval for keepalive tests")
    parser.add_argument("--enable-remote-management", action="store_true",
                        help="Enable remote management (rnstatus -R) destination")
    parser.add_argument("--remote-management-allowed", type=str, action="append",
                        default=None,
                        help="Hex identity hash allowed to remote-manage (repeatable)")
    parser.add_argument("--ifac-netname", type=str, default=None,
                        help="IFAC network_name for the TCP server interface")
    parser.add_argument("--ifac-passphrase", type=str, default=None,
                        help="IFAC passphrase for the TCP server interface")
    parser.add_argument("--ifac-size", type=int, default=None,
                        help="IFAC size in bits for the TCP server interface")
    parser.add_argument("--serial-kind", type=str, default=None,
                        choices=["kiss", "ax25kiss", "pipe"],
                        help="Serial-family interface for #102 socat interop")
    parser.add_argument("--serial-port", type=str, default=None,
                        help="pty path for the kiss/ax25kiss serial interface")
    parser.add_argument("--serial-speed", type=int, default=9600,
                        help="Baud rate for the kiss/ax25kiss serial interface")
    parser.add_argument("--ax25-callsign", type=str, default=None,
                        help="Source callsign for the ax25kiss interface")
    parser.add_argument("--ax25-ssid", type=int, default=None,
                        help="Source SSID for the ax25kiss interface")
    parser.add_argument("--pipe-command", type=str, default=None,
                        help="Bridge command for the pipe interface")
    parser.add_argument("--discoverable", action="store_true",
                        help="Mark a TCPServer interface discoverable (#32)")
    parser.add_argument("--discovery-name", type=str, default=None,
                        help="Discovery name advertised for the interface")
    parser.add_argument("--discovery-stamp-value", type=int, default=None,
                        help="Required PoW stamp value for the discovery announce")
    parser.add_argument("--discovery-encrypt", action="store_true",
                        help="Encrypt discovery announces with the network identity")
    parser.add_argument("--discovery-port", type=int, default=None,
                        help="Add a second discoverable TCPServer on this port "
                             "(main server becomes a plain bootstrap link)")
    parser.add_argument("--discover-interfaces", action="store_true",
                        help="Start the InterfaceDiscovery listener (reverse direction)")
    parser.add_argument("--network-identity", type=str, default=None,
                        help="Path to a shared 64-byte network identity file")
    parser.add_argument("--discovery-job-interval", type=float, default=None,
                        help="Reduce the InterfaceAnnouncer job interval (seconds)")
    parser.add_argument("--discovery-publish-ifac", action="store_true",
                        help="Publish the discoverable interface's IFAC keys "
                             "in the discovery announce (#151)")
    parser.add_argument("--required-discovery-value", type=int, default=None,
                        help="Stamp value the InterfaceDiscovery listener requires "
                             "(RNS 1.5.0 defaults to 16, 1.3.5 to 14)")

    args = parser.parse_args()

    # Apply mgmt_announce_interval override BEFORE Transport.start by setting
    # the class attribute. Transport reads it at check time, so subsequent
    # firings use the reduced interval. Production default stays 2 h.
    if args.mgmt_announce_interval_seconds is not None:
        Transport.mgmt_announce_interval = args.mgmt_announce_interval_seconds

    daemon = TestDaemon(
        rns_port=args.rns_port,
        cmd_port=args.cmd_port,
        verbose=args.verbose,
        udp_listen_port=args.udp_listen_port,
        udp_forward_port=args.udp_forward_port,
        auto_interface=args.auto_interface,
        group_id=args.group_id,
        share_instance=args.share_instance,
        instance_name=args.instance_name,
        echo_channel=args.echo_channel,
        respond_to_probes=args.respond_to_probes,
        enable_remote_management=args.enable_remote_management,
        remote_management_allowed=args.remote_management_allowed,
        ifac_netname=args.ifac_netname,
        ifac_passphrase=args.ifac_passphrase,
        ifac_size=args.ifac_size,
        serial_kind=args.serial_kind,
        serial_port=args.serial_port,
        serial_speed=args.serial_speed,
        ax25_callsign=args.ax25_callsign,
        ax25_ssid=args.ax25_ssid,
        pipe_command=args.pipe_command,
        discoverable=args.discoverable,
        discovery_name=args.discovery_name,
        discovery_stamp_value=args.discovery_stamp_value,
        discovery_encrypt=args.discovery_encrypt,
        discovery_port=args.discovery_port,
        discover_interfaces=args.discover_interfaces,
        network_identity=args.network_identity,
        discovery_job_interval=args.discovery_job_interval,
        discovery_publish_ifac=args.discovery_publish_ifac,
        required_discovery_value=args.required_discovery_value,
    )
    daemon.run()


if __name__ == "__main__":
    main()
