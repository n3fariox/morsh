//! Stock mosh idle timeout policy and process shutdown notifications.

use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Timeouts {
    network: Option<Duration>,
    signaled: Option<Duration>,
}

impl Timeouts {
    pub(crate) fn from_env() -> Self {
        let read = |name| {
            std::env::var(name)
                .ok()
                .and_then(|value| parse_timeout(name, &value))
        };
        Self {
            network: read("MOSH_SERVER_NETWORK_TMOUT"),
            signaled: read("MOSH_SERVER_SIGNAL_TMOUT"),
        }
    }

    pub(crate) fn remaining(&self, last_state: Instant) -> Option<Duration> {
        self.network
            .map(|limit| limit.saturating_sub(last_state.elapsed()))
    }

    pub(crate) fn expired(&self, last_state: Instant) -> bool {
        self.remaining(last_state)
            .is_some_and(|left| left.is_zero())
    }

    pub(crate) fn signal_expired(&self, last_state: Instant) -> bool {
        self.signaled
            .is_none_or(|limit| last_state.elapsed() >= limit)
    }
}

fn parse_timeout(name: &str, value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    match value.trim_start().parse::<i64>() {
        Ok(seconds) if seconds > 0 => Some(Duration::from_secs(seconds as u64)),
        Ok(0) => None,
        _ => {
            eprintln!("{name} is not a nonnegative integer, ignoring");
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ShutdownSignal {
    Terminate,
    Idle,
}

#[cfg(unix)]
pub(crate) struct Signals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    user: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    pub(crate) fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            user: signal(SignalKind::user_defined1())?,
        })
    }

    pub(crate) async fn recv(&mut self) -> ShutdownSignal {
        tokio::select! {
            _ = self.terminate.recv() => ShutdownSignal::Terminate,
            _ = self.interrupt.recv() => ShutdownSignal::Terminate,
            _ = self.user.recv() => ShutdownSignal::Idle,
        }
    }
}

#[cfg(not(unix))]
pub(crate) struct Signals;

#[cfg(not(unix))]
impl Signals {
    pub(crate) fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    pub(crate) async fn recv(&mut self) -> ShutdownSignal {
        let _ = tokio::signal::ctrl_c().await;
        ShutdownSignal::Terminate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_values_follow_stock_mosh_policy() {
        for value in ["", "0", "-1", "junk", "1x", "1 ", "9223372036854775808"] {
            assert_eq!(parse_timeout("test", value), None, "{value}");
        }
        for value in ["1", "+1", " 1"] {
            assert_eq!(parse_timeout("test", value), Some(Duration::from_secs(1)));
        }
    }

    #[test]
    fn recent_client_states_delay_both_idle_timeouts() {
        let now = Instant::now();
        let timeouts = Timeouts {
            network: Some(Duration::from_secs(10)),
            signaled: Some(Duration::from_secs(5)),
        };
        assert!(!timeouts.expired(now));
        assert!(!timeouts.signal_expired(now));
        assert!(timeouts.signal_expired(now - Duration::from_secs(6)));
        assert!(!timeouts.expired(now - Duration::from_secs(6)));
        assert!(timeouts.expired(now - Duration::from_secs(11)));
        assert!(Timeouts::default().signal_expired(now));
        assert!(!Timeouts::default().expired(now - Duration::from_secs(100)));
    }
}
