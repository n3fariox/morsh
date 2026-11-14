use crate::constants::{MAX_RTO_MS, MIN_RTO_MS};

/// Samples at or above this are discarded, e.g. when the peer was suspended
/// with Ctrl-Z (mosh: `R < 5000`).
const MAX_RTT_SAMPLE_MS: f64 = 5000.0;

/// Round-trip time estimator (RFC 6298), a port of the `SRTT`/`RTTVAR`
/// bookkeeping in mosh's `Connection::recv_one` and `Connection::timeout`.
///
/// Mosh keeps both values as `double`s and starts from a deliberately
/// pessimistic 1000/500 ms, so the first frames go out at the slowest rate
/// and the first retransmission timeout is the maximum one. Integer
/// arithmetic would drift: `(3 * 1 + 0) / 4` truncates a small variance to
/// zero, which on a fast link pins the RTO at its floor.
#[derive(Debug, Clone)]
pub struct RttEstimator {
    srtt: f64,
    rttvar: f64,
    have_sample: bool,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RttEstimator {
    pub fn new() -> Self {
        Self {
            srtt: 1000.0,
            rttvar: 500.0,
            have_sample: false,
        }
    }

    /// Record a new RTT sample in milliseconds. A 0 ms sample is valid: on a
    /// LAN the 16-bit millisecond echo routinely comes back unchanged.
    pub fn update(&mut self, rtt_ms: f64) {
        if !(0.0..MAX_RTT_SAMPLE_MS).contains(&rtt_ms) {
            return;
        }

        if !self.have_sample {
            self.srtt = rtt_ms;
            self.rttvar = rtt_ms / 2.0;
            self.have_sample = true;
        } else {
            const ALPHA: f64 = 1.0 / 8.0;
            const BETA: f64 = 1.0 / 4.0;
            self.rttvar = (1.0 - BETA) * self.rttvar + BETA * (self.srtt - rtt_ms).abs();
            self.srtt = (1.0 - ALPHA) * self.srtt + ALPHA * rtt_ms;
        }
    }

    /// The retransmission timeout, mosh's `Connection::timeout()`.
    pub fn rto_ms(&self) -> u64 {
        ((self.srtt + 4.0 * self.rttvar).ceil() as u64).clamp(MIN_RTO_MS, MAX_RTO_MS)
    }

    /// The smoothed RTT in milliseconds, mosh's `get_SRTT()`.
    pub fn srtt(&self) -> f64 {
        self.srtt
    }

    pub fn rttvar(&self) -> f64 {
        self.rttvar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_pessimistic_like_mosh() {
        let rtt = RttEstimator::new();
        assert_eq!(rtt.srtt(), 1000.0);
        assert_eq!(rtt.rttvar(), 500.0);
        assert_eq!(rtt.rto_ms(), MAX_RTO_MS);
    }

    #[test]
    fn first_sample() {
        let mut rtt = RttEstimator::new();
        rtt.update(200.0);
        assert_eq!(rtt.srtt(), 200.0);
        assert_eq!(rtt.rttvar(), 100.0);
    }

    #[test]
    fn smoothing_matches_mosh_floating_point() {
        let mut rtt = RttEstimator::new();
        rtt.update(100.0);
        rtt.update(120.0);
        // RTTVAR = 0.75 * 50 + 0.25 * 20; SRTT = 0.875 * 100 + 0.125 * 120
        assert_eq!(rtt.rttvar(), 42.5);
        assert_eq!(rtt.srtt(), 102.5);
        assert_eq!(rtt.rto_ms(), 273); // ceil(102.5 + 170)
    }

    /// Integer arithmetic used to truncate a small variance to zero.
    #[test]
    fn small_variance_is_not_truncated() {
        let mut rtt = RttEstimator::new();
        rtt.update(1.0);
        for _ in 0..3 {
            rtt.update(1.0);
        }
        assert!(rtt.rttvar() > 0.0);
    }

    /// Mosh accepts `R == 0`; a sub-millisecond LAN echo is a real sample.
    #[test]
    fn zero_ms_sample_is_used() {
        let mut rtt = RttEstimator::new();
        rtt.update(0.0);
        assert_eq!(rtt.srtt(), 0.0);
        assert_eq!(rtt.rto_ms(), MIN_RTO_MS);
    }

    #[test]
    fn oversized_samples_are_ignored() {
        let mut rtt = RttEstimator::new();
        rtt.update(5000.0);
        rtt.update(10000.0);
        rtt.update(-1.0);
        assert_eq!(rtt.srtt(), 1000.0);
        rtt.update(4999.0);
        assert_eq!(rtt.srtt(), 4999.0);
        assert_eq!(rtt.rto_ms(), MAX_RTO_MS);
    }
}
