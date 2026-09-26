//! Minimal INI parser for Python Reticulum's ConfigObj format
//!
//! Handles:
//! - `[section]` headers
//! - `[[subsection]]` headers (ConfigObj-style nested sections under `[interfaces]`)
//! - `key = value` pairs (whitespace-stripped)
//! - `#` comments
//! - Boolean parsing: Yes/yes/True/true/1 → true, No/no/False/false/0 → false
//!
//! Only TCP interfaces are supported; unknown types are logged and skipped.

use std::collections::HashMap;

use crate::config::{Config, InterfaceConfig, ReticulumConfig, SubinterfaceConfig};

/// Parse a Python Reticulum INI config string into our `Config` struct.
pub(crate) fn parse_ini(content: &str) -> Result<Config, String> {
    let mut reticulum = ReticulumConfig::default();
    let mut interfaces: HashMap<String, InterfaceConfig> = HashMap::new();

    let mut current_section = String::new();
    let mut current_subsection: Option<String> = None;
    let mut current_iface: Option<(String, InterfaceConfig)> = None;
    // A nested `[[[name]]]` block: the vport subinterface of an
    // `RNodeMultiInterface`. Flushed into the parent interface's
    // `subinterfaces` when the next header (of any depth) appears.
    let mut current_subinterface: Option<SubinterfaceConfig> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Sub-subsection header: [[[name]]] (must check BEFORE [[..]], since a
        // triple bracket also matches the double-bracket test). This is an
        // RNodeMultiInterface subinterface, nested inside the current [[..]].
        if trimmed.starts_with("[[[") && trimmed.ends_with("]]]") {
            // Flush a previous subinterface into its parent interface.
            flush_subinterface(&mut current_subinterface, &mut current_iface);

            let name = trimmed[3..trimmed.len() - 3].trim().to_string();
            current_subinterface = Some(SubinterfaceConfig {
                name,
                ..Default::default()
            });
            continue;
        }

        // Subsection header: [[name]] (must check before section)
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            // Flush any pending subinterface, then the previous interface.
            flush_subinterface(&mut current_subinterface, &mut current_iface);
            if let Some((name, iface)) = current_iface.take() {
                interfaces.insert(name, iface);
            }

            let name = trimmed[2..trimmed.len() - 2].trim().to_string();
            current_subsection = Some(name.clone());
            current_iface = Some((
                name,
                InterfaceConfig {
                    interface_type: String::new(),
                    ..Default::default()
                },
            ));
            continue;
        }

        // Section header: [name]
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Flush any pending subinterface, then the previous interface.
            flush_subinterface(&mut current_subinterface, &mut current_iface);
            if let Some((name, iface)) = current_iface.take() {
                interfaces.insert(name, iface);
            }
            current_section = trimmed[1..trimmed.len() - 1].trim().to_string();
            current_subsection = None;
            continue;
        }

        // Key = value
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            // ConfigObj value semantics: strip an inline ` #` comment and a
            // single pair of surrounding quotes (Item 4). `strip_value` trims.
            let value = strip_value(value);
            let value = value.as_str();

            if let Some(ref mut sub) = current_subinterface {
                // Inside a [[[subinterface]]] block.
                apply_subinterface_key(sub, key, value);
            } else if current_subsection.is_some() {
                // Inside an interface subsection
                if let Some((_, ref mut iface)) = current_iface {
                    apply_interface_key(iface, key, value);
                }
            } else {
                // Inside a top-level section
                match current_section.as_str() {
                    "reticulum" => apply_reticulum_key(&mut reticulum, key, value),
                    "logging" => apply_logging_key(&mut reticulum, key, value),
                    _ => {}
                }
            }
        }
    }

    // Flush the last subinterface, then the last interface.
    flush_subinterface(&mut current_subinterface, &mut current_iface);
    if let Some((name, iface)) = current_iface.take() {
        interfaces.insert(name, iface);
    }

    // Normalize Backbone interfaces onto our TCP interface. BackboneInterface is
    // wire-identical to TCPInterface (HDLC-over-TCP), so we map the config the
    // same way Python does (Reticulum.py:960-972) and let the rest of the stack
    // treat it as a TCPServer/TCPClient. Done post-parse so key order in the file
    // does not matter -- mirrors ConfigObj handing Python the whole section dict.
    for iface in interfaces.values_mut() {
        normalize_backbone_interface(iface);
    }

    // IFAC keys (network_name/networkname/passphrase/pass_phrase/ifac_size) are
    // now enforced by lnsd (Codeberg #90): the driver derives an IfacConfig and
    // the transport applies the HMAC on TX and verifies + drops on RX. A VALID
    // IFAC config must therefore NOT warn -- it is authenticated on the air.
    //
    // Two cases still deserve a warning:
    //   1. IFAC is requested (network_name and/or passphrase) but the derived
    //      IfacConfig would be INVALID (e.g. an out-of-range ifac_size), which
    //      would silently leave the interface unauthenticated.
    //   2. `ifac_size` is set with NEITHER network_name nor passphrase. Python
    //      needs a netname or netkey to enable IFAC (Reticulum.py:923-926), so
    //      a lone ifac_size is a no-op -- flag the likely misconfiguration.
    for (name, iface) in interfaces.iter() {
        let has_ident = iface.networkname.is_some() || iface.passphrase.is_some();
        if has_ident {
            let size = iface
                .ifac_size
                .unwrap_or(match iface.interface_type.as_str() {
                    "RNodeInterface"
                    | "RNodeMultiInterface"
                    | "SerialInterface"
                    | "PipeInterface"
                    | "KISSInterface"
                    | "AX25KISSInterface" => 8,
                    _ => 16,
                });
            if leviculum_core::ifac::IfacConfig::new(
                iface.networkname.as_deref(),
                iface.passphrase.as_deref(),
                size,
            )
            .is_err()
            {
                tracing::warn!(
                    "interface '{}': IFAC (network_name/passphrase) is configured but the \
                     derived access code is INVALID (check ifac_size) -- this interface will \
                     run UNAUTHENTICATED and cannot join the IFAC-protected network.",
                    name
                );
            }
        } else if iface.ifac_size.is_some() {
            tracing::warn!(
                "interface '{}': ifac_size is set without network_name or passphrase -- IFAC \
                 needs one of those to be enabled, so ifac_size alone has no effect.",
                name
            );
        }
    }

    // RNS 1.3.x semantic: shared_instance_type = tcp disables AF_UNIX and
    // therefore overrides any configured shared_instance_socket path (tcp
    // wins on conflict). Applied here, post-parse, so it holds for any key
    // ordering in the file.
    if reticulum.shared_instance_type.as_deref() == Some("tcp") {
        reticulum.shared_instance_socket = None;
    }

    // Carry the section name into the interface itself: it is Python's
    // `interface.name` and part of every interface's reported identity
    // (Codeberg #177), and the map key alone is lost the moment the
    // interfaces are flattened for the driver.
    for (name, iface) in interfaces.iter_mut() {
        iface.name = name.clone();
    }

    // Filter out unsupported interface types
    let supported: HashMap<String, InterfaceConfig> = interfaces
        .into_iter()
        .filter(|(name, iface)| match iface.interface_type.as_str() {
            "TCPServerInterface"
            | "TCPClientInterface"
            | "UDPInterface"
            | "AutoInterface"
            | "RNodeInterface"
            | "RNodeMultiInterface"
            | "SerialInterface"
            | "PipeInterface"
            | "KISSInterface"
            | "AX25KISSInterface"
            | "I2PInterface"
            | "BLEInterface" => true,
            other => {
                tracing::warn!(
                    "Skipping unsupported interface type '{}' for '{}'",
                    other,
                    name
                );
                false
            }
        })
        .collect();

    Ok(Config {
        reticulum,
        interfaces: supported,
    })
}

fn apply_reticulum_key(config: &mut ReticulumConfig, key: &str, value: &str) {
    match key {
        "enable_transport" => {
            config.enable_transport = parse_bool(value);
        }
        "share_instance" => {
            config.shared_instance = parse_bool(value);
        }
        "instance_name" => {
            config.instance_name = value.trim().to_string();
        }
        "shared_instance_type" => {
            // Upstream only honours tcp/unix (lowercased); other values
            // are tolerated but not stored. The tcp-overrides-socket rule
            // is applied after the full parse (see parse_ini), so it holds
            // regardless of key order.
            let v = value.trim().to_ascii_lowercase();
            if v == "tcp" || v == "unix" {
                config.shared_instance_type = Some(v);
            }
        }
        "shared_instance_socket" => {
            config.shared_instance_socket = Some(value.trim().to_string());
        }
        // TCP-loopback ports for the shared-instance data + RPC channels
        // (Python casts `int(value)`, Reticulum.py:501-507). Only the AF_INET
        // path binds these (`shared_instance_type = tcp`, or Windows); on the
        // default AF_UNIX path the sockets are keyed by `instance_name` and the
        // ports are unused, matching Python. A non-numeric or out-of-u16-range
        // value is left unset rather than aborting the parse.
        "shared_instance_port" => {
            config.shared_instance_port = value.trim().parse().ok();
        }
        "instance_control_port" => {
            config.instance_control_port = value.trim().parse().ok();
        }
        "respond_to_probes" => {
            config.respond_to_probes = parse_bool(value);
        }
        // Whether links prove implicitly (RNS default true, Reticulum.py:575-578).
        // Filled on the TOML path via serde; add it to the INI path too so a
        // stock rnsd config that sets it is honoured rather than silently dropped.
        "use_implicit_proof" => {
            config.use_implicit_proof = parse_bool(value);
        }
        // `enable_remote_management` is the upstream rnsd key
        // (Reticulum.py:548); `remote_management_enabled` is accepted as a
        // Leviculum-side alias so either spelling works.
        "enable_remote_management" | "remote_management_enabled" => {
            config.remote_management_enabled = parse_bool(value);
        }
        // Comma-separated identity hashes (ConfigObj `as_list`,
        // Reticulum.py:553). Hex is validated when the destination is built.
        "remote_management_allowed" => {
            config.remote_management_allowed = value
                .split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect();
        }
        "flush_interval" => {
            if let Ok(v) = value.trim().parse() {
                config.flush_interval_secs = v;
            }
        }
        "keepalive_interval" => {
            if let Ok(v) = value.trim().parse() {
                config.keepalive_interval = Some(v);
            }
        }
        // Codeberg #32 sub-task b: opt-in runtime auto-connect. An integer that
        // both enables the feature and caps concurrent auto-connections (Python
        // `autoconnect_discovered_interfaces`). Only positive values enable it.
        "autoconnect_discovered_interfaces" => {
            if let Ok(v) = value.trim().parse::<i64>() {
                config.autoconnect_discovered_interfaces = v.max(0) as usize;
            }
        }
        // Codeberg #32 sub-task d: path to the network identity for a private
        // (encrypted) discovery network (Python `network_identity`,
        // Reticulum.py:521). When set, encrypted discovery announces are
        // decrypted with this identity before validation.
        "network_identity" => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                config.network_identity = Some(std::path::PathBuf::from(trimmed));
            }
        }
        // Tolerate (accept without error) RNS 1.2.2..1.3.5 reticulum-level
        // keys we don't implement: blackhole_update_interval, default_ar_*,
        // egress_control, the ic_*/ic_pr_*/ec_pr_freq ingress/egress-control
        // tuning knobs, etc. An unknown key must never make lnsd reject a
        // config a current rnsd would accept.
        _ => {}
    }
}

/// Apply one `key = value` from the `[logging]` section. Only `loglevel` (the
/// RNS 0-7 level, Reticulum.py:66-73) is consumed; other keys (`logtimestamps`,
/// `logfile`, ...) are tolerated so a stock rnsd config is not rejected. Before
/// this the whole `[logging]` section was silently dropped.
fn apply_logging_key(config: &mut ReticulumConfig, key: &str, value: &str) {
    if key == "loglevel" {
        // A non-numeric or out-of-u8-range value leaves loglevel unset rather
        // than aborting the parse (matches the tolerant reticulum-key contract).
        config.loglevel = value.trim().parse().ok();
    }
}

/// Lightweight scan for `[logging] loglevel`, used to pick the default log
/// filter BEFORE the tracing subscriber is installed. A full `parse_ini` would
/// emit warnings with no subscriber to capture them (and re-run IFAC key
/// derivation), so this reads just the one key. Returns `None` if absent or
/// unparseable.
pub(crate) fn peek_loglevel(content: &str) -> Option<u8> {
    let mut in_logging = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Any header (`[section]`, `[[sub]]`, `[[[subsub]]]`) switches context.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed.trim_matches(|c| c == '[' || c == ']').trim();
            in_logging = name == "logging";
            continue;
        }
        if in_logging {
            if let Some((k, v)) = trimmed.split_once('=') {
                if k.trim() == "loglevel" {
                    // Strip an inline comment (`loglevel = 6  # verbose`) the
                    // same way a full parse would, or the raw RHS fails to
                    // parse and the daemon silently drops to info.
                    return strip_value(v).parse().ok();
                }
            }
        }
    }
    None
}

fn apply_interface_key(iface: &mut InterfaceConfig, key: &str, value: &str) {
    match key {
        "type" => iface.interface_type = value.to_string(),
        // `enabled` is the modern key; `interface_enabled` is the legacy spelling
        // upstream still honours (Reticulum.py:950). Accept both so a config that
        // disables an interface with the old key is not silently enabled.
        "enabled" | "interface_enabled" => iface.enabled = parse_bool(value),
        "outgoing" => iface.outgoing = parse_bool(value),
        // Kernel NIC to bind to, plus IPv6 preference when resolving it
        // (Codeberg #94/#3; TCPInterface.py:504/509, UDPInterface.py:61,
        // BackboneInterface.py:114/118). The name is resolved to a concrete
        // bind address at interface start.
        "device" => iface.device = Some(value.to_string()),
        "prefer_ipv6" => iface.prefer_ipv6 = Some(parse_bool(value)),
        "listen_ip" => iface.listen_ip = Some(value.to_string()),
        "listen_port" => iface.listen_port = value.parse().ok(),
        "target_host" => iface.target_host = Some(value.to_string()),
        "target_port" => iface.target_port = value.parse().ok(),
        // Backbone key aliases (Reticulum.py:963-964): `remote` is the peer to
        // connect to (-> target_host), `listen_on` is the local bind address
        // (-> listen_ip). No other interface type uses these spellings, so the
        // mapping is a safe superset; the `port` alias needs the type context and
        // is handled in `normalize_backbone_interface` after the full parse.
        "remote" => iface.target_host = Some(value.to_string()),
        "listen_on" => iface.listen_ip = Some(value.to_string()),
        "forward_ip" => iface.forward_ip = Some(value.to_string()),
        "forward_port" => iface.forward_port = value.parse().ok(),
        "port" => iface.port = Some(value.to_string()),
        "speed" | "baudrate" => iface.speed = value.parse().ok(),
        "databits" => iface.databits = value.parse().ok(),
        "parity" => iface.parity = Some(value.to_string()),
        "stopbits" => iface.stopbits = value.parse().ok(),
        // PipeInterface: external command + optional respawn delay
        // (PipeInterface.py:67-68). `command` is a plain shell-style string.
        "command" => iface.command = Some(value.to_string()),
        "respawn_delay" => iface.respawn_delay = value.parse().ok(),
        // KISSInterface TNC parameters (KISSInterface.py:86-89) and beacon
        // identification (id_interval/id_callsign, KISSInterface.py:96-97).
        "preamble" => iface.preamble = value.parse().ok(),
        "txtail" => iface.txtail = value.parse().ok(),
        "persistence" => iface.persistence = value.parse().ok(),
        "slottime" => iface.slottime = value.parse().ok(),
        "id_interval" => iface.id_interval = value.parse().ok(),
        "id_callsign" => iface.id_callsign = Some(value.to_string()),
        // AX25KISSInterface AX.25 addressing (AX25KISSInterface.py:104-105).
        "callsign" => iface.callsign = Some(value.to_string()),
        "ssid" => iface.ssid = value.parse().ok(),
        // I2PInterface: comma-separated list of remote `.b32.i2p` peers
        // (ConfigObj `as_list`, I2PInterface.py:851) and the `connectable`
        // flag that opens a local inbound endpoint (I2PInterface.py:852).
        "peers" => {
            iface.peers = Some(
                value
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect(),
            );
        }
        "connectable" => iface.connectable = Some(parse_bool(value)),
        // Honour a configured bitrate only when it clears MINIMUM_BITRATE, else
        // leave it unset so the interface keeps its default (Python
        // Reticulum.py:793-796). The stored value overrides the medium default
        // and feeds announce bandwidth capping / timing.
        "bitrate" => {
            if let Ok(v) = value.parse::<u64>() {
                if v >= leviculum_core::constants::MINIMUM_BITRATE as u64 {
                    iface.bitrate = Some(v);
                }
            }
        }
        // Interface discovery producer keys (Codeberg #109; Python
        // Reticulum.py:848-860). A `discoverable = yes` interface self-advertises
        // via `descriptor_from_config` + the autonomous announcer, which read
        // these `InterfaceConfig` fields. Before this they fell into the unknown-
        // key catch-all and were silently dropped, so a config-file-driven
        // deployment could never make an interface discoverable.
        "discoverable" => iface.discoverable = parse_bool(value),
        // Emit ENCRYPTED discovery announces for a private discovery network
        // (Python `discovery_encrypt`, Reticulum.py:859). The announcer
        // (`driver::mod`) already reads this field to pick the encrypted vs
        // plaintext app_data; it needs a configured `network_identity` to take
        // effect. Before this it fell into the unknown-key catch-all, so a
        // config-file deployment could never turn on encrypted discovery.
        "discovery_encrypt" => iface.discovery_encrypt = parse_bool(value),
        "discovery_name" => iface.discovery_name = Some(value.to_string()),
        "reachable_on" => iface.reachable_on = Some(value.to_string()),
        // Python config key is `announce_interval` in MINUTES (as_int, *60 with a
        // 5-minute floor, Reticulum.py:852-854); the minutes->seconds conversion
        // and floor live in `resolve_announce_interval_secs`. Stored verbatim.
        "announce_interval" => iface.announce_interval = value.parse().ok(),
        // Leviculum-only fast-test extension (no Python equivalent): a raw-seconds
        // interval that bypasses the 5-minute floor. Takes priority over
        // `announce_interval` in `resolve_announce_interval_secs`.
        "discovery_announce_interval_secs" => {
            iface.discovery_announce_interval_secs = value.parse().ok()
        }
        "buffer_size" => iface.buffer_size = value.parse().ok(),
        "reconnect_interval" => iface.reconnect_interval_secs = value.parse().ok(),
        "max_reconnect_tries" => iface.max_reconnect_tries = value.parse().ok(),
        "frequency" => iface.frequency = value.parse().ok(),
        "bandwidth" => iface.bandwidth = value.parse().ok(),
        "spreadingfactor" | "spreading_factor" => iface.spreading_factor = value.parse().ok(),
        "codingrate" | "coding_rate" => iface.coding_rate = value.parse().ok(),
        "txpower" | "tx_power" => iface.tx_power = value.parse().ok(),
        // LoRa preamble in symbols, for the LNode radio-config frame. Spelled
        // apart from the KISS `preamble` key above (CMD_TXDELAY, milliseconds)
        // because the two travel to different hardware and would otherwise be
        // one typo apart.
        "preamble_symbols" => iface.preamble_symbols = value.parse().ok(),
        // AutoInterface specific
        "group_id" => iface.group_id = Some(value.to_string()),
        "discovery_scope" => iface.discovery_scope = Some(value.to_string()),
        "discovery_port" => iface.discovery_port = value.parse().ok(),
        "data_port" => iface.data_port = value.parse().ok(),
        "devices" => iface.devices = Some(value.to_string()),
        "ignored_devices" => iface.ignored_devices = Some(value.to_string()),
        "multicast_loopback" => iface.multicast_loopback = Some(parse_bool(value)),
        "flow_control" => iface.flow_control = Some(parse_bool(value)),
        "airtime_limit_short" => iface.airtime_limit_short = value.parse().ok(),
        "airtime_limit_long" => iface.airtime_limit_long = value.parse().ok(),
        "csma_enabled" => iface.csma_enabled = Some(parse_bool(value)),
        // TEST-ONLY range emulation for co-located rigs: drop hops=0 ingress
        // frames (see `InterfaceConfig::test_drop_direct_ingress`). No Python
        // equivalent; consumed by the 3-node relay hardware scenario.
        "test_drop_direct_ingress" => iface.test_drop_direct_ingress = parse_bool(value),
        // Ingress control (Codeberg #8, Python `ingress_control`,
        // Reticulum.py:768-769). An explicit value overrides the medium-class
        // default; unset (None) resolves per medium at registration.
        "ingress_control" => iface.ingress_control = Some(parse_bool(value)),
        // Egress control (Codeberg #172, Python `egress_control`,
        // Reticulum.py:770-771). Off unless set, matching
        // `Interface.EGRESS_CONTROL = False`.
        "egress_control" => iface.egress_control = Some(parse_bool(value)),
        // Announce-rate keys (Codeberg #92). Parsed as unsigned, so a
        // negative value fails to parse and stays None; Python's >0 (target) /
        // >=0 (grace, penalty) validation is applied later in the driver.
        "announce_rate_target" => iface.announce_rate_target = value.parse().ok(),
        "announce_rate_penalty" => iface.announce_rate_penalty = value.parse().ok(),
        "announce_rate_grace" => iface.announce_rate_grace = value.parse().ok(),
        // Announce bandwidth cap (Codeberg #92, Reticulum.py:819-822). Python
        // keeps the value only when `0 < v <= 100`; an out-of-range or
        // unparseable value leaves the default (None) in place.
        "announce_cap" => {
            iface.announce_cap = value.parse::<f32>().ok().filter(|&v| v > 0.0 && v <= 100.0);
        }
        // Interface propagation mode (Codeberg #91). Both `mode` and
        // `interface_mode` spellings map here; the value string is resolved to
        // an `InterfaceMode` at registration via `InterfaceMode::from_config_str`
        // (all spellings from Reticulum.py:717-745). Stored verbatim so an
        // unknown value can be logged there rather than silently dropped. Note:
        // Python has a copy/paste bug where `interface_mode = gateway` checks
        // `c["mode"]` (Reticulum.py:729); we accept `gateway` under either key,
        // a strict superset that never rejects a config Python would honour.
        "mode" | "interface_mode" => iface.mode = Some(value.to_string()),
        "networkname" | "network_name" => iface.networkname = Some(value.to_string()),
        "passphrase" | "pass_phrase" => iface.passphrase = Some(value.to_string()),
        // `ifac_size` is specified in BITS and stored in BYTES. Python only
        // honours it when it is at least `IFAC_MIN_SIZE*8` bits (= 8 bits =
        // 1 byte); a smaller value is dropped so the interface falls back to
        // its per-type DEFAULT_IFAC_SIZE (Reticulum.py:747-750). Mirror that
        // guard exactly, otherwise a sub-8-bit value would round down to 0
        // bytes and silently disable IFAC where Python keeps it enabled.
        "ifac_size" => {
            iface.ifac_size = value
                .parse::<usize>()
                .ok()
                .filter(|&bits| bits >= 8)
                .map(|bits| bits / 8)
        }
        // BLEInterface (Columba ble-reticulum): the reference's own key
        // names (`ble-reticulum@07d94130`, `BLEInterface.py`, config
        // parsing). The BlueZ adapter is picked with the shared `device`
        // key above (`device = hci0`). Appended at the end of the match
        // on purpose: the config-reference tables cite the arms above by
        // line.
        "max_connections" => iface.max_connections = value.parse().ok(),
        "min_rssi" => iface.min_rssi = value.parse().ok(),
        "discovery_interval" => iface.discovery_interval = value.parse().ok(),
        "enable_central" => iface.enable_central = Some(parse_bool(value)),
        "enable_peripheral" => iface.enable_peripheral = Some(parse_bool(value)),
        // Unknown per-interface key: log and ignore. An unrecognised key (a
        // Backbone-only knob like `prioritise`, an IFAC field, a kernel device
        // bind, id_callsign, modulation, ...) must never make lnsd reject an
        // interface a current rnsd would accept.
        _ => {
            tracing::debug!("Ignoring unknown interface key '{}' = '{}'", key, value);
        }
    }
}

/// Flush a pending `[[[subinterface]]]` block into its parent interface's
/// `subinterfaces` list. A subinterface with no parent (malformed config, a
/// `[[[..]]]` before any `[[..]]`) is dropped.
fn flush_subinterface(
    current_subinterface: &mut Option<SubinterfaceConfig>,
    current_iface: &mut Option<(String, InterfaceConfig)>,
) {
    if let Some(sub) = current_subinterface.take() {
        if let Some((_, ref mut iface)) = current_iface {
            iface.subinterfaces.push(sub);
        }
    }
}

/// Apply one `key = value` from a `[[[subinterface]]]` block. Only the per-vport
/// radio + routing keys are meaningful here (the shared `port` and `id_*` beacon
/// keys live on the parent). Mirrors the fields Python reads per subinterface in
/// `RNodeMultiInterface.__init__`.
fn apply_subinterface_key(sub: &mut SubinterfaceConfig, key: &str, value: &str) {
    match key {
        "enabled" | "interface_enabled" => sub.enabled = parse_bool(value),
        "outgoing" => sub.outgoing = parse_bool(value),
        "vport" => sub.vport = value.parse().ok(),
        "frequency" => sub.frequency = value.parse().ok(),
        "bandwidth" => sub.bandwidth = value.parse().ok(),
        "spreadingfactor" | "spreading_factor" => sub.spreading_factor = value.parse().ok(),
        "codingrate" | "coding_rate" => sub.coding_rate = value.parse().ok(),
        "txpower" | "tx_power" => sub.tx_power = value.parse().ok(),
        "airtime_limit_short" => sub.airtime_limit_short = value.parse().ok(),
        "airtime_limit_long" => sub.airtime_limit_long = value.parse().ok(),
        _ => {
            tracing::debug!("Ignoring unknown subinterface key '{}' = '{}'", key, value);
        }
    }
}

/// Map a `BackboneInterface` / `BackboneClientInterface` config onto our TCP
/// interface, mirroring Python's normalization + mode dispatch
/// (`Reticulum.py:960-972`). BackboneInterface is wire-identical to TCPInterface
/// (HDLC-over-TCP, same FLAG/ESC framing, no handshake), so the resulting
/// `InterfaceConfig` is indistinguishable from the equivalent
/// `TCPServerInterface` / `TCPClientInterface` and the rest of the stack is
/// untouched. A non-Backbone interface passes through unchanged.
fn normalize_backbone_interface(iface: &mut InterfaceConfig) {
    match iface.interface_type.as_str() {
        "BackboneInterface" | "BackboneClientInterface" => {}
        _ => return,
    }

    // `port` -> both listen_port and target_port (Reticulum.py:961-962). On a
    // Backbone entry `port` is a TCP port number, not a serial device path, so
    // consume `iface.port` here. `remote` (-> target_host) and `listen_on`
    // (-> listen_ip) were already applied during the key scan.
    if let Some(p) = iface
        .port
        .as_deref()
        .and_then(|s| s.trim().parse::<u16>().ok())
    {
        iface.listen_port = Some(p);
        iface.target_port = Some(p);
        iface.port = None;
    }

    // Mode dispatch (Reticulum.py:966-972): a BackboneClientInterface is always a
    // client; a plain BackboneInterface with a target host is a client, otherwise
    // it listens as a server.
    let is_client =
        iface.interface_type == "BackboneClientInterface" || iface.target_host.is_some();
    iface.interface_type = if is_client {
        "TCPClientInterface".to_string()
    } else {
        "TCPServerInterface".to_string()
    };
}

/// Parse a ConfigObj boolean value (case-insensitive, like ConfigObj).
///
/// Accepts (any case): yes, true, on, 1 → true
///                     no, false, off, 0 → false
///                     Anything else → false (conservative default, unchanged
///                     contract).
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "yes" | "true" | "on" | "1"
    )
}

/// Apply ConfigObj value semantics to a raw `key = <value>` right-hand side.
///
/// 1. Strip an inline comment starting at the first UNQUOTED `#`. ConfigObj
///    terminates an unquoted value at the first `#` regardless of preceding
///    whitespace (`_valueexp`, vendor/configobj.py:1099 — the unquoted branch
///    `[^'",\#\s][^,]*?` is non-greedy against the optional `\s*(\#.*)?$`
///    comment). A `#` inside single/double quotes is preserved.
/// 2. Trim surrounding whitespace.
/// 3. Strip a single pair of surrounding double or single quotes.
///
/// This matters for wire compatibility: `passphrase = s3cr#et` must yield the
/// same `s3cr` that rnsd derives its IFAC key from, or the two nodes silently
/// fail to authenticate on that interface.
fn strip_value(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut cut = None;
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => {
                cut = Some(i);
                break;
            }
            _ => {}
        }
    }
    let sliced: String = match cut {
        Some(i) => chars[..i].iter().collect(),
        None => raw.to_string(),
    };
    strip_surrounding_quotes(sliced.trim()).to_string()
}

/// Strip a single pair of matching surrounding quotes (`"..."` or `'...'`).
/// A lone or mismatched quote is left untouched.
fn strip_surrounding_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_true() {
        assert!(parse_bool("Yes"));
        assert!(parse_bool("yes"));
        assert!(parse_bool("True"));
        assert!(parse_bool("true"));
        assert!(parse_bool("1"));
        assert!(parse_bool("on"));
    }

    #[test]
    fn test_parse_bool_false() {
        assert!(!parse_bool("No"));
        assert!(!parse_bool("no"));
        assert!(!parse_bool("False"));
        assert!(!parse_bool("false"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool("off"));
        assert!(!parse_bool(""));
    }

    #[test]
    fn test_parse_bool_case_insensitive() {
        // ConfigObj is case-insensitive for bools and accepts on/off.
        assert!(parse_bool("TRUE"));
        assert!(parse_bool("Yes"));
        assert!(parse_bool("ON"));
        assert!(parse_bool("On"));
        assert!(!parse_bool("OFF"));
        assert!(!parse_bool("off"));
        assert!(!parse_bool("No"));
        assert!(!parse_bool("FALSE"));
        assert!(!parse_bool("0"));
        assert!(parse_bool("1"));
        // Unknown input keeps the current contract: false.
        assert!(!parse_bool("maybe"));
    }

    #[test]
    fn test_strip_value_configobj_semantics() {
        // ConfigObj terminates an unquoted value at the FIRST `#`, whether or
        // not whitespace precedes it (vendor/configobj.py:1099 `_valueexp`).
        assert_eq!(strip_value("abc#def"), "abc");
        assert_eq!(strip_value("s3cr#et"), "s3cr");
        assert_eq!(strip_value("http://x#z"), "http://x");
        assert_eq!(strip_value("4242 # main"), "4242");
        // A `#` inside quotes is protected; the surrounding quotes are stripped.
        assert_eq!(strip_value("\"abc#def\""), "abc#def");
        assert_eq!(strip_value("'s3cr#et'"), "s3cr#et");
        // No comment: value passes through, surrounding quotes stripped.
        assert_eq!(strip_value("plain"), "plain");
        assert_eq!(strip_value("\"example.com\""), "example.com");
    }

    #[test]
    fn test_inline_comment_and_quote_stripping() {
        // ConfigObj strips an inline comment starting at the first unquoted
        // `#`, and a single pair of surrounding quotes.
        let config = parse_ini(
            r#"
[reticulum]
  enable_transport = TRUE

[interfaces]
  [[Srv]]
    type = TCPServerInterface
    listen_port = 4242  # main port
    enabled = off

  [[Cli]]
    type = TCPClientInterface
    target_host = "example.com"
    target_port = 4243
    network_name = abc#def
"#,
        )
        .unwrap();

        assert!(config.reticulum.enable_transport, "TRUE is truthy");

        let srv = config.interfaces.get("Srv").expect("srv");
        // Inline comment stripped -> value parses as a number, not None.
        assert_eq!(srv.listen_port, Some(4242));
        assert!(!srv.enabled, "off is falsy");

        let cli = config.interfaces.get("Cli").expect("cli");
        // Surrounding quotes stripped.
        assert_eq!(cli.target_host.as_deref(), Some("example.com"));
        // An unquoted `#` terminates the value like ConfigObj does; only the
        // text before the hash survives.
        assert_eq!(cli.networkname.as_deref(), Some("abc"));
    }

    /// A `[[BLE Interface]]` section parses with the reference's own key
    /// names (`ble-reticulum@07d94130` `BLEInterface.py`, config parsing), survives the
    /// supported-type filter, and leaves every unset key `None` so the
    /// builder's defaults apply.
    #[test]
    fn test_ble_interface_section_parses_with_reference_keys() {
        let config = parse_ini(
            r#"
[interfaces]
  [[BLE Interface]]
    type = BLEInterface
    device = hci0
    max_connections = 2
    min_rssi = -70
    discovery_interval = 2.5
    enable_central = yes
    enable_peripheral = no
"#,
        )
        .unwrap();

        let ble = config
            .interfaces
            .get("BLE Interface")
            .expect("kept by the type filter");
        assert!(ble.enabled, "enabled defaults to true when configured");
        assert_eq!(ble.interface_type, "BLEInterface");
        assert_eq!(ble.device.as_deref(), Some("hci0"));
        assert_eq!(ble.max_connections, Some(2));
        assert_eq!(ble.min_rssi, Some(-70));
        assert_eq!(ble.discovery_interval, Some(2.5));
        assert_eq!(ble.enable_central, Some(true));
        assert_eq!(ble.enable_peripheral, Some(false));

        // A minimal section leaves the tunables to the builder's defaults.
        let config = parse_ini(
            r#"
[interfaces]
  [[ble]]
    type = BLEInterface
    enabled = no
"#,
        )
        .unwrap();
        let ble = config.interfaces.get("ble").expect("ble");
        assert!(!ble.enabled);
        assert_eq!(ble.max_connections, None);
        assert_eq!(ble.min_rssi, None);
        assert_eq!(ble.discovery_interval, None);
        assert_eq!(ble.enable_central, None);
        assert_eq!(ble.enable_peripheral, None);
    }

    #[test]
    fn test_quoted_hash_survives_stripping() {
        // Quoting protects a `#` so an IFAC network_name/passphrase that really
        // contains a hash is preserved verbatim.
        let config = parse_ini(
            r#"
[interfaces]
  [[Cli]]
    type = TCPClientInterface
    network_name = "abc#def"
    passphrase = 's3cr#et'
"#,
        )
        .unwrap();

        let cli = config.interfaces.get("Cli").expect("cli");
        assert_eq!(cli.networkname.as_deref(), Some("abc#def"));
        assert_eq!(cli.passphrase.as_deref(), Some("s3cr#et"));
    }

    #[test]
    fn test_parse_minimal_ini() {
        let config = parse_ini(
            r#"
[reticulum]
  enable_transport = True

[interfaces]
  [[My TCP Server]]
    type = TCPServerInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242

  [[My TCP Client]]
    type = TCPClientInterface
    enabled = True
    target_host = 127.0.0.1
    target_port = 4243
"#,
        )
        .unwrap();

        assert!(config.reticulum.enable_transport);

        let server = config.interfaces.get("My TCP Server").expect("server");
        assert_eq!(server.interface_type, "TCPServerInterface");
        assert!(server.enabled);
        assert_eq!(server.listen_ip, Some("0.0.0.0".to_string()));
        assert_eq!(server.listen_port, Some(4242));

        let client = config.interfaces.get("My TCP Client").expect("client");
        assert_eq!(client.interface_type, "TCPClientInterface");
        assert!(client.enabled);
        assert_eq!(client.target_host, Some("127.0.0.1".to_string()));
        assert_eq!(client.target_port, Some(4243));
    }

    #[test]
    fn test_parse_device_and_prefer_ipv6() {
        // Codeberg #94/#3: `device` (kernel NIC to bind to) and `prefer_ipv6`
        // parse into the interface config across TCP/Backbone and UDP.
        let config = parse_ini(
            r#"
[interfaces]
  [[Bound TCP]]
    type = TCPServerInterface
    listen_port = 4242
    device = eth0
    prefer_ipv6 = yes

  [[Bound Backbone]]
    type = BackboneInterface
    port = 4243
    device = eth1

  [[Bound UDP]]
    type = UDPInterface
    listen_port = 4244
    device = eth2
"#,
        )
        .unwrap();

        let tcp = config.interfaces.get("Bound TCP").expect("tcp");
        assert_eq!(tcp.device.as_deref(), Some("eth0"));
        assert_eq!(tcp.prefer_ipv6, Some(true));

        // Backbone normalizes to TCPServerInterface but keeps `device`.
        let backbone = config.interfaces.get("Bound Backbone").expect("backbone");
        assert_eq!(backbone.device.as_deref(), Some("eth1"));
        assert_eq!(backbone.prefer_ipv6, None);

        let udp = config.interfaces.get("Bound UDP").expect("udp");
        assert_eq!(udp.device.as_deref(), Some("eth2"));
    }

    #[test]
    fn test_parse_ifac_keys() {
        // network_name + passphrase + ifac_size (in BITS) parse into the
        // interface config; ifac_size is stored in BYTES.
        let config = parse_ini(
            r#"
[interfaces]
  [[Secure TCP]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4242
    network_name = mynet
    passphrase = s3cret
    ifac_size = 128
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Secure TCP").expect("iface");
        assert_eq!(iface.networkname.as_deref(), Some("mynet"));
        assert_eq!(iface.passphrase.as_deref(), Some("s3cret"));
        // 128 bits / 8 = 16 bytes
        assert_eq!(iface.ifac_size, Some(16));
    }

    #[test]
    fn test_parse_interface_mode_all_spellings() {
        // Codeberg #91: `mode` / `interface_mode` on each interface parses into
        // the raw config string and resolves to the right InterfaceMode via
        // InterfaceMode::from_config_str (all Reticulum.py:717-745 spellings).
        use leviculum_core::traits::InterfaceMode;
        let cases = [
            ("gw", InterfaceMode::Gateway),
            ("access_point", InterfaceMode::AccessPoint),
            ("ap", InterfaceMode::AccessPoint),
            ("ptp", InterfaceMode::PointToPoint),
            ("roaming", InterfaceMode::Roaming),
            ("boundary", InterfaceMode::Boundary),
            ("full", InterfaceMode::Full),
        ];
        for (spelling, expected) in cases {
            // `mode = ...`
            let cfg = parse_ini(&format!(
                "[interfaces]\n  [[If]]\n    type = TCPClientInterface\n    mode = {spelling}\n"
            ))
            .unwrap();
            let raw = cfg.interfaces.get("If").unwrap().mode.clone();
            assert_eq!(
                InterfaceMode::from_config_str(raw.as_deref().unwrap()),
                Some(expected),
                "mode = {spelling}"
            );
            // `interface_mode = ...` (alias)
            let cfg2 = parse_ini(&format!(
                "[interfaces]\n  [[If]]\n    type = TCPClientInterface\n    interface_mode = {spelling}\n"
            ))
            .unwrap();
            let raw2 = cfg2.interfaces.get("If").unwrap().mode.clone();
            assert_eq!(
                InterfaceMode::from_config_str(raw2.as_deref().unwrap()),
                Some(expected),
                "interface_mode = {spelling}"
            );
        }
    }

    #[test]
    fn test_parse_interface_mode_absent_defaults_full() {
        // No mode key -> None -> the Full default applies at registration.
        let cfg = parse_ini(
            "[interfaces]\n  [[If]]\n    type = TCPClientInterface\n    target_host = 127.0.0.1\n",
        )
        .unwrap();
        assert_eq!(cfg.interfaces.get("If").unwrap().mode, None);
    }

    #[test]
    fn test_parse_ingress_control_key_and_medium_defaults() {
        use crate::config::ingress_control_default_for_type;
        // Explicit `ingress_control` parses (yes/no spellings) and overrides.
        let on = parse_ini(
            "[interfaces]\n  [[If]]\n    type = TCPClientInterface\n    ingress_control = yes\n",
        )
        .unwrap();
        let tcp_on = on.interfaces.get("If").unwrap();
        assert_eq!(tcp_on.ingress_control, Some(true));
        assert!(
            tcp_on.resolve_ingress_control(),
            "explicit yes restores the limiter on TCP"
        );

        let off = parse_ini(
            "[interfaces]\n  [[If]]\n    type = RNodeInterface\n    ingress_control = no\n",
        )
        .unwrap();
        let lora_off = off.interfaces.get("If").unwrap();
        assert_eq!(lora_off.ingress_control, Some(false));
        assert!(!lora_off.resolve_ingress_control());

        // Absent key -> None -> medium-class default resolves at registration:
        // TCP off, RNode on.
        let tcp = parse_ini("[interfaces]\n  [[If]]\n    type = TCPClientInterface\n").unwrap();
        let tcp_cfg = tcp.interfaces.get("If").unwrap();
        assert_eq!(tcp_cfg.ingress_control, None);
        assert!(
            !tcp_cfg.resolve_ingress_control(),
            "unset TCP resolves ingress control OFF"
        );
        assert!(!ingress_control_default_for_type("TCPClientInterface"));

        let lora = parse_ini("[interfaces]\n  [[If]]\n    type = RNodeInterface\n").unwrap();
        let lora_cfg = lora.interfaces.get("If").unwrap();
        assert_eq!(lora_cfg.ingress_control, None);
        assert!(
            lora_cfg.resolve_ingress_control(),
            "unset RNode resolves ingress control ON"
        );
        assert!(ingress_control_default_for_type("RNodeInterface"));

        // Codeberg #189: a listener resolves ON when unset — it accepts
        // arbitrary unknown peers, which is the storm surface the limiter
        // exists for, and it is also what the reference does for every
        // interface (Interface.py:112). The value is inherited by every
        // connection the listener accepts, so this is the only place an
        // operator can configure the limiter for those connections.
        let srv = parse_ini(
            "[interfaces]\n  [[If]]\n    type = TCPServerInterface\n    listen_port = 4242\n",
        )
        .unwrap();
        let srv_cfg = srv.interfaces.get("If").unwrap();
        assert_eq!(srv_cfg.ingress_control, None);
        assert!(
            srv_cfg.resolve_ingress_control(),
            "unset TCP server resolves ingress control ON"
        );
        assert!(ingress_control_default_for_type("TCPServerInterface"));

        // A listening BackboneInterface normalizes to TCPServerInterface at
        // parse time, so it picks up the listener default too; a Backbone entry
        // with a `remote` becomes a client and keeps the dial-out default.
        let bb_listen =
            parse_ini("[interfaces]\n  [[If]]\n    type = BackboneInterface\n    port = 4242\n")
                .unwrap();
        assert!(bb_listen
            .interfaces
            .get("If")
            .unwrap()
            .resolve_ingress_control());
        let bb_client = parse_ini(
            "[interfaces]\n  [[If]]\n    type = BackboneInterface\n    remote = example.com\n    port = 4242\n",
        )
        .unwrap();
        assert!(!bb_client
            .interfaces
            .get("If")
            .unwrap()
            .resolve_ingress_control());
    }

    /// Codeberg #172: `egress_control` is an rnsd config key
    /// (Reticulum.py:770-771), so a drop-in config must parse it. Unset means
    /// off, matching `Interface.EGRESS_CONTROL = False` — there is no
    /// medium-class default for this one.
    #[test]
    fn test_parse_egress_control_key() {
        let on = parse_ini(
            "[interfaces]\n  [[If]]\n    type = TCPClientInterface\n    egress_control = yes\n",
        )
        .unwrap();
        assert_eq!(on.interfaces.get("If").unwrap().egress_control, Some(true));

        let off = parse_ini(
            "[interfaces]\n  [[If]]\n    type = RNodeInterface\n    egress_control = no\n",
        )
        .unwrap();
        assert_eq!(
            off.interfaces.get("If").unwrap().egress_control,
            Some(false)
        );

        let unset = parse_ini("[interfaces]\n  [[If]]\n    type = RNodeInterface\n").unwrap();
        assert_eq!(unset.interfaces.get("If").unwrap().egress_control, None);
    }

    #[test]
    fn test_parse_pipe_interface() {
        // A PipeInterface config maps `command` + `respawn_delay` onto the
        // interface, is recognised as a supported type, and survives the
        // filter (Codeberg #95).
        let config = parse_ini(
            r#"
[interfaces]
  [[My Pipe]]
    type = PipeInterface
    command = /usr/bin/socat - TCP:host:4242
    respawn_delay = 3
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("My Pipe").expect("pipe iface");
        assert_eq!(iface.interface_type, "PipeInterface");
        assert_eq!(
            iface.command.as_deref(),
            Some("/usr/bin/socat - TCP:host:4242")
        );
        assert_eq!(iface.respawn_delay, Some(3.0));
    }

    #[test]
    fn test_parse_kiss_interface() {
        // A KISSInterface config maps the serial line settings, the TNC
        // parameters (preamble/txtail/persistence/slottime), flow_control, and
        // the beacon keys (id_interval/id_callsign) onto the interface, is a
        // supported type, and survives the filter (Codeberg #96).
        let config = parse_ini(
            r#"
[interfaces]
  [[My KISS TNC]]
    type = KISSInterface
    port = /dev/ttyUSB0
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    preamble = 150
    txtail = 10
    persistence = 200
    slottime = 30
    flow_control = true
    id_interval = 600
    id_callsign = MYCALL-1
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("My KISS TNC").expect("kiss iface");
        assert_eq!(iface.interface_type, "KISSInterface");
        assert_eq!(iface.port.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(iface.speed, Some(115200));
        assert_eq!(iface.databits, Some(8));
        assert_eq!(iface.parity.as_deref(), Some("none"));
        assert_eq!(iface.stopbits, Some(1));
        assert_eq!(iface.preamble, Some(150));
        assert_eq!(iface.txtail, Some(10));
        assert_eq!(iface.persistence, Some(200));
        assert_eq!(iface.slottime, Some(30));
        assert_eq!(iface.flow_control, Some(true));
        assert_eq!(iface.id_interval, Some(600));
        assert_eq!(iface.id_callsign.as_deref(), Some("MYCALL-1"));
    }

    /// A `SerialInterface` block carrying the LoRa PHY keys (the LNode
    /// radio-config frame the driver pushes at startup) parses every one of
    /// them, `preamble_symbols` included.
    #[test]
    fn test_parse_serial_lnode_lora_keys() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Serial LNode]]
    type = SerialInterface
    port = /dev/ttyACM0
    speed = 115200
    frequency = 869525000
    bandwidth = 125000
    spreadingfactor = 10
    codingrate = 8
    txpower = 17
    preamble_symbols = 18
    csma_enabled = yes
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Serial LNode").expect("serial iface");
        assert_eq!(iface.interface_type, "SerialInterface");
        assert_eq!(iface.frequency, Some(869_525_000));
        assert_eq!(iface.bandwidth, Some(125_000));
        assert_eq!(iface.spreading_factor, Some(10));
        assert_eq!(iface.coding_rate, Some(8));
        assert_eq!(iface.tx_power, Some(17));
        assert_eq!(iface.preamble_symbols, Some(18));
        assert_eq!(iface.csma_enabled, Some(true));
    }

    /// The TEST-ONLY `test_drop_direct_ingress` key parses on the INI path
    /// (the periculum-generated lnsd config is INI) and defaults off when
    /// absent.
    #[test]
    fn test_parse_test_drop_direct_ingress_key() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Deaf RNode]]
    type = RNodeInterface
    port = /dev/ttyACM0
    frequency = 869525000
    bandwidth = 125000
    spreadingfactor = 7
    codingrate = 5
    test_drop_direct_ingress = yes
  [[Normal RNode]]
    type = RNodeInterface
    port = /dev/ttyACM1
    frequency = 869525000
    bandwidth = 125000
    spreadingfactor = 7
    codingrate = 5
"#,
        )
        .unwrap();
        assert!(
            config
                .interfaces
                .get("Deaf RNode")
                .expect("deaf iface")
                .test_drop_direct_ingress
        );
        assert!(
            !config
                .interfaces
                .get("Normal RNode")
                .expect("normal iface")
                .test_drop_direct_ingress
        );
    }

    /// Two guarantees the new key has to keep. Omitting `preamble_symbols`
    /// leaves it unset, which is what makes the driver fall back to the
    /// firmware's own 24 and every pre-existing config file mean what it
    /// meant before. And the KISS `preamble` key (CMD_TXDELAY, milliseconds)
    /// still lands on its own field — the two names are one typo apart and
    /// must not alias.
    #[test]
    fn test_serial_preamble_symbols_absent_and_kiss_preamble_distinct() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Serial No Preamble]]
    type = SerialInterface
    port = /dev/ttyACM0
    frequency = 869525000
  [[Serial Kiss Preamble]]
    type = SerialInterface
    port = /dev/ttyACM1
    frequency = 869525000
    preamble = 150
"#,
        )
        .unwrap();

        let plain = config
            .interfaces
            .get("Serial No Preamble")
            .expect("serial iface");
        assert_eq!(plain.preamble_symbols, None);

        let kiss_spelled = config
            .interfaces
            .get("Serial Kiss Preamble")
            .expect("serial iface");
        assert_eq!(kiss_spelled.preamble, Some(150));
        assert_eq!(kiss_spelled.preamble_symbols, None);
    }

    #[test]
    fn test_parse_ax25_kiss_interface() {
        // An AX25KISSInterface config maps the AX.25 addressing (callsign/ssid)
        // on top of the same KISS/serial params, is a supported type, and
        // survives the filter (Codeberg #97).
        let config = parse_ini(
            r#"
[interfaces]
  [[My AX25 TNC]]
    type = AX25KISSInterface
    port = /dev/ttyUSB0
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    callsign = N0CALL
    ssid = 3
    preamble = 150
"#,
        )
        .unwrap();

        let iface = config
            .interfaces
            .get("My AX25 TNC")
            .expect("ax25 kiss iface");
        assert_eq!(iface.interface_type, "AX25KISSInterface");
        assert_eq!(iface.port.as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(iface.speed, Some(115200));
        assert_eq!(iface.callsign.as_deref(), Some("N0CALL"));
        assert_eq!(iface.ssid, Some(3));
        assert_eq!(iface.preamble, Some(150));
    }

    #[test]
    fn test_parse_rnode_multi_interface_nested_subinterfaces() {
        // An RNodeMultiInterface carries several LoRa transceivers as nested
        // [[[subinterface]]] blocks. Each triple-bracket block must parse into a
        // SubinterfaceConfig on the parent, with its own vport + radio settings,
        // while the parent keeps the shared `port` and beacon keys. Mirrors the
        // docs example (interfaces.rst) with a high- and a low-datarate vport.
        let config = parse_ini(
            r#"
[interfaces]
  [[RNode Multi Interface]]
    type = RNodeMultiInterface
    enabled = yes
    port = /dev/ttyACM0
    id_callsign = MYCALL-0
    id_interval = 600

    [[[High Datarate]]]
      enabled = yes
      frequency = 2400000000
      bandwidth = 1625000
      txpower = 0
      vport = 1
      spreadingfactor = 5
      codingrate = 5

    [[[Low Datarate]]]
      enabled = yes
      frequency = 865600000
      vport = 0
      bandwidth = 125000
      txpower = 0
      spreadingfactor = 7
      codingrate = 5
      outgoing = no
"#,
        )
        .unwrap();

        let iface = config
            .interfaces
            .get("RNode Multi Interface")
            .expect("rnode multi iface");
        assert_eq!(iface.interface_type, "RNodeMultiInterface");
        // Shared parent keys stay on the parent, not on a subinterface.
        assert_eq!(iface.port.as_deref(), Some("/dev/ttyACM0"));
        assert_eq!(iface.id_callsign.as_deref(), Some("MYCALL-0"));
        assert_eq!(iface.id_interval, Some(600));

        // Two nested subinterfaces, in file order.
        assert_eq!(iface.subinterfaces.len(), 2);

        let high = &iface.subinterfaces[0];
        assert_eq!(high.name, "High Datarate");
        assert!(high.enabled);
        assert!(high.outgoing);
        assert_eq!(high.vport, Some(1));
        assert_eq!(high.frequency, Some(2_400_000_000));
        assert_eq!(high.bandwidth, Some(1_625_000));
        assert_eq!(high.spreading_factor, Some(5));
        assert_eq!(high.coding_rate, Some(5));
        assert_eq!(high.tx_power, Some(0));

        let low = &iface.subinterfaces[1];
        assert_eq!(low.name, "Low Datarate");
        assert_eq!(low.vport, Some(0));
        assert_eq!(low.frequency, Some(865_600_000));
        assert_eq!(low.bandwidth, Some(125_000));
        assert_eq!(low.spreading_factor, Some(7));
        // `outgoing = no` on the low-datarate vport (RX only).
        assert!(!low.outgoing);
    }

    #[test]
    fn test_parse_rnode_multi_interface_survives_filter() {
        // RNodeMultiInterface is a supported type: it must survive the
        // unsupported-type filter (unlike I2PInterface) with subinterfaces intact.
        let config = parse_ini(
            r#"
[interfaces]
  [[Multi]]
    type = RNodeMultiInterface
    port = /dev/ttyACM0
    [[[Band A]]]
      enabled = yes
      vport = 0
      frequency = 868000000
      bandwidth = 125000
      spreadingfactor = 7
      codingrate = 5
      txpower = 17
"#,
        )
        .unwrap();
        let iface = config
            .interfaces
            .get("Multi")
            .expect("multi survives filter");
        assert_eq!(iface.interface_type, "RNodeMultiInterface");
        assert_eq!(iface.subinterfaces.len(), 1);
        assert_eq!(iface.subinterfaces[0].vport, Some(0));
    }

    #[test]
    fn test_parse_ifac_alt_spellings() {
        // The Python config accepts both `networkname`/`network_name` and
        // `passphrase`/`pass_phrase`. Both spellings must populate the same
        // field so a drop-in rnsd config authenticates identically.
        let config = parse_ini(
            r#"
[interfaces]
  [[Alt Spelling]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4242
    networkname = altnet
    pass_phrase = altpass
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Alt Spelling").expect("iface");
        assert_eq!(iface.networkname.as_deref(), Some("altnet"));
        assert_eq!(iface.passphrase.as_deref(), Some("altpass"));
    }

    #[test]
    fn test_parse_ifac_size_below_min_dropped() {
        // Python drops an ifac_size below IFAC_MIN_SIZE*8 (= 8 bits) so the
        // interface falls back to its per-type default. We must do the same:
        // a value of 4 bits must NOT round down to 0 bytes.
        let config = parse_ini(
            r#"
[interfaces]
  [[Tiny IFAC]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4242
    network_name = mynet
    ifac_size = 4
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Tiny IFAC").expect("iface");
        assert_eq!(
            iface.ifac_size, None,
            "sub-8-bit ifac_size must be dropped, not stored as 0 bytes"
        );
        // A valid IFAC config is still derivable from network_name alone, using
        // the interface default size (16 bytes for TCP).
        let ifac = leviculum_core::ifac::IfacConfig::new(iface.networkname.as_deref(), None, 16);
        assert!(ifac.is_ok());
    }

    #[test]
    fn test_parse_ifac_config_builds_expected_identity() {
        // The parsed keys must derive the SAME IFAC identity as constructing
        // the IfacConfig directly from those values -- this is what the driver
        // does when it enforces IFAC, so it pins the config-to-crypto path.
        let config = parse_ini(
            r#"
[interfaces]
  [[Secure]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4242
    network_name = mynet
    passphrase = s3cret
    ifac_size = 128
"#,
        )
        .unwrap();
        let iface = config.interfaces.get("Secure").expect("iface");

        let from_parsed = leviculum_core::ifac::IfacConfig::new(
            iface.networkname.as_deref(),
            iface.passphrase.as_deref(),
            iface.ifac_size.unwrap(),
        )
        .expect("valid IFAC config");
        let direct = leviculum_core::ifac::IfacConfig::new(Some("mynet"), Some("s3cret"), 16)
            .expect("valid IFAC config");

        assert_eq!(from_parsed.ifac_size(), 16);
        assert_eq!(
            from_parsed.identity().hash(),
            direct.identity().hash(),
            "parsed IFAC keys must derive the canonical IFAC identity"
        );
    }

    #[test]
    fn test_parse_announce_rate_keys() {
        // Codeberg #67 Stage 2a: the three announce_rate_* keys parse into the
        // interface config; an interface that omits them leaves them None.
        let config = parse_ini(
            r#"
[reticulum]
  enable_transport = True

[interfaces]
  [[Rated TCP]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4243
    announce_rate_target = 7200
    announce_rate_penalty = 30
    announce_rate_grace = 2

  [[Plain TCP]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4244
"#,
        )
        .unwrap();

        let rated = config.interfaces.get("Rated TCP").expect("rated");
        assert_eq!(rated.announce_rate_target, Some(7200));
        assert_eq!(rated.announce_rate_penalty, Some(30));
        assert_eq!(rated.announce_rate_grace, Some(2));

        let plain = config.interfaces.get("Plain TCP").expect("plain");
        assert_eq!(plain.announce_rate_target, None);
        assert_eq!(plain.announce_rate_penalty, None);
        assert_eq!(plain.announce_rate_grace, None);
    }

    #[test]
    fn test_parse_announce_cap_key() {
        // Codeberg #92: announce_cap parses a percentage, kept only when
        // 0 < v <= 100 (Reticulum.py:819-822). Out-of-range / unparseable
        // values leave the default (None) in place.
        let config = parse_ini(
            r#"
[interfaces]
  [[Capped]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4243
    announce_cap = 5

  [[Fractional]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4244
    announce_cap = 2.5

  [[Zero]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4245
    announce_cap = 0

  [[TooLarge]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4246
    announce_cap = 150

  [[Plain]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4247
"#,
        )
        .unwrap();

        assert_eq!(
            config.interfaces.get("Capped").unwrap().announce_cap,
            Some(5.0)
        );
        assert_eq!(
            config.interfaces.get("Fractional").unwrap().announce_cap,
            Some(2.5)
        );
        // 0 and >100 are rejected by Python's validation, so they stay None.
        assert_eq!(config.interfaces.get("Zero").unwrap().announce_cap, None);
        assert_eq!(
            config.interfaces.get("TooLarge").unwrap().announce_cap,
            None
        );
        assert_eq!(config.interfaces.get("Plain").unwrap().announce_cap, None);
    }

    #[test]
    fn test_parse_bitrate_key() {
        // Codeberg #93: a configured bitrate is honoured only when it clears
        // MINIMUM_BITRATE (5 bps); below that it is ignored and stays unset,
        // matching Python (Reticulum.py:793-796). An unset key also stays None.
        let config = parse_ini(
            r#"
[interfaces]
  [[Fast]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4243
    bitrate = 1200

  [[AtMinimum]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4244
    bitrate = 5

  [[BelowMinimum]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4245
    bitrate = 4

  [[Plain]]
    type = TCPClientInterface
    target_host = 127.0.0.1
    target_port = 4246
"#,
        )
        .unwrap();

        assert_eq!(
            config.interfaces.get("Fast").unwrap().bitrate,
            Some(1200),
            "bitrate >= MINIMUM_BITRATE is honoured"
        );
        assert_eq!(
            config.interfaces.get("AtMinimum").unwrap().bitrate,
            Some(5),
            "bitrate exactly at MINIMUM_BITRATE is honoured"
        );
        assert_eq!(
            config.interfaces.get("BelowMinimum").unwrap().bitrate,
            None,
            "bitrate below MINIMUM_BITRATE is ignored (stays unset)"
        );
        assert_eq!(
            config.interfaces.get("Plain").unwrap().bitrate,
            None,
            "an unset bitrate key stays None"
        );
    }

    #[test]
    fn test_skip_unsupported_interface_types() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Auto]]
    type = AutoInterface
    enabled = Yes

  [[RNode]]
    type = RNodeInterface
    port = /dev/ttyACM0

  [[TCP Server]]
    type = TCPServerInterface
    enabled = yes
    listen_port = 4242

  [[Unknown]]
    type = SomeFutureInterface
    peers = somebase64.b32.i2p
"#,
        )
        .unwrap();

        // A genuinely-unknown interface type is skipped; Auto, RNode, TCP are
        // supported. (I2PInterface is now supported and covered separately.)
        assert_eq!(config.interfaces.len(), 3);
        assert!(config.interfaces.contains_key("TCP Server"));
        assert!(config.interfaces.contains_key("Auto"));
        assert!(config.interfaces.contains_key("RNode"));
        assert!(!config.interfaces.contains_key("Unknown"));
    }

    #[test]
    fn test_parse_auto_interface_all_params() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Auto Interface]]
    type = AutoInterface
    enabled = yes
    group_id = my_network
    discovery_scope = site
    discovery_port = 30000
    data_port = 40000
    devices = eth0, wlan0
    ignored_devices = docker0
"#,
        )
        .unwrap();

        let auto = config.interfaces.get("Auto Interface").expect("auto iface");
        assert_eq!(auto.interface_type, "AutoInterface");
        assert!(auto.enabled);
        assert_eq!(auto.group_id, Some("my_network".to_string()));
        assert_eq!(auto.discovery_scope, Some("site".to_string()));
        assert_eq!(auto.discovery_port, Some(30000));
        assert_eq!(auto.data_port, Some(40000));
        assert_eq!(auto.devices, Some("eth0, wlan0".to_string()));
        assert_eq!(auto.ignored_devices, Some("docker0".to_string()));
    }

    #[test]
    fn test_parse_auto_interface_defaults() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Auto]]
    type = AutoInterface
"#,
        )
        .unwrap();

        let auto = config.interfaces.get("Auto").expect("auto iface");
        assert_eq!(auto.interface_type, "AutoInterface");
        assert!(auto.enabled); // default
        assert_eq!(auto.group_id, None);
        assert_eq!(auto.discovery_scope, None);
        assert_eq!(auto.discovery_port, None);
        assert_eq!(auto.data_port, None);
        assert_eq!(auto.devices, None);
        assert_eq!(auto.ignored_devices, None);
    }

    #[test]
    fn test_parse_two_auto_interface_sections() {
        // Codeberg #7: two AutoInterface sections (distinct group_id + distinct
        // discovery_port/data_port) parse into two independent configs.
        let config = parse_ini(
            r#"
[interfaces]
  [[Auto Home]]
    type = AutoInterface
    group_id = home_net
    discovery_port = 29716
    data_port = 42671

  [[Auto Lab]]
    type = AutoInterface
    group_id = lab_net
    discovery_port = 30000
    data_port = 43000
"#,
        )
        .unwrap();

        let home = config.interfaces.get("Auto Home").expect("home iface");
        assert_eq!(home.interface_type, "AutoInterface");
        assert_eq!(home.group_id, Some("home_net".to_string()));
        assert_eq!(home.discovery_port, Some(29716));
        assert_eq!(home.data_port, Some(42671));

        let lab = config.interfaces.get("Auto Lab").expect("lab iface");
        assert_eq!(lab.interface_type, "AutoInterface");
        assert_eq!(lab.group_id, Some("lab_net".to_string()));
        assert_eq!(lab.discovery_port, Some(30000));
        assert_eq!(lab.data_port, Some(43000));

        // Both sections are present as distinct AutoInterface configs.
        let auto_count = config
            .interfaces
            .values()
            .filter(|c| c.interface_type == "AutoInterface")
            .count();
        assert_eq!(auto_count, 2);
    }

    #[test]
    fn test_parse_udp_interface() {
        let config = parse_ini(
            r#"
[interfaces]
  [[UDP Interface]]
    type = UDPInterface
    enabled = yes
    listen_ip = 0.0.0.0
    listen_port = 4242
    forward_ip = 192.168.1.255
    forward_port = 4242
"#,
        )
        .unwrap();

        let udp = config.interfaces.get("UDP Interface").expect("udp");
        assert_eq!(udp.interface_type, "UDPInterface");
        assert!(udp.enabled);
        assert_eq!(udp.listen_ip, Some("0.0.0.0".to_string()));
        assert_eq!(udp.listen_port, Some(4242));
        assert_eq!(udp.forward_ip, Some("192.168.1.255".to_string()));
        assert_eq!(udp.forward_port, Some(4242));
    }

    #[test]
    fn test_comments_and_whitespace() {
        let config = parse_ini(
            r#"
# This is a comment
[reticulum]
  # Another comment
  enable_transport = True
  share_instance = No

[interfaces]
  # Commented out interface
  # [[Disabled]]
  #   type = TCPClientInterface

  [[Active]]
    type = TCPServerInterface
    enabled = yes
    listen_port = 1234
"#,
        )
        .unwrap();

        assert!(config.reticulum.enable_transport);
        assert!(!config.reticulum.shared_instance);
        assert_eq!(config.interfaces.len(), 1);
    }

    #[test]
    fn test_disabled_interface() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Disabled Server]]
    type = TCPServerInterface
    enabled = No
    listen_port = 4242
"#,
        )
        .unwrap();

        let server = config.interfaces.get("Disabled Server").expect("server");
        assert!(!server.enabled);
    }

    #[test]
    fn test_empty_config() {
        let config = parse_ini("").unwrap();
        assert!(
            config.reticulum.enable_transport,
            "empty config should default enable_transport to true"
        );
        assert!(config.interfaces.is_empty());
    }

    #[test]
    fn test_reticulum_section_only() {
        let config = parse_ini(
            r#"
[reticulum]
  enable_transport = False
  share_instance = Yes
"#,
        )
        .unwrap();

        assert!(!config.reticulum.enable_transport);
        assert!(config.reticulum.shared_instance);
        assert!(config.interfaces.is_empty());
    }

    #[test]
    fn test_flush_interval_parsed() {
        let config = parse_ini(
            r#"
[reticulum]
  flush_interval = 600
"#,
        )
        .unwrap();
        assert_eq!(config.reticulum.flush_interval_secs, 600);
    }

    #[test]
    fn test_flush_interval_default_when_absent() {
        let config = parse_ini("[reticulum]\n  enable_transport = True\n").unwrap();
        assert_eq!(
            config.reticulum.flush_interval_secs,
            crate::config::DEFAULT_FLUSH_INTERVAL_SECS,
            "absence of flush_interval must keep the 3600 s default"
        );
    }

    #[test]
    fn test_flush_interval_unparseable_keeps_default() {
        let config = parse_ini("[reticulum]\n  flush_interval = often\n").unwrap();
        assert_eq!(
            config.reticulum.flush_interval_secs,
            crate::config::DEFAULT_FLUSH_INTERVAL_SECS
        );
    }

    #[test]
    fn test_parse_logging_loglevel() {
        // Codeberg #89-adjacent drop-in: the [logging] section is no longer
        // dropped; `loglevel = 6` parses into the struct field.
        let config = parse_ini("[logging]\n  loglevel = 6\n").unwrap();
        assert_eq!(config.reticulum.loglevel, Some(6));

        // Absent -> None; lnsd then falls back to its default filter.
        let none = parse_ini("[reticulum]\n  enable_transport = True\n").unwrap();
        assert_eq!(none.reticulum.loglevel, None);

        // peek_loglevel reads the same value without a full parse.
        assert_eq!(peek_loglevel("[logging]\n  loglevel = 6\n"), Some(6));
        assert_eq!(peek_loglevel("[reticulum]\n  loglevel = 6\n"), None);
    }

    #[test]
    fn test_peek_loglevel_strips_inline_comment() {
        // An inline comment on the loglevel line must not defeat the parse, or
        // the daemon silently runs at info instead of the configured level.
        assert_eq!(
            peek_loglevel("[logging]\n  loglevel = 6  # verbose\n"),
            Some(6)
        );
    }

    #[test]
    fn test_parse_use_implicit_proof() {
        // RNS default is true (Reticulum.py:575-578); an explicit `No` on the
        // INI path must disable it.
        let off = parse_ini("[reticulum]\n  use_implicit_proof = No\n").unwrap();
        assert!(!off.reticulum.use_implicit_proof);

        // Absent key keeps the default true.
        let default = parse_ini("[reticulum]\n  enable_transport = True\n").unwrap();
        assert!(default.reticulum.use_implicit_proof);
    }

    #[test]
    fn test_respond_to_probes_default_false() {
        let config = parse_ini("[reticulum]\n").unwrap();
        assert!(!config.reticulum.respond_to_probes);
    }

    #[test]
    fn test_respond_to_probes_enabled() {
        let config = parse_ini(
            r#"
[reticulum]
  respond_to_probes = Yes
"#,
        )
        .unwrap();
        assert!(config.reticulum.respond_to_probes);
    }

    #[test]
    fn test_remote_management_default_off() {
        let config = parse_ini("[reticulum]\n").unwrap();
        assert!(!config.reticulum.remote_management_enabled);
        assert!(config.reticulum.remote_management_allowed.is_empty());
    }

    #[test]
    fn test_remote_management_python_keys() {
        // The upstream rnsd spelling `enable_remote_management` plus the
        // comma-separated `remote_management_allowed` list (ConfigObj as_list).
        let config = parse_ini(
            r#"
[reticulum]
  enable_remote_management = Yes
  remote_management_allowed = aabbccddeeff00112233445566778899, 00112233445566778899aabbccddeeff
"#,
        )
        .unwrap();
        assert!(config.reticulum.remote_management_enabled);
        assert_eq!(
            config.reticulum.remote_management_allowed,
            vec![
                "aabbccddeeff00112233445566778899".to_string(),
                "00112233445566778899aabbccddeeff".to_string(),
            ]
        );
    }

    #[test]
    fn test_remote_management_enabled_alias() {
        // `remote_management_enabled` is accepted as a Leviculum-side alias.
        let config = parse_ini("[reticulum]\n  remote_management_enabled = True\n").unwrap();
        assert!(config.reticulum.remote_management_enabled);
    }

    #[test]
    fn test_instance_name_parsed() {
        let config = parse_ini(
            r#"
[reticulum]
  share_instance = Yes
  instance_name = miauhaus
"#,
        )
        .unwrap();
        assert!(config.reticulum.shared_instance);
        assert_eq!(config.reticulum.instance_name, "miauhaus");
    }

    #[test]
    fn test_instance_name_defaults_to_default() {
        let config = parse_ini("[reticulum]\n").unwrap();
        assert_eq!(config.reticulum.instance_name, "default");
    }

    #[test]
    fn test_network_identity_parsed() {
        // Codeberg #32 sub-task d: private discovery network identity path.
        let config = parse_ini(
            r#"
[reticulum]
  network_identity = ~/.reticulum/network_identity
"#,
        )
        .unwrap();
        assert_eq!(
            config.reticulum.network_identity,
            Some(std::path::PathBuf::from("~/.reticulum/network_identity"))
        );
    }

    #[test]
    fn test_network_identity_defaults_to_none() {
        let config = parse_ini("[reticulum]\n").unwrap();
        assert!(config.reticulum.network_identity.is_none());
    }

    #[test]
    fn test_shared_instance_type_tcp_overrides_socket_path() {
        // RNS 1.3.x semantic: shared_instance_type = tcp wins over a
        // configured shared_instance_socket path (tcp disables AF_UNIX),
        // regardless of key order in the file.
        let config = parse_ini(
            r#"
[reticulum]
  share_instance = Yes
  shared_instance_socket = /run/reticulum/custom.sock
  shared_instance_type = tcp
"#,
        )
        .unwrap();
        assert!(config.reticulum.shared_instance);
        assert_eq!(config.reticulum.shared_instance_type, Some("tcp".into()));
        assert_eq!(
            config.reticulum.shared_instance_socket, None,
            "tcp must override (clear) a configured socket path"
        );
    }

    #[test]
    fn test_shared_instance_ports_parsed() {
        // Codeberg #112: both TCP-loopback ports parse into config instead of
        // falling into the unknown-key catch-all (previously silently dropped).
        let config = parse_ini(
            r#"
[reticulum]
  shared_instance_port = 37500
  instance_control_port = 37501
"#,
        )
        .unwrap();
        assert_eq!(config.reticulum.shared_instance_port, Some(37500));
        assert_eq!(config.reticulum.instance_control_port, Some(37501));
    }

    #[test]
    fn test_shared_instance_ports_default_none() {
        let config = parse_ini("[reticulum]\n").unwrap();
        assert_eq!(config.reticulum.shared_instance_port, None);
        assert_eq!(config.reticulum.instance_control_port, None);
    }

    #[test]
    fn test_shared_instance_port_out_of_range_left_unset() {
        // 70000 does not fit u16; a bad value must not abort the parse, and the
        // key stays unset (Python casts int() but our bind wants a u16 port).
        let config = parse_ini(
            r#"
[reticulum]
  shared_instance_port = 70000
"#,
        )
        .unwrap();
        assert_eq!(config.reticulum.shared_instance_port, None);
    }

    #[test]
    fn test_discovery_encrypt_parsed() {
        // Codeberg #112: `discovery_encrypt` reaches the interface field the
        // announcer reads, rather than being dropped by the catch-all.
        let config = parse_ini(
            r#"
[interfaces]
  [[Private TCP]]
    type = TCPServerInterface
    listen_ip = 0.0.0.0
    listen_port = 4242
    discoverable = yes
    discovery_encrypt = yes
"#,
        )
        .unwrap();
        let iface = config
            .interfaces
            .values()
            .find(|c| c.discoverable)
            .expect("discoverable interface");
        assert!(iface.discovery_encrypt);
    }

    #[test]
    fn test_discovery_encrypt_defaults_false() {
        let config = parse_ini(
            r#"
[interfaces]
  [[Public TCP]]
    type = TCPServerInterface
    listen_ip = 0.0.0.0
    listen_port = 4242
    discoverable = yes
"#,
        )
        .unwrap();
        let iface = config
            .interfaces
            .values()
            .find(|c| c.discoverable)
            .expect("discoverable interface");
        assert!(!iface.discovery_encrypt);
    }

    #[test]
    fn test_discovery_encrypt_end_to_end_from_config() {
        // Functional (Codeberg #112): a `discovery_encrypt = yes` interface
        // sourced entirely from a config file yields an ENCRYPTED announce that
        // only a matching network identity decodes; a foreign identity and the
        // plaintext parser both reject it. This exercises the config -> field ->
        // descriptor -> encrypted-announce path the daemon's announcer runs.
        use leviculum_core::discovery::{
            build_announce_app_data_encrypted, parse_announce_app_data,
            parse_announce_app_data_decrypt, MIN_REQUIRED_STAMP_VALUE,
        };
        use leviculum_core::identity::Identity;

        let config = parse_ini(
            r#"
[interfaces]
  [[Private RNode]]
    type = RNodeInterface
    discoverable = yes
    discovery_encrypt = yes
    discovery_name = secret
    frequency = 868000000
    bandwidth = 125000
    spreadingfactor = 8
    codingrate = 5
"#,
        )
        .unwrap();
        let iface = config
            .interfaces
            .values()
            .find(|c| c.discoverable)
            .expect("discoverable interface");
        assert!(iface.discovery_encrypt, "config must enable encryption");

        let desc =
            crate::discovery::descriptor_from_config(iface).expect("descriptor for RNode iface");

        let mut rng = rand_core::OsRng;
        let net_id = Identity::generate(&mut rng);
        let transport_id = [0u8; 16];
        let network_id = [0u8; 16];

        let app = build_announce_app_data_encrypted(&desc, &transport_id, true, &net_id, &mut rng)
            .expect("built encrypted announce");

        // Matching identity decodes it.
        assert!(
            parse_announce_app_data_decrypt(&app, &network_id, MIN_REQUIRED_STAMP_VALUE, &net_id)
                .is_some(),
            "matching network identity must decode the encrypted announce"
        );
        // A foreign identity does not.
        let other = Identity::generate(&mut rng);
        assert!(
            parse_announce_app_data_decrypt(&app, &network_id, MIN_REQUIRED_STAMP_VALUE, &other)
                .is_none(),
            "foreign identity must not decode the announce"
        );
        // Neither does the plaintext parser.
        assert!(
            parse_announce_app_data(&app, &network_id, MIN_REQUIRED_STAMP_VALUE).is_none(),
            "plaintext parser must reject an encrypted announce"
        );
    }

    #[test]
    fn test_shared_instance_socket_kept_when_type_unix() {
        let config = parse_ini(
            r#"
[reticulum]
  shared_instance_socket = /run/reticulum/custom.sock
  shared_instance_type = unix
"#,
        )
        .unwrap();
        assert_eq!(config.reticulum.shared_instance_type, Some("unix".into()));
        assert_eq!(
            config.reticulum.shared_instance_socket,
            Some("/run/reticulum/custom.sock".into())
        );
    }

    #[test]
    fn test_shared_instance_socket_kept_when_type_absent() {
        let config = parse_ini(
            r#"
[reticulum]
  shared_instance_socket = /run/reticulum/custom.sock
"#,
        )
        .unwrap();
        assert_eq!(config.reticulum.shared_instance_type, None);
        assert_eq!(
            config.reticulum.shared_instance_socket,
            Some("/run/reticulum/custom.sock".into())
        );
    }

    #[test]
    fn test_shared_instance_type_invalid_value_ignored() {
        // Mirror upstream: only tcp/unix are accepted; anything else is
        // tolerated but not stored.
        let config = parse_ini("[reticulum]\n  shared_instance_type = bogus\n").unwrap();
        assert_eq!(config.reticulum.shared_instance_type, None);
    }

    #[test]
    fn test_tolerate_all_rns_13x_new_keys() {
        // A config carrying every new 1.2.2..1.3.5 key (reticulum-level,
        // logging section, and per-interface) must parse without error.
        // None of these features are implemented; they are tolerate-only.
        let config = parse_ini(
            r#"
[reticulum]
  enable_transport = True
  blackhole_update_interval = 3600
  default_ar_target = 0.5
  default_ar_penalty = 2.0
  default_ar_grace = 60
  ic_max_held_announces = 10
  ic_burst_hold = 5.0
  ic_burst_freq_new = 3.5
  ic_burst_freq = 12.0
  ic_pr_burst_freq_new = 3.5
  ic_pr_burst_freq = 12.0
  ec_pr_freq = 1.0
  egress_control = True
  ic_new_time = 2.0
  ic_burst_penalty = 5.0
  ic_held_release_interval = 60

[logging]
  logtimestamps = True

[interfaces]
  [[Tolerant TCP]]
    type = TCPServerInterface
    enabled = yes
    listen_port = 4242
    egress_control = True
    ic_pr_burst_freq_new = 3.5
    ic_pr_burst_freq = 12.0
    ec_pr_freq = 1.0
"#,
        )
        .unwrap();

        // Known keys still take effect; the new keys are ignored cleanly.
        assert!(config.reticulum.enable_transport);
        let iface = config.interfaces.get("Tolerant TCP").expect("iface");
        assert_eq!(iface.interface_type, "TCPServerInterface");
        assert_eq!(iface.listen_port, Some(4242));
    }

    #[test]
    fn test_parse_rnode_interface() {
        let config = parse_ini(
            r#"
[interfaces]
  [[My RNode]]
    type = RNodeInterface
    port = /dev/ttyACM0
    frequency = 868000000
    bandwidth = 125000
    spreadingfactor = 7
    codingrate = 5
    txpower = 17
    flow_control = true
    airtime_limit_short = 15.0
    airtime_limit_long = 5.0
"#,
        )
        .unwrap();

        let rnode = config.interfaces.get("My RNode").expect("rnode iface");
        assert_eq!(rnode.interface_type, "RNodeInterface");
        assert_eq!(rnode.port, Some("/dev/ttyACM0".to_string()));
        assert_eq!(rnode.frequency, Some(868_000_000));
        assert_eq!(rnode.bandwidth, Some(125_000));
        assert_eq!(rnode.spreading_factor, Some(7));
        assert_eq!(rnode.coding_rate, Some(5));
        assert_eq!(rnode.tx_power, Some(17));
        assert_eq!(rnode.flow_control, Some(true));
        assert_eq!(rnode.airtime_limit_short, Some(15.0));
        assert_eq!(rnode.airtime_limit_long, Some(5.0));
    }

    // --- Backbone drop-in (Codeberg #89) -----------------------------------
    // BackboneInterface is wire-identical to TCPInterface; a stock rnsd config
    // that uses `type = BackboneInterface` must load unchanged and normalize
    // onto our TCP interface exactly as Python does (Reticulum.py:960-972).

    #[test]
    fn test_backbone_interface_server_listen() {
        // `type = BackboneInterface` WITHOUT a target host -> TCP server.
        // `port` -> listen_port, `listen_on` -> listen_ip.
        let config = parse_ini(
            r#"
[interfaces]
  [[Backbone Listen]]
    type = BackboneInterface
    listen_on = 0.0.0.0
    port = 4242
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Backbone Listen").expect("iface");
        assert_eq!(
            iface.interface_type, "TCPServerInterface",
            "a Backbone entry with no target host must become a TCP server"
        );
        assert_eq!(iface.listen_ip, Some("0.0.0.0".to_string()));
        assert_eq!(iface.listen_port, Some(4242));
        // `port` is a TCP port here, not a serial device path -> consumed.
        assert_eq!(iface.port, None);
        assert_eq!(iface.target_host, None);
    }

    #[test]
    fn test_backbone_interface_client_via_remote() {
        // `type = BackboneInterface` WITH `remote` -> TCP client.
        // `port` -> target_port, `remote` -> target_host.
        let config = parse_ini(
            r#"
[interfaces]
  [[Backbone Uplink]]
    type = BackboneInterface
    remote = backbone.example.com
    port = 4965
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Backbone Uplink").expect("iface");
        assert_eq!(
            iface.interface_type, "TCPClientInterface",
            "a Backbone entry with a remote host must become a TCP client"
        );
        assert_eq!(iface.target_host, Some("backbone.example.com".to_string()));
        assert_eq!(iface.target_port, Some(4965));
        assert_eq!(iface.port, None);
    }

    #[test]
    fn test_backbone_client_interface_always_client() {
        // `type = BackboneClientInterface` is always a client, even though the
        // reference config below only carries `remote`/`port`.
        let config = parse_ini(
            r#"
[interfaces]
  [[Backbone Client]]
    type = BackboneClientInterface
    remote = 127.0.0.1
    port = 7822
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Backbone Client").expect("iface");
        assert_eq!(iface.interface_type, "TCPClientInterface");
        assert_eq!(iface.target_host, Some("127.0.0.1".to_string()));
        assert_eq!(iface.target_port, Some(7822));
    }

    #[test]
    fn test_backbone_uses_explicit_tcp_keys() {
        // A Backbone entry may also carry the literal TCP keys instead of the
        // `port`/`remote`/`listen_on` aliases; presence of target_host still
        // dispatches to client (Reticulum.py:967).
        let config = parse_ini(
            r#"
[interfaces]
  [[Backbone Explicit]]
    type = BackboneInterface
    target_host = 10.0.0.1
    target_port = 4242
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Backbone Explicit").expect("iface");
        assert_eq!(iface.interface_type, "TCPClientInterface");
        assert_eq!(iface.target_host, Some("10.0.0.1".to_string()));
        assert_eq!(iface.target_port, Some(4242));
    }

    #[test]
    fn test_stock_rnsd_default_config_loads() {
        // Config drop-in audit (Codeberg #89): the verbatim stock rnsd default
        // config (Reticulum.py __default_rns_config__) must load unchanged --
        // known [reticulum] keys take effect, [logging] is ignored, the default
        // AutoInterface is accepted.
        let config = parse_ini(
            r#"[reticulum]
enable_transport = False
share_instance = Yes
instance_name = default

[logging]
loglevel = 4

[interfaces]
  [[Default Interface]]
    type = AutoInterface
    enabled = Yes
"#,
        )
        .unwrap();

        assert!(!config.reticulum.enable_transport);
        assert!(config.reticulum.shared_instance);
        assert_eq!(config.reticulum.instance_name, "default");
        let auto = config.interfaces.get("Default Interface").expect("auto");
        assert_eq!(auto.interface_type, "AutoInterface");
        assert!(auto.enabled);
    }

    #[test]
    fn test_example_config_interface_type_coverage() {
        // Config drop-in audit (Codeberg #89): `rnsd --exampleconfig` documents
        // eight interface types, all of which we now accept. No key on any block
        // causes the config to fail to load.
        let config = parse_ini(
            r#"
[interfaces]
  [[Default Interface]]
    type = AutoInterface
    enabled = Yes
  [[UDP Interface]]
    type = UDPInterface
    enabled = no
    listen_ip = 0.0.0.0
    listen_port = 4242
    forward_ip = 255.255.255.255
    forward_port = 4242
  [[TCP Server Interface]]
    type = TCPServerInterface
    enabled = no
    listen_ip = 0.0.0.0
    listen_port = 4242
    device = eth0
    connectable = yes
  [[TCP Client Interface]]
    type = TCPClientInterface
    enabled = no
    target_host = 127.0.0.1
    target_port = 4242
  [[I2P]]
    type = I2PInterface
    enabled = no
    connectable = yes
    peers = ykzlw5ujbaqc2xkec4cpvgyxj257wcrmmgkuxqmqcur7cq3w3lha.b32.i2p
  [[RNode LoRa Interface]]
    type = RNodeInterface
    enabled = no
    port = /dev/ttyUSB0
    frequency = 867200000
    bandwidth = 125000
    txpower = 7
    spreadingfactor = 8
    codingrate = 5
    id_callsign = MYCALL-0
    id_interval = 600
  [[Packet Radio KISS Interface]]
    type = KISSInterface
    enabled = no
    port = /dev/ttyUSB1
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
    preamble = 150
    txtail = 10
    persistence = 200
    slottime = 20
  [[Packet Radio AX.25 KISS Interface]]
    type = AX25KISSInterface
    enabled = no
    callsign = MYCALL
    ssid = 0
    port = /dev/ttyUSB2
    speed = 115200
    databits = 8
    parity = none
    stopbits = 1
"#,
        )
        .unwrap();

        // Accepted interface types survive the load.
        for accepted in [
            "Default Interface",
            "UDP Interface",
            "TCP Server Interface",
            "TCP Client Interface",
            "I2P",
            "RNode LoRa Interface",
            "Packet Radio KISS Interface",
            "Packet Radio AX.25 KISS Interface",
        ] {
            assert!(
                config.interfaces.contains_key(accepted),
                "{accepted} must be accepted"
            );
        }

        // The I2P block's `peers`/`connectable` keys parse into typed fields.
        let i2p = config.interfaces.get("I2P").expect("I2P");
        assert_eq!(i2p.connectable, Some(true));
        assert_eq!(
            i2p.peers.as_deref(),
            Some(&["ykzlw5ujbaqc2xkec4cpvgyxj257wcrmmgkuxqmqcur7cq3w3lha.b32.i2p".to_string()][..])
        );
    }

    #[test]
    fn test_interface_enabled_legacy_alias() {
        // Config drop-in audit fix: the legacy `interface_enabled` key must
        // disable an interface just like `enabled` (Reticulum.py:950).
        let config = parse_ini(
            r#"
[interfaces]
  [[Legacy Off]]
    type = TCPServerInterface
    interface_enabled = No
    listen_port = 4242
"#,
        )
        .unwrap();
        let iface = config.interfaces.get("Legacy Off").expect("iface");
        assert!(
            !iface.enabled,
            "interface_enabled = No must disable the interface"
        );
    }

    #[test]
    fn test_backbone_with_unknown_extra_keys_accepted() {
        // Backbone-only knobs (prioritise, network_name, ifac_*, device binds)
        // must not reject the interface -- they are logged and ignored.
        let config = parse_ini(
            r#"
[interfaces]
  [[Backbone Rich]]
    type = BackboneInterface
    listen_on = 0.0.0.0
    port = 4242
    prioritise = eth0
    ifac_netname = mynet
    ifac_netkey = supersecret
    some_future_backbone_knob = 1234
"#,
        )
        .unwrap();

        let iface = config.interfaces.get("Backbone Rich").expect("iface");
        assert_eq!(iface.interface_type, "TCPServerInterface");
        assert_eq!(iface.listen_port, Some(4242));
        assert_eq!(iface.listen_ip, Some("0.0.0.0".to_string()));
    }

    #[test]
    fn test_parse_discovery_producer_keys() {
        // Codeberg #109: the discovery producer keys must parse from the config
        // FILE, not just the builder. Before the arms were added they fell into
        // the unknown-key catch-all and were silently dropped, so
        // `descriptor_from_config` always saw `discoverable == false` and
        // returned None.
        let config = parse_ini(
            r#"
[interfaces]
  [[Advertised Server]]
    type = TCPServerInterface
    listen_ip = 0.0.0.0
    listen_port = 4242
    discoverable = yes
    reachable_on = 1.2.3.4
    discovery_name = My Node
    announce_interval = 30

  [[Fast Test Server]]
    type = TCPServerInterface
    listen_ip = 0.0.0.0
    listen_port = 4343
    discoverable = yes
    discovery_announce_interval_secs = 5
"#,
        )
        .unwrap();

        let adv = config.interfaces.get("Advertised Server").expect("iface");
        assert!(adv.discoverable);
        assert_eq!(adv.reachable_on, Some("1.2.3.4".to_string()));
        assert_eq!(adv.discovery_name, Some("My Node".to_string()));
        assert_eq!(adv.announce_interval, Some(30));
        assert_eq!(adv.discovery_announce_interval_secs, None);

        // The parse now feeds the producer path: a config-file-driven interface
        // yields a discovery descriptor (returned None before the arms existed).
        let desc =
            crate::discovery::descriptor_from_config(adv).expect("descriptor for discoverable");
        assert_eq!(desc.interface_type, "TCPServerInterface");
        assert_eq!(desc.name, Some("My Node".to_string()));
        assert_eq!(desc.reachable_on, Some("1.2.3.4".to_string()));
        assert_eq!(desc.port, Some(4242));

        // `announce_interval` minutes -> seconds (no floor hit at 30 min).
        assert_eq!(
            crate::discovery::resolve_announce_interval_secs(adv),
            30 * 60
        );

        // The Leviculum raw-seconds override parses and wins over the floor.
        let fast = config.interfaces.get("Fast Test Server").expect("iface");
        assert!(fast.discoverable);
        assert_eq!(fast.discovery_announce_interval_secs, Some(5));
        assert_eq!(crate::discovery::resolve_announce_interval_secs(fast), 5);
        assert!(crate::discovery::descriptor_from_config(fast).is_some());
    }
}
