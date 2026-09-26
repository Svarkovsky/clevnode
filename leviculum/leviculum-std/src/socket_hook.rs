//! Outbound-socket hook.
//!
//! Some embedders need to apply host-level policy to the node's outbound
//! sockets before they connect — bind them to a specific device, set a firewall
//! mark, or keep them out of a routing domain the host manages. The node
//! invokes an [`OutboundSocketHook`] with each freshly created connect socket,
//! before it dials. Registered via
//! [`ReticulumNodeBuilder::outbound_socket_hook`](crate::driver::ReticulumNodeBuilder::outbound_socket_hook).

use std::sync::Arc;

/// Callback invoked with each new outbound socket fd before it connects.
///
/// The fd is borrowed for the call only; the callback must not close it or
/// retain it past return.
#[cfg(unix)]
pub type OutboundSocketHook = Arc<dyn Fn(std::os::fd::RawFd) + Send + Sync>;

/// Callback invoked with each new outbound socket handle before it connects.
#[cfg(windows)]
pub type OutboundSocketHook = Arc<dyn Fn(std::os::windows::io::RawSocket) + Send + Sync>;

/// Create an outbound TCP socket for `addr`, run the hook against it, then
/// dial — so the hook sees the fd before connect.
pub(crate) async fn connect_hooked(
    addr: std::net::SocketAddr,
    hook: Option<&OutboundSocketHook>,
) -> std::io::Result<tokio::net::TcpStream> {
    let socket = match addr {
        std::net::SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        std::net::SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    apply(hook, &socket);
    socket.connect(addr).await
}

/// [`connect_hooked`] for a `host:port` string, resolved asynchronously — for
/// dialers that hold unresolved addresses (the I2P SAM bridge).
pub(crate) async fn connect_hooked_host(
    addr: &str,
    hook: Option<&OutboundSocketHook>,
) -> std::io::Result<tokio::net::TcpStream> {
    let sockaddr = tokio::net::lookup_host(addr).await?.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no addresses for {addr}"),
        )
    })?;
    connect_hooked(sockaddr, hook).await
}

/// Run the hook against a socket before connect, if one is registered.
pub(crate) fn apply(hook: Option<&OutboundSocketHook>, socket: &tokio::net::TcpSocket) {
    let Some(hook) = hook else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        hook(socket.as_raw_fd());
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        hook(socket.as_raw_socket());
    }
}
