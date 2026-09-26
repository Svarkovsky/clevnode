mod common;

use leviculum_lxmf::{
    announce::DeliveryAnnounce, MessageGetRequest, MessageGetResponse, MessageListResponse,
    PropagationNodeAnnounce, TransferLimit, TransientId, MESSAGE_GET_PATH,
};

fn id(start: u8) -> TransientId {
    let mut id = [0; 32];
    for (offset, byte) in id.iter_mut().enumerate() {
        *byte = start + offset as u8;
    }
    id
}

#[test]
fn python_delivery_announce_vector() {
    let vector = common::fixture("VEC-ANN-DELIVERY", "app_data_hex");
    let announce = DeliveryAnnounce {
        display_name: Some(b"Alice".to_vec()),
        stamp_cost: Some(8),
        compression_supported: true,
    };

    assert_eq!(hex::encode(announce.encode()), vector);
    assert_eq!(
        DeliveryAnnounce::decode(&hex::decode(vector).unwrap()).unwrap(),
        announce
    );
}

#[test]
fn python_get_list_download_and_acknowledgement_vectors() {
    assert_eq!(MESSAGE_GET_PATH, "/get");

    let list = MessageGetRequest::list();
    assert_eq!(
        hex::encode(list.encode().unwrap()),
        common::fixture("VEC-PROP-GET-LIST", "request_hex")
    );

    let download = MessageGetRequest {
        wants: Some(vec![id(32)]),
        haves: Some(vec![id(64)]),
        transfer_limit_kb: Some(TransferLimit::Integer(1000)),
    };
    let download_vector = common::fixture("VEC-PROP-GET-DOWNLOAD", "request_hex");
    assert_eq!(hex::encode(download.encode().unwrap()), download_vector);

    let acknowledgement = MessageGetRequest::acknowledge(vec![id(0)]);
    let acknowledgement_vector = common::fixture("VEC-PROP-GET-ACK", "request_hex");
    assert_eq!(
        hex::encode(acknowledgement.encode().unwrap()),
        acknowledgement_vector
    );

    let listing_vector = common::fixture("VEC-PROP-LIST-RESPONSE", "response_hex");
    let listing = MessageListResponse::TransientIds(vec![id(32), id(64)]);
    assert_eq!(
        MessageListResponse::decode(&hex::decode(listing_vector).unwrap()).unwrap(),
        listing
    );

    let messages_vector = common::fixture("VEC-PROP-GET-RESPONSE", "response_hex");
    let messages = MessageGetResponse::Messages(vec![b"one".to_vec(), b"two".to_vec()]);
    assert_eq!(
        MessageGetResponse::decode(&hex::decode(messages_vector).unwrap()).unwrap(),
        messages
    );
}

#[test]
fn python_propagation_node_announce_vector() {
    let announce = PropagationNodeAnnounce {
        legacy_support: false,
        timebase: 1_700_000_000,
        enabled: true,
        transfer_limit_kb: 256,
        sync_limit_kb: 10_240,
        stamp_cost: 16,
        stamp_cost_flexibility: 3,
        peering_cost: 18,
        metadata: vec![(1, vec![0xc4, 4, b'N', b'o', b'd', b'e'])],
    };
    let vector = common::fixture("VEC-ANN-PROPAGATION", "app_data_hex");

    assert_eq!(
        PropagationNodeAnnounce::decode(&hex::decode(vector).unwrap()).unwrap(),
        announce
    );
}
