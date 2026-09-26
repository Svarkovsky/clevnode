//! Deterministic integration tests for the I2P interface against a mock SAM
//! bridge.
//!
//! These run in `just standard` with no i2pd: an in-process TCP listener speaks
//! just enough SAM v3 to drive the interface state machine, and splices a
//! client `STREAM CONNECT` to a server `STREAM ACCEPT` so a Reticulum packet
//! actually crosses a simulated I2P link end to end. This proves session setup,
//! the `SILENT=false` peer-destination handshake, HDLC framing in both
//! directions, and the `new_iface_tx` registration path — everything except the
//! live-network tunnel build, which is covered by the `#[ignore]` tests in
//! `i2pd_live.rs`.

use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use super::sam::{self, GOLDEN_B32, GOLDEN_PRIV, GOLDEN_PUB};
use super::*;

/// A minimal SAM v3 bridge for tests. Per connection it does the `HELLO`
/// handshake, then dispatches one command. `STREAM CONNECT` sockets are handed
/// to a rendezvous; `STREAM ACCEPT` sockets pull from it, write the connecting
/// peer's destination line (`SILENT=false` behaviour), and splice the two
/// sockets so bytes flow client <-> server.
async fn start_mock_sam() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Rendezvous between STREAM CONNECT (producer) and STREAM ACCEPT (consumer).
    let (connect_tx, connect_rx) = mpsc::unbounded_channel::<TcpStream>();
    let connect_rx = Arc::new(Mutex::new(connect_rx));

    tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let connect_tx = connect_tx.clone();
            let connect_rx = Arc::clone(&connect_rx);
            tokio::spawn(async move {
                // HELLO handshake.
                let hello = match sam::read_line(&mut sock).await {
                    Ok(l) => l,
                    Err(_) => return,
                };
                if !hello.starts_with("HELLO") {
                    return;
                }
                let _ = sock.write_all(b"HELLO REPLY RESULT=OK VERSION=3.1\n").await;

                // One command per connection.
                let cmd = match sam::read_line(&mut sock).await {
                    Ok(l) => l,
                    Err(_) => return,
                };

                if cmd.starts_with("SESSION CREATE") {
                    let reply = format!("SESSION STATUS RESULT=OK DESTINATION={GOLDEN_PRIV}\n");
                    let _ = sock.write_all(reply.as_bytes()).await;
                    // Hold the session socket open (drain to EOF).
                    let mut scratch = [0u8; 256];
                    loop {
                        match sock.read(&mut scratch).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                } else if cmd.starts_with("NAMING LOOKUP") {
                    let reply = format!("NAMING REPLY RESULT=OK NAME=x VALUE={GOLDEN_PUB}\n");
                    let _ = sock.write_all(reply.as_bytes()).await;
                } else if cmd.starts_with("STREAM CONNECT") {
                    let _ = sock.write_all(b"STREAM STATUS RESULT=OK\n").await;
                    // Hand the raw stream socket to a waiting STREAM ACCEPT.
                    let _ = connect_tx.send(sock);
                } else if cmd.starts_with("STREAM ACCEPT") {
                    let _ = sock.write_all(b"STREAM STATUS RESULT=OK\n").await;
                    // Wait for an inbound connection to splice with.
                    let mut connect_sock = {
                        let mut rx = connect_rx.lock().await;
                        match rx.recv().await {
                            Some(s) => s,
                            None => return,
                        }
                    };
                    // SILENT=false: deliver the connecting peer's destination
                    // line before the stream bytes.
                    let dest_line = format!("{GOLDEN_PUB}\n");
                    let _ = sock.write_all(dest_line.as_bytes()).await;
                    // Splice the two sockets together.
                    let _ = tokio::io::copy_bidirectional(&mut sock, &mut connect_sock).await;
                }
            });
        }
    });

    addr
}

fn out(data: &[u8]) -> OutgoingPacket {
    OutgoingPacket {
        peer: None,
        data: data.to_vec(),
        high_priority: false,
    }
}

/// End-to-end: bring up one server (`connectable`) and one client I2P
/// sub-interface against the mock, then prove a packet crosses the link in both
/// directions. Also asserts the server persists its SAM private key for a
/// stable `.b32.i2p` address.
#[tokio::test]
async fn announce_crosses_i2p_loopback() {
    let mock = start_mock_sam().await;
    let tmp = tempfile::tempdir().unwrap();
    let keyfile = tmp.path().join("i2p").join("srv.i2p");

    let (new_tx, mut new_rx) = mpsc::channel::<InterfaceHandle>(8);
    let next_id = Arc::new(AtomicUsize::new(100));

    spawn_i2p_server(I2pServerConfig {
        sam_address: mock.to_string(),
        keyfile: keyfile.clone(),
        buffer_size: 32,
        name_prefix: "srv".to_string(),
        reconnect_wait: Duration::from_millis(200),
        next_id,
        new_interface_tx: new_tx,
        ingress_control: false,
        ifac: None,
        outbound_socket_hook: None,
    });

    let mut client = spawn_i2p_client(I2pClientConfig {
        id: InterfaceId(1),
        name: "cli".to_string(),
        sam_address: mock.to_string(),
        peer: "somepeer.b32.i2p".to_string(),
        buffer_size: 32,
        reconnect_wait: Duration::from_millis(200),
        ifac: None,
        reconnect_notify: None,
        ingress_control: false,
        outbound_socket_hook: None,
    });

    // The server accept loop spawns an interface once the client connects.
    let mut server_iface = tokio::time::timeout(Duration::from_secs(5), new_rx.recv())
        .await
        .expect("timeout waiting for accepted I2P interface")
        .expect("new-interface channel closed");
    assert!(server_iface.info.name.starts_with("srv/"));
    assert_eq!(server_iface.info.hw_mtu, Some(1064));

    // Client -> server.
    client.outgoing.send(out(b"announce-c2s")).await.unwrap();
    let pkt = tokio::time::timeout(Duration::from_secs(5), server_iface.incoming.recv())
        .await
        .expect("timeout: client->server packet")
        .expect("server incoming closed");
    assert_eq!(pkt.data, b"announce-c2s");

    // Server -> client.
    server_iface
        .outgoing
        .send(out(b"announce-s2c"))
        .await
        .unwrap();
    let pkt = tokio::time::timeout(Duration::from_secs(5), client.incoming.recv())
        .await
        .expect("timeout: server->client packet")
        .expect("client incoming closed");
    assert_eq!(pkt.data, b"announce-s2c");

    // The server persisted its private key, so its address is stable across
    // restarts, and the key yields the golden b32.
    let saved = std::fs::read_to_string(&keyfile).unwrap();
    assert_eq!(saved.trim(), GOLDEN_PRIV);
    let dest = sam::Destination::from_private_base64(saved.trim()).unwrap();
    assert_eq!(dest.base32(), GOLDEN_B32);
}

/// A packet with a payload byte equal to the HDLC flag/escape must survive the
/// round trip (framing escapes it correctly on the SAM stream).
#[tokio::test]
async fn hdlc_escaping_survives_i2p_stream() {
    let mock = start_mock_sam().await;
    let (new_tx, mut new_rx) = mpsc::channel::<InterfaceHandle>(8);
    let next_id = Arc::new(AtomicUsize::new(200));
    let tmp = tempfile::tempdir().unwrap();

    spawn_i2p_server(I2pServerConfig {
        sam_address: mock.to_string(),
        keyfile: tmp.path().join("k.i2p"),
        buffer_size: 32,
        name_prefix: "srv".to_string(),
        reconnect_wait: Duration::from_millis(200),
        next_id,
        new_interface_tx: new_tx,
        ingress_control: false,
        ifac: None,
        outbound_socket_hook: None,
    });
    let client = spawn_i2p_client(I2pClientConfig {
        id: InterfaceId(1),
        name: "cli".to_string(),
        sam_address: mock.to_string(),
        peer: "peer.b32.i2p".to_string(),
        buffer_size: 32,
        reconnect_wait: Duration::from_millis(200),
        ifac: None,
        reconnect_notify: None,
        ingress_control: false,
        outbound_socket_hook: None,
    });

    let mut server_iface = tokio::time::timeout(Duration::from_secs(5), new_rx.recv())
        .await
        .expect("timeout waiting for accepted interface")
        .unwrap();

    // Payload full of FLAG (0x7E) and ESC (0x7D) bytes plus their escaped forms.
    let payload = vec![0x7e, 0x7d, 0x5e, 0x5d, 0x7e, 0x7e, 0x00, 0x7d];
    client.outgoing.send(out(&payload)).await.unwrap();
    let pkt = tokio::time::timeout(Duration::from_secs(5), server_iface.incoming.recv())
        .await
        .expect("timeout: escaped packet")
        .unwrap();
    assert_eq!(pkt.data, payload);
}

/// The outbound-socket hook fires for every TCP dial the I2P client makes
/// toward the SAM bridge: session control, NAMING LOOKUP (`.i2p` peer), and
/// the stream socket — so an embedder's socket policy covers the I2P
/// transport, not just plain TCP interfaces.
#[tokio::test]
async fn socket_hook_covers_every_sam_dial() {
    let mock = start_mock_sam().await;

    let dials = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&dials);
    let hook: crate::socket_hook::OutboundSocketHook = Arc::new(move |_fd| {
        recorded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });

    let handle = spawn_i2p_client(I2pClientConfig {
        id: InterfaceId(1),
        name: "cli".to_string(),
        sam_address: mock.to_string(),
        peer: "somepeer.b32.i2p".to_string(),
        buffer_size: 32,
        reconnect_wait: Duration::from_secs(30),
        ifac: None,
        reconnect_notify: None,
        ingress_control: false,
        outbound_socket_hook: Some(hook),
    });
    handle
        .ready
        .wait(Duration::from_secs(5))
        .await
        .expect("client should establish its stream against the mock SAM");

    assert_eq!(
        dials.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "hook must fire for the control, lookup, and stream dials"
    );
}

#[test]
fn keyfile_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("i2p").join("iface.i2p");
    assert!(load_keyfile(&path).is_none());
    save_keyfile(&path, GOLDEN_PRIV);
    assert_eq!(load_keyfile(&path).as_deref(), Some(GOLDEN_PRIV));
}
