use crate::constants::{MAX_RTO_MS, MIN_RTO_MS};

/// Round-trip time estimator following RFC 6298.
#[derive(Debug, Clone)]
pub struct RttEstimator {
    srtt_ms: u64,
    rttvar_ms: u64,
    have_sample: bool,
}

impl RttEstimator {
    pub fn new() -> Self {
        Self {
            srtt_ms: 50,
            rttvar_ms: 25,
            have_sample: false,
        }
    }

    /// Record a new RTT sample (in milliseconds).
    pub fn update(&mut self, rtt_ms: u64) {
        // Ignore unreasonable samples (e.g., server suspended)
        if rtt_ms == 0 || rtt_ms > 5000 {
            return;
        }

        if !self.have_sample {
            self.srtt_ms = rtt_ms;
            self.rttvar_ms = rtt_ms / 2;
            self.have_sample = true;
        } else {
            // alpha = 1/8, beta = 1/4
            let diff = if self.srtt_ms > rtt_ms {
                self.srtt_ms - rtt_ms
            } else {
                rtt_ms - self.srtt_ms
            };
            self.rttvar_ms = (3 * self.rttvar_ms + diff) / 4;
            self.srtt_ms = (7 * self.srtt_ms + rtt_ms) / 8;
        }
    }

    /// Compute the retransmission timeout.
    pub fn rto_ms(&self) -> u64 {
        let rto = self.srtt_ms + 4 * self.rttvar_ms;
        rto.clamp(MIN_RTO_MS, MAX_RTO_MS)
    }

    /// Get the current smoothed RTT.
    pub fn srtt_ms(&self) -> u64 {
        self.srtt_ms
    }

    /// Get the current RTT variance.
    pub fn rttvar_ms(&self) -> u64 {
        self.rttvar_ms
    }

    /// Compute adaptive send interval: ceil(SRTT / 2), clamped.
    pub fn send_interval_ms(&self) -> u64 {
        let interval = (self.srtt_ms + 1) / 2;
        interval.clamp(crate::constants::SEND_INTERVAL_MIN_MS, crate::constants::SEND_INTERVAL_MAX_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let rtt = RttEstimator::new();
        assert_eq!(rtt.srtt_ms(), 50);
        assert_eq!(rtt.rttvar_ms(), 25);
        assert!(rtt.rto_ms() >= MIN_RTO_MS);
    }

    #[test]
    fn first_sample() {
        let mut rtt = RttEstimator::new();
        rtt.update(200);
        assert_eq!(rtt.srtt_ms(), 200);
        assert_eq!(rtt.rttvar_ms(), 100);
    }

    #[test]
    fn converging_samples() {
        let mut rtt = RttEstimator::new();
        rtt.update(100);
        rtt.update(120);
        rtt.update(110);
        // SRTT converges toward the mean (~103-104)
        assert!(rtt.srtt_ms() >= 100 && rtt.srtt_ms() <= 110,
            "SRTT {} out of expected range", rtt.srtt_ms());
    }

    #[test]
    fn rto_clamped() {
        let mut rtt = RttEstimator::new();
        rtt.update(10000); // way too high, ignored
        assert_eq!(rtt.srtt_ms(), 50); // unchanged

        let mut rtt2 = RttEstimator::new();
        rtt2.update(1);
        assert!(rtt2.rto_ms() >= MIN_RTO_MS);
    }

    #[test]
    fn send_interval_bounds() {
        use crate::constants::{SEND_INTERVAL_MIN_MS, SEND_INTERVAL_MAX_MS};
        let mut rtt = RttEstimator::new();

        // Low RTT → minimum send interval
        rtt.update(10);
        assert_eq!(rtt.send_interval_ms(), SEND_INTERVAL_MIN_MS);

        // Fresh estimator with high RTT → maximum send interval
        let mut rtt2 = RttEstimator::new();
        rtt2.update(1000);
        assert_eq!(rtt2.send_interval_ms(), SEND_INTERVAL_MAX_MS);
    }
}
