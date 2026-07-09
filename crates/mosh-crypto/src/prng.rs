/// Cryptographically secure PRNG using the system random source.
///
/// Uses `getrandom` which works on:
/// - Linux: `getrandom()` syscall or `/dev/urandom`
/// - Windows: BCryptGenRandom
/// - macOS: `getentropy()`
/// - Most other Unix: `getentropy()` or `/dev/urandom`
pub struct Prng;

impl Prng {
    /// Fill a buffer with random bytes.
    pub fn fill(dest: &mut [u8]) {
        getrandom::getrandom(dest).expect("Failed to read random bytes");
    }

    /// Generate a random u8.
    pub fn u8(&mut self) -> u8 {
        let mut buf = [0u8; 1];
        Self::fill(&mut buf);
        buf[0]
    }

    /// Generate a random u32.
    pub fn u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        Self::fill(&mut buf);
        u32::from_ne_bytes(buf)
    }

    /// Generate a random u64.
    pub fn u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        Self::fill(&mut buf);
        u64::from_ne_bytes(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_works() {
        let mut buf = [0u8; 32];
        Prng::fill(&mut buf);
        // Extremely unlikely to be all zeros
        assert_ne!(buf, [0u8; 32]);
    }

    #[test]
    fn randomness() {
        let mut prng = Prng;
        let a = prng.u64();
        let b = prng.u64();
        assert_ne!(a, b);
    }
}
