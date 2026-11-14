use aes::Aes128;
use ocb3::{consts::U12, AeadInPlace, GenericArray, KeyInit, Ocb3};

use crate::nonce::Nonce;

/// A Mosh message: nonce + plaintext/ciphertext payload.
#[derive(Debug, Clone)]
pub struct Message {
    pub nonce: Nonce,
    pub text: Vec<u8>,
}

impl Message {
    pub fn new(nonce: Nonce, text: Vec<u8>) -> Self {
        Self { nonce, text }
    }
}

/// AES-128-OCB3 encryption session for the Mosh protocol.
///
/// Wire format of an encrypted packet:
/// ```text
/// [8-byte nonce repr] [ciphertext || 16-byte OCB tag]
/// ```
///
/// The 12-byte OCB nonce is constructed by prepending 4 zero bytes to the
/// 8-byte wire nonce.
pub struct Session {
    cipher: Ocb3<Aes128, U12, ocb3::consts::U16>,
    blocks_encrypted: u64,
}

/// Overhead added by encryption: 16-byte OCB tag.
pub const ADDED_BYTES: usize = 16;

/// Length of the nonce as carried on the wire, ahead of the ciphertext.
pub const NONCE_WIRE_LEN: usize = 8;

type Tag = GenericArray<u8, ocb3::consts::U16>;

impl Session {
    /// Create a new session with the given 16-byte key.
    pub fn new(key: [u8; 16]) -> Self {
        let ga_key = GenericArray::from(key);
        let cipher = Ocb3::<Aes128, U12, ocb3::consts::U16>::new(&ga_key);
        Self {
            cipher,
            blocks_encrypted: 0,
        }
    }

    /// Encrypt a datagram in place.
    ///
    /// On entry `packet` is [`NONCE_WIRE_LEN`] bytes of headroom (contents
    /// ignored) followed by the plaintext; on return it is the wire datagram,
    /// `[8-byte nonce repr][ciphertext][16-byte tag]`. Reusing one buffer
    /// across packets makes encryption allocation-free.
    pub fn seal(&mut self, nonce: &Nonce, packet: &mut Vec<u8>) {
        assert!(
            packet.len() >= NONCE_WIRE_LEN,
            "packet needs room for the nonce"
        );
        let pt_len = packet.len() - NONCE_WIRE_LEN;

        // Track blocks encrypted (for the 2^47 security limit). Checked
        // before encrypting, so nothing past the limit is ever produced.
        self.blocks_encrypted += (pt_len as u64).div_ceil(16);
        assert!(
            self.blocks_encrypted >> 47 == 0,
            "Encrypted 2^47 blocks — session key must be rotated"
        );

        packet[..NONCE_WIRE_LEN].copy_from_slice(&nonce.cc_str());
        let tag = self
            .cipher
            .encrypt_in_place_detached(
                &GenericArray::from(*nonce.data()),
                &[],
                &mut packet[NONCE_WIRE_LEN..],
            )
            .expect("OCB encryption failed");
        packet.extend_from_slice(&tag);
    }

    /// Decrypt a wire-format datagram in place, returning its nonce and the
    /// plaintext (a sub-slice of `packet`).
    pub fn open<'a>(&self, packet: &'a mut [u8]) -> Result<(Nonce, &'a mut [u8]), CryptoError> {
        if packet.len() < NONCE_WIRE_LEN + ADDED_BYTES {
            return Err(CryptoError::TooShort);
        }
        let (wire_nonce, body) = packet.split_at_mut(NONCE_WIRE_LEN);
        let (text, tag) = body.split_at_mut(body.len() - ADDED_BYTES);
        let nonce = Nonce::from_bytes(wire_nonce);
        self.cipher
            .decrypt_in_place_detached(
                &GenericArray::from(*nonce.data()),
                &[],
                text,
                Tag::from_slice(tag),
            )
            .map_err(|_| CryptoError::IntegrityCheckFailed)?;
        Ok((nonce, text))
    }

    /// Encrypt a plaintext message.
    ///
    /// Returns the wire-format bytes: [8-byte nonce repr][ciphertext + tag].
    pub fn encrypt(&mut self, msg: &Message) -> Vec<u8> {
        let mut packet = Vec::with_capacity(NONCE_WIRE_LEN + msg.text.len() + ADDED_BYTES);
        packet.resize(NONCE_WIRE_LEN, 0);
        packet.extend_from_slice(&msg.text);
        self.seal(&msg.nonce, &mut packet);
        packet
    }

    /// Decrypt a wire-format packet.
    ///
    /// Input: [8-byte nonce repr][ciphertext + tag]
    /// Returns the decrypted message.
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Message, CryptoError> {
        let mut packet = data.to_vec();
        let (nonce, text) = self.open(&mut packet)?;
        let len = text.len();
        packet.copy_within(NONCE_WIRE_LEN..NONCE_WIRE_LEN + len, 0);
        packet.truncate(len);
        Ok(Message::new(nonce, packet))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    TooShort,
    IntegrityCheckFailed,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::TooShort => write!(f, "Ciphertext must contain nonce and tag"),
            CryptoError::IntegrityCheckFailed => write!(f, "Packet failed integrity check"),
        }
    }
}

impl std::error::Error for CryptoError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base64_key::Base64Key;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());

        let plaintext = b"Hello, Mosh!";
        let msg = Message::new(Nonce::from_val(42), plaintext.to_vec());

        let encrypted = session.encrypt(&msg);
        let decrypted = session.decrypt(&encrypted).unwrap();

        assert_eq!(decrypted.nonce, msg.nonce);
        assert_eq!(decrypted.text, plaintext);
    }

    #[test]
    fn different_keys_fail() {
        let key1 = Base64Key::random();
        let key2 = Base64Key::random();
        let mut session1 = Session::new(*key1.data());
        let mut session2 = Session::new(*key2.data());

        let msg = Message::new(Nonce::from_val(1), b"secret".to_vec());
        let encrypted = session1.encrypt(&msg);

        assert!(session2.decrypt(&encrypted).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());

        let msg = Message::new(Nonce::from_val(7), b"test data".to_vec());
        let mut encrypted = session.encrypt(&msg);

        // Flip a bit in the ciphertext
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0x01;

        assert!(session.decrypt(&encrypted).is_err());
    }

    #[test]
    fn wrong_nonce_fails() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());

        let msg = Message::new(Nonce::from_val(100), b"test".to_vec());
        let encrypted = session.encrypt(&msg);

        // Construct a packet with a different nonce
        let mut bad = encrypted.clone();
        bad[0] ^= 0xFF;

        assert!(session.decrypt(&bad).is_err());
    }

    #[test]
    fn too_short_fails() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());
        assert!(session.decrypt(&[0u8; 10]).is_err());
    }

    #[test]
    fn empty_plaintext() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());

        let msg = Message::new(Nonce::from_val(0), vec![]);
        let encrypted = session.encrypt(&msg);
        // 8 nonce + 16 tag = 24 bytes minimum
        assert_eq!(encrypted.len(), 8 + 16);

        let decrypted = session.decrypt(&encrypted).unwrap();
        assert!(decrypted.text.is_empty());
    }

    #[test]
    fn large_payload() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());

        let payload = vec![0xABu8; 1500];
        let msg = Message::new(Nonce::from_val(999), payload.clone());
        let encrypted = session.encrypt(&msg);
        let decrypted = session.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted.text, payload);
    }

    #[test]
    fn seal_and_open_match_encrypt_and_decrypt() {
        let key = Base64Key::random();
        let mut a = Session::new(*key.data());
        let mut b = Session::new(*key.data());
        let nonce = Nonce::from_val((1 << 63) | 77);
        let text = b"in place, please".to_vec();

        let mut packet = vec![0xEE; NONCE_WIRE_LEN];
        packet.extend_from_slice(&text);
        a.seal(&nonce, &mut packet);
        assert_eq!(packet, b.encrypt(&Message::new(nonce, text.clone())));

        let (got_nonce, plain) = a.open(&mut packet).unwrap();
        assert_eq!(got_nonce, nonce);
        assert_eq!(plain, &text[..]);
    }

    #[test]
    fn open_rejects_short_and_forged_packets() {
        let s = Session::new([1; 16]);
        assert_eq!(s.open(&mut [0u8; 23]).unwrap_err(), CryptoError::TooShort);
        assert_eq!(
            s.open(&mut [0u8; 40]).unwrap_err(),
            CryptoError::IntegrityCheckFailed
        );
    }

    #[test]
    fn block_counter_limit() {
        let key = Base64Key::random();
        let mut session = Session::new(*key.data());
        session.blocks_encrypted = (1u64 << 47) - 1;

        let msg = Message::new(Nonce::from_val(0), vec![0u8; 16]);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.encrypt(&msg);
        }));
        assert!(result.is_err());
    }
}
