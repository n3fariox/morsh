use std::fmt;

/// A 128-bit (16-byte) key encoded as a 22-character base64 string.
///
/// Mosh uses URL-safe base64 without padding:
/// - 16 bytes → 24 base64 chars → strip trailing `==` → 22 chars
#[derive(Clone)]
pub struct Base64Key([u8; 16]);

impl Base64Key {
    /// Create a random key using the system CSPRNG.
    pub fn random() -> Self {
        let mut key = [0u8; 16];
        getrandom::getrandom(&mut key).expect("Failed to generate random key");
        Self(key)
    }

    /// Parse a 22-character base64 key string.
    pub fn from_printable(printable: &str) -> Result<Self, String> {
        if printable.len() != 22 {
            return Err(format!("Key must be 22 characters, got {}", printable.len()));
        }

        // Add back the padding
        let mut padded = String::with_capacity(24);
        padded.push_str(printable);
        padded.push_str("==");

        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&padded)
            .map_err(|e| format!("Invalid base64: {e}"))?;

        if decoded.len() != 16 {
            return Err(format!("Key must represent 16 octets, got {}", decoded.len()));
        }

        let mut key = [0u8; 16];
        key.copy_from_slice(&decoded);

        // Verify roundtrip
        let reencoded = Self(key).printable_key();
        if reencoded != printable {
            return Err("Base64 key was not encoded 128-bit key".to_string());
        }

        Ok(Self(key))
    }

    /// Encode the key as a 22-character base64 string (no padding).
    pub fn printable_key(&self) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&self.0);
        // encoded is 24 chars with "==" padding — strip the last 2
        encoded[..22].to_string()
    }

    /// Raw 16-byte key data.
    pub fn data(&self) -> &[u8; 16] {
        &self.0
    }

    /// Consume and return the raw key bytes.
    pub fn into_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for Base64Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Base64Key({})", self.printable_key())
    }
}

impl PartialEq for Base64Key {
    fn eq(&self, other: &Self) -> bool {
        subtle::ConstantTimeEq::ct_eq(&self.0[..], &other.0[..]).into()
    }
}

impl Eq for Base64Key {}

impl From<[u8; 16]> for Base64Key {
    fn from(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let key = Base64Key::random();
        let printable = key.printable_key();
        assert_eq!(printable.len(), 22);
        let parsed = Base64Key::from_printable(&printable).unwrap();
        assert_eq!(key, parsed);
    }

    #[test]
    fn key_from_raw_bytes() {
        let bytes = [0x41u8; 16];
        let key = Base64Key::from(bytes);
        let printable = key.printable_key();
        assert_eq!(printable.len(), 22);
        let parsed = Base64Key::from_printable(&printable).unwrap();
        assert_eq!(key, parsed);
    }

    #[test]
    fn key_bad_length() {
        assert!(Base64Key::from_printable("short").is_err());
        assert!(Base64Key::from_printable("this-is-way-too-long-key!!").is_err());
    }

    #[test]
    fn key_constant_time_eq() {
        let a = Base64Key::from([0x41u8; 16]);
        let b = Base64Key::from([0x42u8; 16]);
        assert_eq!(a, a);
        assert_ne!(a, b);
    }
}
