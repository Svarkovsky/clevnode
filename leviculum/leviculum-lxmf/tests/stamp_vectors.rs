#![cfg(feature = "pow")]

mod common;

use futures::executor::block_on;
use leviculum_core::crypto::full_hash;
use leviculum_lxmf::constants::{WORKBLOCK_EXPAND_ROUNDS, WORKBLOCK_EXPAND_ROUNDS_PN};
use leviculum_lxmf::stamp::{
    valid, value, CooperativeStamper, ReadyYield, StampError, StampExecutor,
};
use leviculum_lxmf::{DeliveryStampRequest, PropagationStampRequest};
use rand_core::OsRng;
use std::{boxed::Box, future::Future, pin::Pin};

struct RecordingExecutor {
    material: [u8; 32],
    cost: u8,
    rounds: usize,
    result: [u8; 32],
}

impl StampExecutor for RecordingExecutor {
    fn generate<'a>(
        &'a mut self,
        material: &'a [u8],
        cost: u8,
        rounds: usize,
    ) -> Pin<Box<dyn Future<Output = Result<[u8; 32], StampError>> + Send + 'a>> {
        assert_eq!(material, self.material);
        assert_eq!(cost, self.cost);
        assert_eq!(rounds, self.rounds);
        Box::pin(core::future::ready(Ok(self.result)))
    }

    fn validate<'a>(
        &'a mut self,
        _: &'a [u8],
        _: &'a [u8; 32],
        _: u8,
        _: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Option<u16>, StampError>> + 'a>> {
        unreachable!("this executor only records generation")
    }
}

#[test]
fn python_stamp_workblock_vector() {
    let material: [u8; 32] = hex::decode(common::fixture("VEC-STAMP-1", "material_hex"))
        .unwrap()
        .try_into()
        .unwrap();
    let stamp: [u8; 32] = hex::decode(common::fixture("VEC-STAMP-1", "stamp_hex"))
        .unwrap()
        .try_into()
        .unwrap();
    let rounds = common::fixture("VEC-STAMP-1", "expand_rounds")
        .parse::<usize>()
        .unwrap();
    let cost = common::fixture("VEC-STAMP-1", "target_cost")
        .parse::<u8>()
        .unwrap();
    let expected_workblock_hash = common::fixture("VEC-STAMP-1", "workblock_sha256_hex");
    let mut stamper = CooperativeStamper::new(OsRng, ReadyYield);

    let workblock = block_on(stamper.workblock(&material, rounds));

    assert_eq!(hex::encode(full_hash(&workblock)), expected_workblock_hash);
    assert!(valid(&workblock, &stamp, cost));
    assert_eq!(
        value(&workblock, &stamp),
        common::fixture("VEC-STAMP-1", "stamp_value")
            .parse::<u16>()
            .unwrap()
    );
}

#[test]
fn python_propagation_stamp_uses_the_1000_round_workblock() {
    let material: [u8; 32] = hex::decode(common::fixture("VEC-STAMP-PN", "material_hex"))
        .unwrap()
        .try_into()
        .unwrap();
    let stamp: [u8; 32] = hex::decode(common::fixture("VEC-STAMP-PN", "stamp_hex"))
        .unwrap()
        .try_into()
        .unwrap();
    let rounds = common::fixture("VEC-STAMP-PN", "expand_rounds")
        .parse::<usize>()
        .unwrap();
    let cost = common::fixture("VEC-STAMP-PN", "target_cost")
        .parse::<u8>()
        .unwrap();
    let mut stamper = CooperativeStamper::new(OsRng, ReadyYield);

    assert_eq!(rounds, WORKBLOCK_EXPAND_ROUNDS_PN);
    let workblock = block_on(stamper.workblock(&material, rounds));

    assert_eq!(
        workblock.len().to_string(),
        common::fixture("VEC-STAMP-PN", "workblock_len")
    );
    assert_eq!(
        hex::encode(full_hash(&workblock)),
        common::fixture("VEC-STAMP-PN", "workblock_sha256_hex")
    );
    assert!(valid(&workblock, &stamp, cost));
    assert_eq!(
        value(&workblock, &stamp).to_string(),
        common::fixture("VEC-STAMP-PN", "stamp_value")
    );
}

#[test]
fn detached_requests_select_delivery_and_propagation_workblocks() {
    let delivery = DeliveryStampRequest {
        message_id: [0x31; 32],
        target_cost: 7,
    };
    let mut executor = RecordingExecutor {
        material: delivery.message_id,
        cost: delivery.target_cost,
        rounds: WORKBLOCK_EXPAND_ROUNDS,
        result: [0x41; 32],
    };
    assert_eq!(
        block_on(delivery.generate_with(&mut executor)).unwrap(),
        [0x41; 32]
    );

    let propagation = PropagationStampRequest {
        message_id: [0x32; 32],
        transient_id: [0x33; 32],
        target_cost: 9,
    };
    executor = RecordingExecutor {
        material: propagation.transient_id,
        cost: propagation.target_cost,
        rounds: WORKBLOCK_EXPAND_ROUNDS_PN,
        result: [0x42; 32],
    };
    assert_eq!(
        block_on(propagation.generate_with(&mut executor)).unwrap(),
        [0x42; 32]
    );
}
