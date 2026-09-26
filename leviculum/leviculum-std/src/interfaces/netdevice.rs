//! Resolve a configured kernel network interface name (the `device` config key)
//! to a concrete bind address.
//!
//! Mirrors Python-RNS `TCPServerInterface.get_address_for_if`
//! (TCPInterface.py:458-476, identical in BackboneInterface.py:64-82) and
//! `UDPInterface.get_broadcast_for_if` (UDPInterface.py:50-54). Binding to one
//! named NIC on a multi-homed host, rather than the wildcard address, is the
//! goal of Codeberg #94 (TCP/Backbone) and #3 (UDP).
//!
//! This lives in the interfaces layer because address selection is socket
//! setup: the core, transport, and daemon stay media-agnostic and never see a
//! `device` name.

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use if_addrs::{IfAddr, Interface};

/// Collect every address configured on the kernel interface `device`.
///
/// Errors with a message matching Python's `SystemError` text when the
/// interface has no addresses (or does not exist), so a bogus `device` name
/// fails cleanly at interface start instead of silently binding elsewhere.
fn addrs_for_device(device: &str) -> io::Result<Vec<Interface>> {
    let matching: Vec<Interface> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|i| i.name == device)
        .collect();
    if matching.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("No addresses available on specified kernel interface \"{device}\" to bind to"),
        ));
    }
    Ok(matching)
}

/// True for an IPv6 link-local address (`fe80::/10`), which needs the interface
/// index as its scope id to be usable as a bind address (Python's `%zone`
/// handling in `get_address_for_if`).
fn is_link_local_v6(addr: &Ipv6Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80
}

/// Resolve `device` to a TCP/Backbone listen bind address, matching
/// `TCPServerInterface.get_address_for_if` (TCPInterface.py:458-476).
///
/// Picks an IPv6 address when `prefer_ipv6` is set or the interface has no
/// IPv4 address; otherwise the first IPv4 address. A link-local IPv6 address
/// carries the interface index as its scope id, matching Python's `%zone`
/// handling. Errors when the interface has no usable address.
pub(crate) fn resolve_if_bind_address(
    device: &str,
    port: u16,
    prefer_ipv6: bool,
) -> io::Result<SocketAddr> {
    let ifaces = addrs_for_device(device)?;

    let v4 = ifaces.iter().find_map(|i| match &i.addr {
        IfAddr::V4(a) => Some(a.ip),
        IfAddr::V6(_) => None,
    });
    let v6 = ifaces.iter().find_map(|i| match &i.addr {
        IfAddr::V6(a) => Some((a.ip, i.index)),
        IfAddr::V4(_) => None,
    });

    // Python: `(prefer_ipv6 or not AF_INET in ifaddr) and AF_INET6 in ifaddr`.
    if prefer_ipv6 || v4.is_none() {
        if let Some((ip, index)) = v6 {
            let scope = if is_link_local_v6(&ip) {
                index.unwrap_or(0)
            } else {
                0
            };
            return Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope)));
        }
    }
    if let Some(ip) = v4 {
        return Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!("No addresses available on specified kernel interface \"{device}\" to bind to"),
    ))
}

/// Resolve `device` to its IPv4 broadcast address, matching
/// `UDPInterface.get_broadcast_for_if` (UDPInterface.py:50-54).
///
/// UDP broadcast is IPv4-only in Python-RNS, so `prefer_ipv6` does not apply.
/// Used to fill in `listen_ip` / `forward_ip` when a UDP interface names a
/// `device` but leaves those keys unset.
pub(crate) fn resolve_if_broadcast(device: &str) -> io::Result<IpAddr> {
    let ifaces = addrs_for_device(device)?;
    for i in &ifaces {
        if let IfAddr::V4(a) = &i.addr {
            if let Some(bcast) = a.broadcast {
                return Ok(IpAddr::V4(bcast));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        format!("No IPv4 broadcast address available on kernel interface \"{device}\""),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The loopback interface is a stable target for asserting the resolver
    // picks the right family without multi-NIC hardware — it always carries
    // 127.0.0.1 (v4) and ::1 (v6). But its *device name* is platform-specific
    // (`lo` on Linux, `lo0` on macOS, "Loopback Pseudo-Interface 1" on Windows),
    // so resolve it from the same enumeration the production code uses rather
    // than hardcoding the Linux name. This keeps the tests portable across the
    // macOS/Windows CI lanes and exercises the real resolver on each OS's own
    // loopback (`addrs_for_device` and this both go through `if_addrs`, so the
    // name is guaranteed to round-trip).
    fn loopback_device() -> String {
        if_addrs::get_if_addrs()
            .expect("enumerate interfaces")
            .into_iter()
            .find(|i| i.addr.ip().is_loopback())
            .map(|i| i.name)
            .expect("host must have a loopback interface")
    }

    #[test]
    fn resolves_loopback_to_v4_by_default() {
        let addr = resolve_if_bind_address(&loopback_device(), 4242, false)
            .expect("loopback should resolve to a bind address");
        assert_eq!(addr, "127.0.0.1:4242".parse().unwrap());
    }

    #[test]
    fn prefer_ipv6_selects_v6_on_loopback() {
        let addr = resolve_if_bind_address(&loopback_device(), 4242, true)
            .expect("loopback should resolve to a v6 bind address");
        assert_eq!(addr, "[::1]:4242".parse().unwrap());
    }

    #[test]
    fn bogus_device_fails_cleanly() {
        // A device that cannot exist must error, not fall back to a wildcard
        // bind (which would silently accept traffic on every NIC).
        let err = resolve_if_bind_address("nonexistent-nic-xyz", 4242, false)
            .expect_err("bogus device must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn resolved_loopback_address_is_bindable() {
        // Functional check: the resolved address is a real, bindable local
        // address (the socket is bound as configured, on the loopback).
        let addr = resolve_if_bind_address(&loopback_device(), 0, false).expect("resolve loopback");
        let listener = std::net::TcpListener::bind(addr).expect("bind resolved loopback address");
        assert_eq!(
            listener.local_addr().expect("local_addr").ip(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn broadcast_bogus_device_fails_cleanly() {
        let err = resolve_if_broadcast("nonexistent-nic-xyz")
            .expect_err("bogus device must error for broadcast");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
