pub const MOSH_PROTOCOL_VERSION: u32 = 2;

pub const ADDED_BYTES: usize = 12; // 8 nonce + 4 timestamps
pub const SESSION_ADDED_BYTES: usize = 16; // OCB tag
pub const FRAG_HEADER_LEN: usize = 10; // 8 id + 2 combined_fragment_num

pub const SEND_INTERVAL_MIN_MS: u64 = 20;
pub const SEND_INTERVAL_MAX_MS: u64 = 250;
pub const ACK_INTERVAL_MS: u64 = 3000;
pub const ACK_DELAY_MS: u64 = 100;
pub const SEND_MINDELAY_MS: u64 = 8;
pub const ACTIVE_RETRY_TIMEOUT_MS: u64 = 10000;

pub const MIN_RTO_MS: u64 = 50;
pub const MAX_RTO_MS: u64 = 1000;

pub const PORT_RANGE_LOW: u16 = 60001;
pub const PORT_RANGE_HIGH: u16 = 60999;
pub const PORT_HOP_INTERVAL_MS: u64 = 10000;
pub const MAX_PORTS_OPEN: usize = 10;
pub const MAX_OLD_SOCKET_AGE_MS: u64 = 60000;
pub const SERVER_ASSOCIATION_TIMEOUT_MS: u64 = 40000;

pub const SHUTDOWN_RETRIES: u32 = 16;

pub const CHAFF_MAX: usize = 16;

pub const CONGESTION_TIMESTAMP_PENALTY_MS: u64 = 500;

pub const MTU_IPV4: usize = 1252; // 1280 - 28 (20 IP + 8 UDP)
pub const MTU_IPV6: usize = 1216; // 1280 - 64 (40 IP + 16 ext + 8 UDP)
pub const MTU_FALLBACK: usize = 500;

pub fn max_fragment_payload(mtu: usize) -> usize {
    mtu.saturating_sub(ADDED_BYTES + SESSION_ADDED_BYTES + FRAG_HEADER_LEN)
}
