use std::fmt;

/// 12-byte nonce for AES-128-OCB3.
///
/// Mosh encodes a 64-bit value into 12 bytes:
/// - First 4 bytes: zeros
/// - Last 8 bytes: the 64-bit value in big-endian
///
/// The 64-bit value packs a direction bit (bit 63) with a sequence number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Nonce([u8; Self::LEN]);

impl Nonce {
    pub const LEN: usize = 12;

    /// Create a nonce from a 64-bit value (direction + sequence number).
    pub fn from_val(val: u64) -> Self {
        let be = val.to_be_bytes();
        let mut bytes = [0u8; 12];
        bytes[4..].copy_from_slice(&be);
        Self(bytes)
    }

    /// Create a nonce from the 8-byte wire representation.
    pub fn from_bytes(s_bytes: &[u8]) -> Self {
        assert_eq!(s_bytes.len(), 8, "Nonce wire representation must be 8 bytes");
        let mut bytes = [0u8; 12];
        bytes[4..].copy_from_slice(s_bytes);
        Self(bytes)
    }

    /// Create a nonce from a full 12-byte value.
    pub fn from_raw(bytes: [u8; 12]) -> Self {
        Self(bytes)
    }

    /// The 8-byte wire representation (last 8 bytes of the 12-byte nonce).
    pub fn cc_str(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out.copy_from_slice(&self.0[4..]);
        out
    }

    /// Full 12-byte nonce data.
    pub fn data(&self) -> &[u8; 12] {
        &self.0
    }

    /// Extract the 64-bit value.
    pub fn val(&self) -> u64 {
        u64::from_be_bytes(self.0[4..].try_into().unwrap())
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({:016x})", self.val())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_roundtrip() {
        let val: u64 = (1u64 << 63) | 42; // direction=1, seq=42
        let n = Nonce::from_val(val);
        assert_eq!(n.val(), val);
        assert_eq!(n.cc_str(), val.to_be_bytes());
    }

    #[test]
    fn nonce_from_wire() {
        let wire = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2a];
        let n = Nonce::from_bytes(&wire);
        assert_eq!(n.val(), 42);
        assert_eq!(&n.0[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn nonce_direction_bit() {
        let server_val = 0u64; // TO_SERVER, seq=0
        let client_val = 1u64 << 63; // TO_CLIENT, seq=0
        let n_server = Nonce::from_val(server_val);
        let n_client = Nonce::from_val(client_val);
        assert_ne!(n_server, n_client);
        assert_eq!(n_server.val(), 0);
        assert_eq!(n_client.val(), client_val);
    }
}
