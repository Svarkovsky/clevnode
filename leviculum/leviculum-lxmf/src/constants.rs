/// Per-transfer limit for message propagation, in kilobytes: Python
/// `PROPAGATION_LIMIT` (`reference/LXMF/LXMF/LXMRouter.py:55`).
///
/// No consumer in this crate yet. It binds in the propagation-node hosting
/// paths this crate does not implement: announce field 3
/// (`reference/LXMF/LXMF/LXMRouter.py:331`) and the incoming sync-resource
/// refusal in `propagation_resource_advertised`
/// (`reference/LXMF/LXMF/LXMRouter.py:2206`). The client-side `/get` request
/// limit is the delivery limit instead, see
/// [`DELIVERY_LIMIT_KB`].
pub const PROPAGATION_LIMIT_KB: u64 = 256;

/// Per-transfer limit for one LXMF delivery, in kilobytes: Python
/// `DELIVERY_LIMIT` (`reference/LXMF/LXMF/LXMRouter.py:60`).
pub const DELIVERY_LIMIT_KB: u64 = 1000;

/// The delivery limit in bytes, converted exactly as the reference applies
/// it: a receiver refuses an advertised delivery Resource strictly above
/// `delivery_per_transfer_limit*1000` uncompressed bytes
/// (`reference/LXMF/LXMF/LXMRouter.py:1979`). Exactly at the limit is
/// accepted; the comparison is `size > limit`.
pub const DELIVERY_LIMIT_BYTES: u64 = DELIVERY_LIMIT_KB * 1000;

pub const DESTINATION_LENGTH: usize = 16;
pub const SIGNATURE_LENGTH: usize = 64;
pub const TICKET_LENGTH: usize = 16;
pub const STAMP_SIZE: usize = 32;
pub const LXMF_OVERHEAD: usize = 112;
pub const ENCRYPTED_PACKET_MAX_CONTENT: usize = 295;
pub const LINK_PACKET_MAX_CONTENT: usize = 319;
pub const PLAIN_PACKET_MAX_CONTENT: usize = 368;
pub const PAPER_MDU: usize = 2210;
pub const COST_TICKET: u16 = 256;
pub const TICKET_EXPIRY: u64 = 1_814_400;
pub const TICKET_GRACE: u64 = 432_000;
pub const TICKET_RENEW: u64 = 1_209_600;
pub const TICKET_INTERVAL: u64 = 86_400;
pub const STAMP_COST_EXPIRY: u64 = 45 * 24 * 60 * 60;
pub const WORKBLOCK_EXPAND_ROUNDS: usize = 3000;
/// Workblock expansion rounds for the outer propagation-node stamp.
pub const WORKBLOCK_EXPAND_ROUNDS_PN: usize = 1000;

pub const FIELD_EMBEDDED_LXMS: i64 = 0x01;
pub const FIELD_TELEMETRY: i64 = 0x02;
pub const FIELD_TELEMETRY_STREAM: i64 = 0x03;
pub const FIELD_ICON_APPEARANCE: i64 = 0x04;
pub const FIELD_FILE_ATTACHMENTS: i64 = 0x05;
pub const FIELD_IMAGE: i64 = 0x06;
pub const FIELD_AUDIO: i64 = 0x07;
pub const AUDIO_MODE_CUSTOM: u8 = 0xFF;
pub const FIELD_THREAD: i64 = 0x08;
pub const FIELD_COMMANDS: i64 = 0x09;
pub const FIELD_RESULTS: i64 = 0x0A;
pub const FIELD_COMMANDS_EXECUTED: i64 = FIELD_RESULTS;
pub const FIELD_GROUP: i64 = 0x0B;
pub const FIELD_TICKET: i64 = 0x0C;
pub const FIELD_EVENT: i64 = 0x0D;
pub const FIELD_RNR_REFS: i64 = 0x0E;
pub const FIELD_RENDERER: i64 = 0x0F;
pub const FIELD_CUSTOM_TYPE: i64 = 0xFB;
pub const FIELD_CUSTOM_DATA: i64 = 0xFC;
pub const FIELD_CUSTOM_META: i64 = 0xFD;
pub const FIELD_NON_SPECIFIC: i64 = 0xFE;
pub const FIELD_DEBUG: i64 = 0xFF;
pub const FIELD_REPLY_TO: i64 = 0x30;
pub const FIELD_REPLY_QUOTE: i64 = 0x31;
pub const FIELD_REACTION: i64 = 0x40;
pub const FIELD_COMMENT: i64 = 0x41;
pub const FIELD_CONTINUATION: i64 = 0x42;
