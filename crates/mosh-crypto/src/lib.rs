mod base64_key;
mod nonce;
mod prng;
mod session;

pub use base64_key::Base64Key;
pub use nonce::Nonce;
pub use prng::Prng;
pub use session::{Message, Session};

/// Monotonic counter for nonce generation.
/// Each call returns the next sequence number, panicking on overflow.
pub fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let val = COUNTER.fetch_add(1, Ordering::Relaxed);
    assert!(val != u64::MAX, "Nonce counter wrapped");
    val
}
