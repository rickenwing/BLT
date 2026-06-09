//! Per-service NIC binding support (F1, TDD §3.1): interface enumeration for
//! the admin dropdowns and helpers to pick the advertised IP per service.

use serde::Serialize;
use std::net::{IpAddr, SocketAddr};

/// One bindable interface option shown in the admin panel (F1.1).
#[derive(Debug, Clone, Serialize)]
pub struct InterfaceInfo {
    pub name: String,
    pub ip: String,
    pub is_loopback: bool,
}

/// Enumerate local IPv4 interfaces (plus the "all interfaces" pseudo-entry the
/// admin UI offers separately).
pub fn list_interfaces() -> Vec<InterfaceInfo> {
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for i in ifaces {
            let ip = i.ip();
            if ip.is_ipv4() {
                out.push(InterfaceInfo {
                    name: i.name.clone(),
                    ip: ip.to_string(),
                    is_loopback: ip.is_loopback(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The IP a service should advertise over mDNS for its bind address: a concrete
/// bind IP is advertised as-is; `0.0.0.0` falls back to the machine's primary
/// local IP (TDD §9 — TXT records carry the IP of the relevant bound NIC).
pub fn advertise_ip(bind: &SocketAddr) -> IpAddr {
    let ip = bind.ip();
    if !ip.is_unspecified() {
        return ip;
    }
    local_ip_address::local_ip().unwrap_or(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_at_least_loopback() {
        let ifaces = list_interfaces();
        // every machine has at least loopback; don't assume more in CI
        assert!(ifaces.iter().any(|i| i.is_loopback));
    }

    #[test]
    fn advertise_ip_passes_through_concrete_bind() {
        let bind: SocketAddr = "192.168.1.10:7400".parse().unwrap();
        assert_eq!(advertise_ip(&bind).to_string(), "192.168.1.10");
    }
}
