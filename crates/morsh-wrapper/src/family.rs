//! Address filtering and ordering from mosh.pl's resolvename.

use std::net::{SocketAddr, ToSocketAddrs};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Family {
    Inet,
    Inet6,
    Auto,
    All,
    #[default]
    PreferInet,
    PreferInet6,
}

impl std::str::FromStr for Family {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "inet" => Ok(Self::Inet),
            "inet6" => Ok(Self::Inet6),
            "auto" => Ok(Self::Auto),
            "all" => Ok(Self::All),
            "prefer-inet" => Ok(Self::PreferInet),
            "prefer-inet6" => Ok(Self::PreferInet6),
            _ => Err(format!("invalid address family '{value}' (expected inet, inet6, auto, all, prefer-inet, or prefer-inet6)")),
        }
    }
}

impl Family {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inet => "inet",
            Self::Inet6 => "inet6",
            Self::Auto => "auto",
            Self::All => "all",
            Self::PreferInet => "prefer-inet",
            Self::PreferInet6 => "prefer-inet6",
        }
    }

    pub(crate) fn resolve(self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("could not resolve '{host}': {e}"))?
            .collect();
        self.order(addresses).map_err(|e| format!("{host}: {e}"))
    }

    fn order(self, mut addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
        match self {
            Self::Inet => addresses.retain(SocketAddr::is_ipv4),
            Self::Inet6 => addresses.retain(SocketAddr::is_ipv6),
            Self::PreferInet => addresses.sort_by_key(|a| !a.is_ipv4()),
            Self::PreferInet6 => addresses.sort_by_key(|a| !a.is_ipv6()),
            Self::Auto => {
                if addresses.iter().any(SocketAddr::is_ipv4)
                    && addresses.iter().any(SocketAddr::is_ipv6)
                {
                    return Err(
                        "host has both IPv4 and IPv6 addresses, use --family to specify behavior"
                            .into(),
                    );
                }
            }
            Self::All => {}
        }
        if addresses.is_empty() {
            return Err("no addresses found for requested address family".into());
        }
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses() -> Vec<SocketAddr> {
        ["[::1]:22", "127.0.0.1:22", "[::2]:22", "127.0.0.2:22"]
            .map(|value| value.parse().unwrap())
            .to_vec()
    }

    #[test]
    fn fixed_families_filter_and_preferences_stably_reorder() {
        let input = addresses();
        for (family, indices) in [
            (Family::Inet, vec![1, 3]),
            (Family::Inet6, vec![0, 2]),
            (Family::All, vec![0, 1, 2, 3]),
            (Family::PreferInet, vec![1, 3, 0, 2]),
            (Family::PreferInet6, vec![0, 2, 1, 3]),
        ] {
            let expected: Vec<_> = indices.into_iter().map(|i| input[i]).collect();
            assert_eq!(family.order(input.clone()).unwrap(), expected);
        }
    }

    #[test]
    fn auto_requires_a_single_family_and_filtering_may_fail() {
        assert!(Family::Auto.order(addresses()).is_err());
        let ipv6 = Family::Inet6.order(addresses()).unwrap();
        assert_eq!(Family::Auto.order(ipv6.clone()).unwrap(), ipv6);
        assert!(Family::Inet.order(ipv6).is_err());
        assert!(Family::All.order(Vec::new()).is_err());
    }
}
