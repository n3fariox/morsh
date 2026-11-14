//! Numeric bind addresses, including IPv6 zones, with stock mosh's fallback.

use std::ffi::CString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

fn interface_index(name: &str) -> Result<u32, String> {
    let name = CString::new(name).map_err(|_| "invalid IPv6 interface name".to_string())?;
    #[cfg(unix)]
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    #[cfg(windows)]
    let index = unsafe {
        windows_sys::Win32::NetworkManagement::IpHelper::if_nametoindex(name.as_ptr().cast())
    };
    if index == 0 {
        return Err(format!("unknown IPv6 scope interface {name:?}"));
    }
    Ok(index)
}

fn numeric_address(address: &str) -> Result<SocketAddr, String> {
    if let Some((ip, zone)) = address.split_once('%') {
        let ip = ip.parse::<Ipv6Addr>().map_err(|e| e.to_string())?;
        if zone.is_empty() {
            return Err("missing IPv6 scope identifier".into());
        }
        let scope = if zone.bytes().all(|b| b.is_ascii_digit()) {
            zone.parse::<u32>().map_err(|e| e.to_string())?
        } else {
            interface_index(zone)?
        };
        return Ok(SocketAddrV6::new(ip, 0, 0, scope).into());
    }
    address
        .parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, 0))
        .map_err(|e| e.to_string())
}

/// Try the desired address first, then any interface. Port zero is a template
/// so the port probe and real UDP transport use identical address selection.
pub(crate) fn addresses(desired: Option<&str>) -> Vec<SocketAddr> {
    let any = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0);
    let mut addresses = Vec::with_capacity(2);
    if let Some(desired) = desired {
        match numeric_address(desired) {
            Ok(address) if address != any => addresses.push(address),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                "Cannot resolve bind address {desired}: {error}; trying any interface"
            ),
        }
    }
    addresses.push(any);
    addresses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ipv6_scope_is_preserved() {
        let SocketAddr::V6(address) = numeric_address("fe80::1%42").unwrap() else {
            panic!("expected IPv6 address");
        };
        assert_eq!(*address.ip(), "fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(address.scope_id(), 42);
        assert_eq!(address.port(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn named_ipv6_scope_resolves_to_an_interface_index() {
        let (name, index) = ["lo", "lo0"]
            .into_iter()
            .find_map(|name| interface_index(name).ok().map(|index| (name, index)))
            .expect("a loopback interface must exist");
        let SocketAddr::V6(address) = numeric_address(&format!("fe80::1%{name}")).unwrap() else {
            panic!("expected IPv6 address");
        };
        assert_eq!(address.scope_id(), index);
    }

    #[test]
    fn invalid_numeric_addresses_fall_back_without_dns() {
        for address in [
            "hostname",
            "fe80::1%",
            "fe80::1%4294967296",
            "fe80::1%no-such-morsh-interface",
            "127.0.0.1%2",
        ] {
            let candidates = addresses(Some(address));
            assert_eq!(candidates.len(), 1, "{address}");
            assert!(candidates[0].ip().is_unspecified());
        }
    }

    #[test]
    fn desired_address_precedes_any_interface_without_duplicates() {
        let candidates = addresses(Some("127.0.0.1"));
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].ip().is_loopback());
        assert!(candidates[1].ip().is_unspecified());
        assert_eq!(addresses(Some("0.0.0.0")).len(), 1);
        assert_eq!(addresses(None).len(), 1);
    }
}
