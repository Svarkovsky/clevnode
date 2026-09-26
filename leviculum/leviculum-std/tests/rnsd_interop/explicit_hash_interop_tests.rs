//! Explicit-hash destination interop cells against real Python RNS (PR #254).
//!
//! `Destination::with_explicit_hash` indexes a Single destination under a
//! caller-supplied 16-byte hash. Such a destination must NEVER put anything
//! announce-shaped on a channel shared with Python peers: every Python-RNS
//! node validates announces by recomputing
//! `truncated_hash(name_hash || identity_hash)` and rejects a mismatch
//! (`Identity.validate_announce`, Identity.py:584-587). The reference answers
//! a path request for a local destination by regenerating the announce
//! (`Transport.path_request`, Transport.py:2940); for an explicit-hash
//! destination our answer must be silence.
//!
//! Two cells:
//!
//! * **positive** — while an explicit-hash destination is registered and
//!   receiving locally-routed traffic (a consenting peer links to it over the
//!   node's own TCP server), traffic to/from a NORMAL destination on the same
//!   node is unaffected: its announce propagates to the Python daemon and a
//!   Python-initiated link to it establishes.
//! * **negative** — over a shared channel with a Python transport peer, the
//!   explicit-hash destination never emits an announce: the driver announce
//!   API refuses, a wire observer on the node's own TCP server sees zero
//!   announce frames for the hash (while seeing the path response for a
//!   normal destination — the positive control that the observer WOULD catch
//!   a leak), and a path request for the hash — injected both directly and
//!   through the Python transport peer — produces no path response. The
//!   Python side never learns a path for the hash.
//!
//! Mutation contract: reverting the refusal (`explicit_hash: false` in
//! `with_explicit_hash` or removing the guard in `Destination::announce`)
//! must turn the negative cell red.

use std::net::SocketAddr;
use std::time::Duration;

use rand_core::OsRng;
use tokio::net::TcpStream;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::identity::Identity;
use leviculum_core::link::{Link, LinkState};
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::interfaces::hdlc::Deframer;

use crate::common::{
    build_path_request_raw_with_tag, init_tracing, receive_proof_for_link, send_framed,
    send_raw_to_daemon, temp_storage, wait_for_announce_for_dest,
    wait_for_node_reannounce_on_daemon, wait_for_responder_established_link,
};
use crate::harness::{find_available_ports, TestDaemon};

/// Build packet bytes for a raw LINK_REQUEST addressed to `dest_hash` and bind
/// the link to the resulting link id. Mirrors the reference wire format
/// (Type1 header, broadcast, Single, context None).
fn build_link_request_raw(link: &mut Link, dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Vec<u8> {
    use leviculum_core::packet::{
        HeaderType, PacketContext, PacketFlags, PacketType, TransportType,
    };

    let request_data = link.create_link_request();
    let flags = PacketFlags {
        ifac_flag: false,
        header_type: HeaderType::Type1,
        context_flag: false,
        transport_type: TransportType::Broadcast,
        dest_type: DestinationType::Single,
        packet_type: PacketType::LinkRequest,
    };
    let mut raw = Vec::with_capacity(85);
    raw.push(flags.to_byte());
    raw.push(0); // hops
    raw.extend_from_slice(dest_hash);
    raw.push(PacketContext::None.to_byte());
    raw.extend_from_slice(&request_data);
    let link_id = Link::calculate_link_id(&raw);
    link.set_link_id(link_id);
    raw
}

/// Registered pair on one node: a normal (derived-hash) destination and an
/// explicit-hash destination. Returns
/// `(normal_hash, normal_pubkey_hex, explicit_hash, explicit_signing_key)`.
fn make_dest_pair(
    node: &leviculum_std::driver::ReticulumNode,
    explicit_hash: [u8; TRUNCATED_HASHBYTES],
) -> (DestinationHash, String, DestinationHash, [u8; 32]) {
    let normal_identity = Identity::generate(&mut OsRng);
    let normal_pubkey_hex = hex::encode(normal_identity.public_key_bytes());
    let mut normal = Destination::new(
        Some(normal_identity),
        Direction::In,
        DestinationType::Single,
        "explicittest",
        &["normal"],
    )
    .expect("create normal destination");
    normal.set_accepts_links(true);
    let normal_hash = *normal.hash();

    let explicit_identity = Identity::generate(&mut OsRng);
    let explicit_signing_key = explicit_identity.ed25519_verifying().to_bytes();
    let mut explicit = Destination::with_explicit_hash(
        Some(explicit_identity),
        Direction::In,
        DestinationType::Single,
        "explicittest",
        &["federation"],
        explicit_hash,
    )
    .expect("create explicit-hash destination");
    explicit.set_accepts_links(true);
    let explicit_dest_hash = *explicit.hash();
    assert_eq!(explicit_dest_hash.as_bytes(), &explicit_hash);

    node.register_destination(normal);
    node.register_destination(explicit);
    (
        normal_hash,
        normal_pubkey_hex,
        explicit_dest_hash,
        explicit_signing_key,
    )
}

/// Positive cell: normal-destination traffic is unaffected while an
/// explicit-hash destination is registered and receiving locally-routed
/// traffic.
#[tokio::test]
async fn test_explicit_hash_registered_normal_traffic_unaffected() {
    init_tracing();
    let explicit_bytes: [u8; TRUNCATED_HASHBYTES] = [
        0xE1, 0x9C, 0x5A, 0x11, 0x84, 0x3B, 0xD2, 0x07, 0x6E, 0xF0, 0x21, 0x9D, 0x48, 0xC3, 0x55,
        0x0A,
    ];

    let daemon = TestDaemon::start().await.expect("start Python daemon");
    let (ports, _alloc) = find_available_ports::<2>().await.expect("alloc ports");
    let node_addr: SocketAddr = format!("127.0.0.1:{}", ports[0]).parse().unwrap();

    let _storage = temp_storage("explicit_hash_positive", "node");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .add_tcp_server(node_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    let mut event_rx = node.take_event_receiver().expect("event receiver");
    node.start().await.expect("start Rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (normal_hash, normal_pubkey_hex, explicit_hash, explicit_signing_key) =
        make_dest_pair(&node, explicit_bytes);

    // Normal destination announces and the Python daemon learns the path,
    // with the explicit-hash destination registered the whole time.
    node.announce_destination(&normal_hash, Some(b"explicit-test"))
        .await
        .expect("announce normal destination");
    let daemon_has_path = wait_for_node_reannounce_on_daemon(
        &daemon,
        &normal_hash,
        &node,
        b"explicit-test",
        Duration::from_secs(20),
    )
    .await;
    assert!(
        daemon_has_path,
        "Python daemon must learn the path to the NORMAL destination while \
         an explicit-hash destination is registered"
    );

    // Consenting peer: links to the explicit-hash destination over the node's
    // TCP server, knowing hash and identity out-of-band. This is the
    // locally-routed traffic the explicit destination exists for.
    let mut peer = TcpStream::connect(node_addr).await.expect("connect peer");
    let mut link = Link::new_outgoing(explicit_hash, &mut OsRng);
    link.set_destination_keys(&explicit_signing_key)
        .expect("set destination keys");
    let raw = build_link_request_raw(&mut link, explicit_hash.as_bytes());
    send_framed(&mut peer, &raw).await;

    let mut deframer = Deframer::new();
    let proof =
        receive_proof_for_link(&mut peer, &mut deframer, link.id(), Duration::from_secs(10))
            .await
            .expect(
                "the explicit-hash destination must prove the link request (bare table lookup)",
            );
    link.process_proof(proof.data.as_slice())
        .expect("LRPROOF must verify against the destination's real identity");
    assert_eq!(link.state(), LinkState::Active);

    // Python-initiated link to the NORMAL destination still establishes.
    let create_handle = {
        let cmd_addr = daemon.cmd_addr();
        let dh = hex::encode(normal_hash.as_bytes());
        tokio::spawn(async move {
            crate::common::create_link_raw(cmd_addr, &dh, &normal_pubkey_hex, 30).await
        })
    };
    let established =
        wait_for_responder_established_link(&mut event_rx, Duration::from_secs(30)).await;
    assert!(
        established.is_some(),
        "Rust node must auto-accept the Python link to the normal destination"
    );
    let create_result = create_handle.await.expect("join create_link task");
    assert!(
        create_result.is_ok(),
        "Python create_link to the normal destination must succeed: {create_result:?}"
    );

    // The explicit hash never became announce-visible on the Python side.
    assert!(
        !daemon.has_path(explicit_hash.as_bytes()).await,
        "Python daemon must have no path for the explicit hash"
    );

    node.stop().await.ok();
}

/// Negative cell: the explicit-hash destination never emits an announce on a
/// channel shared with a Python transport peer, and path requests for it get
/// no path response.
#[tokio::test]
async fn test_explicit_hash_never_announces_no_path_response() {
    init_tracing();
    let explicit_bytes: [u8; TRUNCATED_HASHBYTES] = [
        0xE2, 0x44, 0x71, 0xAB, 0x09, 0x5F, 0x36, 0xC8, 0xDD, 0x12, 0x8E, 0x60, 0xF7, 0x2B, 0x94,
        0x1C,
    ];

    let daemon = TestDaemon::start().await.expect("start Python daemon");
    let (ports, _alloc) = find_available_ports::<2>().await.expect("alloc ports");
    let node_addr: SocketAddr = format!("127.0.0.1:{}", ports[0]).parse().unwrap();

    let _storage = temp_storage("explicit_hash_negative", "node");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .add_tcp_server(node_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    node.start().await.expect("start Rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (normal_hash, _normal_pubkey_hex, explicit_hash, _explicit_signing_key) =
        make_dest_pair(&node, explicit_bytes);

    // The driver announce API refuses the explicit-hash destination.
    let refused = node.announce_destination(&explicit_hash, None).await;
    assert!(
        refused.is_err(),
        "announce_destination for an explicit-hash destination must refuse, got Ok"
    );

    // Channel control: the normal destination's announce DOES cross the
    // shared channel and the Python daemon learns it.
    node.announce_destination(&normal_hash, Some(b"neg-control"))
        .await
        .expect("announce normal destination");
    let daemon_has_path = wait_for_node_reannounce_on_daemon(
        &daemon,
        &normal_hash,
        &node,
        b"neg-control",
        Duration::from_secs(20),
    )
    .await;
    assert!(daemon_has_path, "control announce must reach the daemon");

    // Wire observer on the node's own TCP server. Every broadcast announce
    // reaches this interface, and a path response is targeted at the
    // interface the request came from — so this stream catches both leak
    // shapes.
    let mut observer = TcpStream::connect(node_addr)
        .await
        .expect("connect observer");
    let mut deframer = Deframer::new();

    // Positive control: a path request for the NORMAL destination is answered
    // with a fresh path-response announce on this very stream (Transport.py:
    // 2940 regenerates unconditionally; silence for the explicit hash below
    // is therefore the guard, not a mute responder).
    let pr_normal = build_path_request_raw_with_tag(normal_hash.as_bytes(), &[0xA1; 16]);
    send_framed(&mut observer, &pr_normal).await;
    let control = wait_for_announce_for_dest(
        &mut observer,
        &mut deframer,
        &normal_hash,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        control.is_some(),
        "positive control: path request for the normal destination must be \
         answered with a path response on the observer stream"
    );

    // Path request for the explicit hash, directly at the node: nothing
    // announce-shaped may come back.
    let pr_explicit = build_path_request_raw_with_tag(explicit_hash.as_bytes(), &[0xA2; 16]);
    send_framed(&mut observer, &pr_explicit).await;
    let leaked = wait_for_announce_for_dest(
        &mut observer,
        &mut deframer,
        &explicit_hash,
        Duration::from_secs(8),
    )
    .await;
    assert!(
        leaked.is_none(),
        "a path request for the explicit hash must be answered with SILENCE \
         (a path-response announce would be Python-unverifiable, \
         Identity.py:584-587)"
    );

    // Same request arriving through the Python transport peer on the shared
    // channel: still silence, and the daemon never learns a path.
    let pr_via_python = build_path_request_raw_with_tag(explicit_hash.as_bytes(), &[0xA3; 16]);
    send_raw_to_daemon(&daemon, &pr_via_python).await;
    let leaked_via_python = wait_for_announce_for_dest(
        &mut observer,
        &mut deframer,
        &explicit_hash,
        Duration::from_secs(8),
    )
    .await;
    assert!(
        leaked_via_python.is_none(),
        "no announce for the explicit hash may surface after a path request \
         relayed by the Python transport peer"
    );
    assert!(
        !daemon.has_path(explicit_hash.as_bytes()).await,
        "the Python side must never hold a path for the explicit hash"
    );

    node.stop().await.ok();
}
