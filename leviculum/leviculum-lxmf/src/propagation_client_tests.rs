use super::*;
use alloc::collections::VecDeque;
use core::cell::Cell;
use leviculum_core::resource::{ResourceError, RESOURCE_MAX_EFFICIENT_SIZE};
use leviculum_core::{
    Action, InterfaceId, MemoryStorage, NodeCoreBuilder, ProofStrategy, RequestPolicy,
    ResourceStrategy,
};
use rand_core::OsRng;

struct TestClock(Cell<u64>);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

fn identity(seed: u8) -> Identity {
    let mut private = [0u8; 64];
    for (index, byte) in private.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    Identity::from_private_key_bytes(&private).expect("valid deterministic identity")
}

fn setup() -> (
    NodeCore<OsRng, TestClock, MemoryStorage>,
    PropagationTransport,
) {
    let mut node = NodeCoreBuilder::new().build(
        OsRng,
        TestClock(Cell::new(1_000)),
        MemoryStorage::with_defaults(),
    );
    let destination = PropagationTransport::destination(identity(7)).unwrap();
    assert!(!destination.accepts_links());
    let transport = PropagationTransport::register(&mut node, destination).unwrap();
    assert!(!node
        .destination(&transport.destination_hash())
        .unwrap()
        .accepts_links());
    (node, transport)
}

fn take_packets(actions: Vec<Action>) -> Vec<Vec<u8>> {
    actions
        .into_iter()
        .map(|action| match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
        })
        .collect()
}

fn absorb_client(
    node: &mut NodeCore<OsRng, TestClock, MemoryStorage>,
    transport: &mut PropagationTransport,
    core: TickOutput,
    events: &mut Vec<PropagationTransportEvent>,
) -> Vec<Vec<u8>> {
    let mut actions = core.actions;
    let mut core_events: VecDeque<_> = core.events.into();
    while let Some(event) = core_events.pop_front() {
        let follow_up = transport.handle_event(node, &event).unwrap();
        events.extend(follow_up.events);
        actions.extend(follow_up.core.actions);
        core_events.extend(follow_up.core.events);
    }
    take_packets(actions)
}

fn receive_client(
    node: &mut NodeCore<OsRng, TestClock, MemoryStorage>,
    transport: &mut PropagationTransport,
    packets: Vec<Vec<u8>>,
    events: &mut Vec<PropagationTransportEvent>,
) -> Vec<Vec<u8>> {
    let mut outbound = Vec::new();
    for packet in packets {
        let core = node.handle_packet(InterfaceId(0), &packet);
        outbound.extend(absorb_client(node, transport, core, events));
    }
    outbound
}

fn receive_server(
    node: &mut NodeCore<OsRng, TestClock, MemoryStorage>,
    packets: Vec<Vec<u8>>,
    events: &mut Vec<NodeEvent>,
) -> Vec<Vec<u8>> {
    let mut outbound = Vec::new();
    for packet in packets {
        let core = node.handle_packet(InterfaceId(0), &packet);
        events.extend(core.events);
        outbound.extend(take_packets(core.actions));
    }
    outbound
}

fn pump(
    client: &mut NodeCore<OsRng, TestClock, MemoryStorage>,
    transport: &mut PropagationTransport,
    server: &mut NodeCore<OsRng, TestClock, MemoryStorage>,
    mut to_server: Vec<Vec<u8>>,
    mut to_client: Vec<Vec<u8>>,
    client_events: &mut Vec<PropagationTransportEvent>,
    server_events: &mut Vec<NodeEvent>,
) {
    for _ in 0..256 {
        if !to_server.is_empty() {
            to_client.extend(receive_server(
                server,
                core::mem::take(&mut to_server),
                server_events,
            ));
        }
        if !to_client.is_empty() {
            to_server.extend(receive_client(
                client,
                transport,
                core::mem::take(&mut to_client),
                client_events,
            ));
        }
        if to_server.is_empty() && to_client.is_empty() {
            return;
        }
    }
    panic!("propagation transport exchange did not quiesce");
}

#[test]
fn client_destination_never_accepts_incoming_links() {
    let (_node, transport) = setup();
    assert_ne!(transport.destination_hash().into_bytes(), [0; 16]);
}

#[test]
fn unknown_node_without_path_enters_path_requested_state() {
    let (mut node, mut transport) = setup();
    let remote = DestinationHash::new([0x21; 16]);
    let output = transport
        .ensure_link(&mut node, remote)
        .expect("a missing key must not block the initial path request");
    assert!(!output.core.actions.is_empty());
    assert!(transport.wanted_links.contains(&remote));
    assert_eq!(transport.active_link(&remote), None);
}

#[test]
fn maps_only_outgoing_client_state() {
    let (mut node, mut transport) = setup();
    let remote = DestinationHash::new([0x31; 16]);
    let link_id = LinkId::new([0x42; 16]);
    transport.link_destinations.insert(link_id, remote);
    transport.pending_links.insert(remote, link_id);

    let output = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkEstablished {
                link_id,
                is_initiator: true,
                destination_hash: remote,
            },
        )
        .unwrap();
    assert!(matches!(
        output.events.as_slice(),
        [PropagationTransportEvent::LinkEstablished {
            destination,
            link_id: established,
        }] if *destination == remote && *established == link_id
    ));

    let request_id = [0x53; 16];
    transport.pending_requests.insert(
        request_id,
        PendingRequest {
            kind: PropagationRequestKind::Get,
            link_id,
            request_resource_hash: None,
        },
    );
    let progress = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceProgress {
                link_id,
                resource_hash: [0x64; 32],
                progress: 0.5,
                transfer_size: 1_000,
                data_size: 2_000,
                is_sender: false,
            },
        )
        .unwrap();
    assert!(matches!(
        progress.events.as_slice(),
        [PropagationTransportEvent::RequestProgress {
            kind: PropagationRequestKind::Get,
            progress,
            transfer_size: 1_000,
            ..
        }] if *progress == 0.5
    ));

    let response = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResponseReceived {
                link_id,
                request_id,
                response_data: alloc::vec![0xc3],
                metadata: None,
            },
        )
        .unwrap();
    assert!(matches!(
        response.events.as_slice(),
        [PropagationTransportEvent::ResponseReceived {
            kind: PropagationRequestKind::Get,
            data,
            ..
        }] if data == &[0xc3]
    ));

    // Server/raw-link events are intentionally outside this adapter.
    let ignored = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDataReceived {
                link_id,
                data: alloc::vec![1, 2, 3],
            },
        )
        .unwrap();
    assert!(ignored.events.is_empty());
    assert!(ignored.core.actions.is_empty());

    let closed = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkClosed {
                link_id,
                reason: LinkCloseReason::PeerClosed,
                is_initiator: true,
                destination_hash: remote,
            },
        )
        .unwrap();
    assert!(matches!(
        closed.events.as_slice(),
        [PropagationTransportEvent::LinkClosed {
            destination,
            reason: LinkCloseReason::PeerClosed,
            ..
        }] if *destination == remote
    ));
    assert_eq!(transport.active_link(&remote), None);
}

#[test]
fn upload_representation_uses_python_lxmf_cutoff() {
    assert_eq!(
        upload_representation(LINK_PACKET_MAX_CONTENT),
        PropagationUploadRepresentation::Packet
    );
    assert_eq!(
        upload_representation(LINK_PACKET_MAX_CONTENT + 1),
        PropagationUploadRepresentation::Resource
    );
}

#[test]
fn packet_upload_proof_and_timeout_are_correlated_once() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x61; 16]);
    let message_id = [0x62; 32];
    let packet_hash = [0x63; 32];
    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Packet { packet_hash },
        },
    );
    assert_eq!(transport.active_upload(&link_id), Some(message_id));
    assert!(transport.has_active_upload(&message_id));

    let unrelated = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDeliveryConfirmed {
                link_id,
                packet_hash: [0x64; 32],
            },
        )
        .unwrap();
    assert!(unrelated.events.is_empty());

    let completed = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDeliveryConfirmed {
                link_id,
                packet_hash,
            },
        )
        .unwrap();
    assert!(matches!(
        completed.events.as_slice(),
        [PropagationTransportEvent::UploadCompleted {
            link_id: completed_link,
            message_id: completed_message,
        }] if *completed_link == link_id && *completed_message == message_id
    ));
    assert_eq!(transport.active_upload(&link_id), None);

    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Packet { packet_hash },
        },
    );
    let timed_out = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDeliveryFailed {
                link_id,
                packet_hash,
            },
        )
        .unwrap();
    assert!(matches!(
        timed_out.events.as_slice(),
        [PropagationTransportEvent::UploadFailed {
            link_id: failed_link,
            message_id: failed_message,
            reason: PropagationUploadFailure::PacketTimeout,
        }] if failed_link == &link_id && failed_message == &message_id
    ));
    assert!(transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDeliveryFailed {
                link_id,
                packet_hash,
            },
        )
        .unwrap()
        .events
        .is_empty());
}

#[test]
fn upload_invalid_stamp_signal_rejects_only_an_active_upload() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x65; 16]);
    let message_id = [0x66; 32];
    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Packet {
                packet_hash: [0x67; 32],
            },
        },
    );

    let malformed = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDataReceived {
                link_id,
                data: alloc::vec![0x91, 0xcc, 0xf4],
            },
        )
        .unwrap();
    assert!(malformed.events.is_empty());
    assert_eq!(transport.active_upload(&link_id), Some(message_id));

    let rejected = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkDataReceived {
                link_id,
                data: alloc::vec![0x91, 0xcc, 0xf5],
            },
        )
        .unwrap();
    assert!(matches!(
        rejected.events.as_slice(),
        [PropagationTransportEvent::UploadRejected {
            link_id: rejected_link,
            message_id: rejected_message,
            signal: PropagationSignal::InvalidStamp,
        }] if *rejected_link == link_id && *rejected_message == message_id
    ));
    assert_eq!(transport.active_upload(&link_id), None);
}

#[test]
fn resource_upload_progress_completion_and_failure_are_sender_side() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x68; 16]);
    let message_id = [0x69; 32];
    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Resource,
        },
    );

    let progress = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceProgress {
                link_id,
                resource_hash: [0x6a; 32],
                progress: 0.75,
                transfer_size: 4_000,
                data_size: 4_000,
                is_sender: true,
            },
        )
        .unwrap();
    assert!(matches!(
        progress.events.as_slice(),
        [PropagationTransportEvent::UploadProgress {
            message_id: progress_message,
            progress,
            ..
        }] if *progress_message == message_id && *progress == 0.75
    ));

    let intermediate = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceCompleted {
                link_id,
                resource_hash: [0x6a; 32],
                data: Vec::new(),
                metadata: None,
                is_sender: true,
                segment_index: 1,
                total_segments: 2,
            },
        )
        .unwrap();
    assert!(intermediate.events.is_empty());
    assert_eq!(transport.active_upload(&link_id), Some(message_id));

    let completed = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceCompleted {
                link_id,
                resource_hash: [0x6b; 32],
                data: Vec::new(),
                metadata: None,
                is_sender: true,
                segment_index: 2,
                total_segments: 2,
            },
        )
        .unwrap();
    assert!(matches!(
        completed.events.as_slice(),
        [PropagationTransportEvent::UploadCompleted {
            message_id: completed_message,
            ..
        }] if *completed_message == message_id
    ));

    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Resource,
        },
    );
    let failed = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceFailed {
                link_id,
                resource_hash: [0x6c; 32],
                error: ResourceError::Timeout,
                is_sender: true,
            },
        )
        .unwrap();
    assert!(matches!(
        failed.events.as_slice(),
        [PropagationTransportEvent::UploadFailed {
            message_id: failed_message,
            reason: PropagationUploadFailure::Resource(ResourceError::Timeout),
            ..
        }] if *failed_message == message_id
    ));
}

#[test]
fn closing_an_upload_link_fails_then_reports_the_close() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x6d; 16]);
    let destination = DestinationHash::new([0x6e; 16]);
    let message_id = [0x6f; 32];
    transport.link_destinations.insert(link_id, destination);
    transport.active_links.insert(destination, link_id);
    transport.pending_uploads.insert(
        link_id,
        PendingUpload {
            message_id,
            transfer: PendingUploadTransfer::Resource,
        },
    );

    let output = transport
        .handle_event(
            &mut node,
            &NodeEvent::LinkClosed {
                link_id,
                reason: LinkCloseReason::PeerClosed,
                is_initiator: true,
                destination_hash: destination,
            },
        )
        .unwrap();
    assert!(matches!(
        output.events.as_slice(),
        [
            PropagationTransportEvent::UploadFailed {
                message_id: failed_message,
                reason: PropagationUploadFailure::LinkClosed(LinkCloseReason::PeerClosed),
                ..
            },
            PropagationTransportEvent::LinkClosed {
                destination: closed_destination,
                ..
            }
        ] if *failed_message == message_id && *closed_destination == destination
    ));
    assert_eq!(transport.active_upload(&link_id), None);
    assert!(!transport.owns_link(&link_id));
}

#[test]
fn request_resource_sender_failure_is_correlated_once() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x71; 16]);
    let request_id = [0x72; 16];
    let resource_hash = [0x73; 32];
    transport.pending_requests.insert(
        request_id,
        PendingRequest {
            kind: PropagationRequestKind::Get,
            link_id,
            request_resource_hash: Some(resource_hash),
        },
    );

    let failed = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error: ResourceError::Timeout,
                is_sender: true,
            },
        )
        .unwrap();
    assert!(matches!(
        failed.events.as_slice(),
        [PropagationTransportEvent::RequestTimedOut {
            link_id: failed_link,
            kind: PropagationRequestKind::Get,
        }] if *failed_link == link_id
    ));
    assert!(!transport.pending_requests.contains_key(&request_id));

    // Core also emits its generic request-failure notification. Since the
    // Resource event already concluded the semantic request, it is ignored.
    let duplicate = transport
        .handle_event(
            &mut node,
            &NodeEvent::RequestTimedOut {
                link_id,
                request_id,
            },
        )
        .unwrap();
    assert!(duplicate.events.is_empty());
}

#[test]
fn request_resource_sender_completion_clears_only_upload_correlation() {
    let (mut node, mut transport) = setup();
    let link_id = LinkId::new([0x81; 16]);
    let request_id = [0x82; 16];
    let resource_hash = [0x83; 32];
    transport.pending_requests.insert(
        request_id,
        PendingRequest {
            kind: PropagationRequestKind::List,
            link_id,
            request_resource_hash: Some(resource_hash),
        },
    );

    let completed = transport
        .handle_event(
            &mut node,
            &NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                data: Vec::new(),
                metadata: None,
                is_sender: true,
                segment_index: 1,
                total_segments: 1,
            },
        )
        .unwrap();
    assert!(completed.events.is_empty());
    let pending = transport
        .pending_requests
        .get(&request_id)
        .expect("completion keeps the request pending for its response");
    assert_eq!(pending.request_resource_hash, None);
}

#[test]
fn large_get_and_response_use_resource_fallback_end_to_end() {
    let (mut client, mut transport) = setup();
    let mut server = NodeCoreBuilder::new().build(
        OsRng,
        TestClock(Cell::new(1_000)),
        MemoryStorage::with_defaults(),
    );
    let server_identity = identity(91);
    let server_public_identity =
        Identity::from_public_key_bytes(&server_identity.public_key_bytes()).unwrap();
    let mut server_destination = Destination::new(
        Some(server_identity),
        Direction::In,
        DestinationType::Single,
        APP_NAME,
        &[PROPAGATION_ASPECT],
    )
    .unwrap();
    server_destination.set_accepts_links(true);
    let server_hash = *server_destination.hash();
    server.register_destination(server_destination);
    server.register_request_handler(server_hash, MESSAGE_GET_PATH, RequestPolicy::AllowAll);

    client.remember_identity(server_hash, server_public_identity);
    transport.wanted_links.insert(server_hash);
    let connected = transport.connect(&mut client, server_hash).unwrap();
    let mut client_events = Vec::new();
    let mut server_events = Vec::new();
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(connected.core.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    let link_id = transport
        .active_link(&server_hash)
        .expect("outgoing propagation link established");

    // Python would split a /get request above Resource.MAX_EFFICIENT_SIZE.
    // Core deliberately leaves that semantic reassembly for the deferred
    // split-request work, so LXMF must surface the explicit size error without
    // retaining a request correlation that can never complete.
    let mut split_sized_request = Vec::new();
    crate::msgpack::bin(
        &mut split_sized_request,
        &alloc::vec![0x5c; RESOURCE_MAX_EFFICIENT_SIZE],
    );
    assert!(matches!(
        transport.submit_mailbox_request(
            &mut client,
            link_id,
            &split_sized_request,
            PropagationRequestKind::Get,
            Some(60_000),
        ),
        Err(PropagationTransportError::Resource(
            ResourceError::ResourceTooLarge
        ))
    ));
    assert!(transport.pending_requests.is_empty());
    assert!(!client
        .link(&link_id)
        .expect("active propagation link")
        .has_outgoing_resource());

    let request_body: Vec<u8> = (0..1_400)
        .map(|index| ((index * 79 + 11) & 0xff) as u8)
        .collect();
    let mut encoded_request = Vec::new();
    crate::msgpack::bin(&mut encoded_request, &request_body);
    let request_output = transport
        .submit_mailbox_request(
            &mut client,
            link_id,
            &encoded_request,
            PropagationRequestKind::Get,
            None,
        )
        .unwrap();
    assert!(transport
        .pending_requests
        .values()
        .any(|pending| pending.request_resource_hash.is_some()));
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(request_output.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    assert!(transport
        .pending_requests
        .values()
        .all(|pending| pending.request_resource_hash.is_none()));

    let request_id = server_events
        .iter()
        .find_map(|event| match event {
            NodeEvent::RequestReceived {
                request_id, data, ..
            } if data == &encoded_request => Some(*request_id),
            _ => None,
        })
        .expect("large Resource request reached /get handler");

    // Enough incompressible data to cross the receiver window boundary,
    // proving that Resource progress is correlated to the pending `/get`.
    let mut state = 0x6d2b_79f5u32;
    let response_body: Vec<u8> = (0..32_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    let mut encoded_response = Vec::new();
    crate::msgpack::bin(&mut encoded_response, &response_body);
    let (_, response_output) = server
        .send_response_resource(
            &server_events
                .iter()
                .find_map(|event| match event {
                    NodeEvent::RequestReceived { link_id, .. } => Some(*link_id),
                    _ => None,
                })
                .unwrap(),
            &request_id,
            &encoded_response,
        )
        .unwrap();
    pump(
        &mut client,
        &mut transport,
        &mut server,
        Vec::new(),
        take_packets(response_output.actions),
        &mut client_events,
        &mut server_events,
    );

    assert!(client_events.iter().any(|event| matches!(
        event,
        PropagationTransportEvent::RequestProgress {
            kind: PropagationRequestKind::Get,
            ..
        }
    )));
    assert!(client_events.iter().any(|event| matches!(
        event,
        PropagationTransportEvent::ResponseReceived {
            kind: PropagationRequestKind::Get,
            data,
            ..
        } if data == &encoded_response
    )));
}

#[test]
fn packet_and_resource_uploads_complete_end_to_end_without_offer_endpoint() {
    let (mut client, mut transport) = setup();
    let mut server = NodeCoreBuilder::new().build(
        OsRng,
        TestClock(Cell::new(1_000)),
        MemoryStorage::with_defaults(),
    );
    let server_identity = identity(101);
    let server_public_identity =
        Identity::from_public_key_bytes(&server_identity.public_key_bytes()).unwrap();
    let mut server_destination = Destination::new(
        Some(server_identity),
        Direction::In,
        DestinationType::Single,
        APP_NAME,
        &[PROPAGATION_ASPECT],
    )
    .unwrap();
    server_destination.set_accepts_links(true);
    server_destination.set_proof_strategy(ProofStrategy::All);
    let server_hash = *server_destination.hash();
    server.register_destination(server_destination);

    client.remember_identity(server_hash, server_public_identity);
    transport.wanted_links.insert(server_hash);
    let connected = transport.connect(&mut client, server_hash).unwrap();
    let mut client_events = Vec::new();
    let mut server_events = Vec::new();
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(connected.core.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    let client_link = transport.active_link(&server_hash).unwrap();
    let server_link = server_events
        .iter()
        .find_map(|event| match event {
            NodeEvent::LinkEstablished {
                link_id,
                is_initiator: false,
                ..
            } => Some(*link_id),
            _ => None,
        })
        .expect("server propagation Link established");
    server
        .set_resource_strategy(&server_link, ResourceStrategy::AcceptAll)
        .unwrap();

    client_events.clear();
    server_events.clear();
    let packet_data = alloc::vec![0x71; LINK_PACKET_MAX_CONTENT];
    let packet_message_id = [0x72; 32];
    let submitted = transport
        .submit_upload(&mut client, client_link, packet_message_id, &packet_data)
        .unwrap();
    assert!(matches!(
        submitted.events.as_slice(),
        [PropagationTransportEvent::UploadSubmitted {
            representation: PropagationUploadRepresentation::Packet,
            ..
        }]
    ));
    assert!(matches!(
        transport.submit_upload(&mut client, client_link, [0x73; 32], b"busy"),
        Err(PropagationTransportError::UploadInProgress)
    ));
    client_events.extend(submitted.events);
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(submitted.core.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    assert!(client_events.iter().any(|event| matches!(
        event,
        PropagationTransportEvent::UploadCompleted { message_id, .. }
            if message_id == &packet_message_id
    )));
    assert!(server_events.iter().any(|event| matches!(
        event,
        NodeEvent::LinkDataReceived { data, .. } if data == &packet_data
    )));

    client_events.clear();
    server_events.clear();
    let resource_data = alloc::vec![0x74; LINK_PACKET_MAX_CONTENT + 1];
    let resource_message_id = [0x75; 32];
    let submitted = transport
        .submit_upload(
            &mut client,
            client_link,
            resource_message_id,
            &resource_data,
        )
        .unwrap();
    assert!(matches!(
        submitted.events.as_slice(),
        [PropagationTransportEvent::UploadSubmitted {
            representation: PropagationUploadRepresentation::Resource,
            ..
        }]
    ));
    client_events.extend(submitted.events);
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(submitted.core.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    assert!(client_events.iter().any(|event| matches!(
        event,
        PropagationTransportEvent::UploadCompleted { message_id, .. }
            if message_id == &resource_message_id
    )));
    assert!(server_events.iter().any(|event| matches!(
        event,
        NodeEvent::ResourceCompleted {
            data,
            is_sender: false,
            ..
        } if data == &resource_data
    )));

    // The same upload built off the caller's lock: a second build path that
    // must put the identical bytes on the wire.
    client_events.clear();
    server_events.clear();
    let phased_data = alloc::vec![0x76; LINK_PACKET_MAX_CONTENT + 1];
    let phased_message_id = [0x77; 32];
    let params = transport
        .upload_send_params(&client, client_link, phased_message_id, phased_data.len())
        .unwrap();
    let prepared = params.build(&phased_data, &mut OsRng).unwrap();
    let submitted = transport.commit_upload(&mut client, prepared).unwrap();
    assert!(matches!(
        submitted.events.as_slice(),
        [PropagationTransportEvent::UploadSubmitted {
            representation: PropagationUploadRepresentation::Resource,
            ..
        }]
    ));
    client_events.extend(submitted.events);
    pump(
        &mut client,
        &mut transport,
        &mut server,
        take_packets(submitted.core.actions),
        Vec::new(),
        &mut client_events,
        &mut server_events,
    );
    assert!(client_events.iter().any(|event| matches!(
        event,
        PropagationTransportEvent::UploadCompleted { message_id, .. }
            if message_id == &phased_message_id
    )));
    assert!(server_events.iter().any(|event| matches!(
        event,
        NodeEvent::ResourceCompleted {
            data,
            is_sender: false,
            ..
        } if data == &phased_data
    )));
}
