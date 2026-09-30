//! Peer address advertisement and merge rules.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Stored `host:port` strings kept for one peer.
pub const MAX_PEER_ADDRESSES: usize = 8;

/// Tailscale CGNAT (`100.64.0.0/10`).
const TS_V4_PREFIX: Ipv4Addr = Ipv4Addr::new(100, 64, 0, 0);
const TS_V4_MASK: u32 = 0xffc00000;

/// Tailscale IPv6 (`fd7a:115c:a1e0::/48`).
const TS_V6: [u16; 3] = [0xfd7a, 0x115c, 0xa1e0];

pub fn is_tailscale_v4(ip: Ipv4Addr) -> bool {
    (u32::from(ip) & TS_V4_MASK) == u32::from(TS_V4_PREFIX)
}

pub fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    let segs = ip.segments();
    segs[0] == TS_V6[0] && segs[1] == TS_V6[1] && segs[2] == TS_V6[2]
}

/// IPs we advertise: not loopback or link-local. Includes Tailscale ranges.
pub fn is_advertisable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !v.is_unspecified()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_multicast()
                && !v.is_broadcast()
        }
        IpAddr::V6(v) => {
            !v.is_unspecified()
                && !v.is_loopback()
                && !v.is_multicast()
                && !is_ipv6_link_local(v)
                && (is_tailscale_v6(v) || !is_ipv6_unique_local(v))
        }
    }
}

fn is_ipv6_link_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn is_ipv6_unique_local(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

pub fn format_ip_port(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v) => format!("{v}:{port}"),
        IpAddr::V6(v) => format!("[{v}]:{port}"),
    }
}

pub fn format_socket_addr(addr: SocketAddr) -> String {
    format_ip_port(addr.ip(), addr.port())
}

/// Put newly discovered (typically LAN) addresses first, keep existing ones
/// (Tailscale, VPN), drop duplicates, cap at [`MAX_PEER_ADDRESSES`].
pub fn merge_peer_addresses(existing: &[String], discovered: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for addr in discovered.iter().chain(existing) {
        if addr.is_empty() {
            continue;
        }
        if !out.iter().any(|have| have == addr) {
            out.push(addr.clone());
        }
        if out.len() == MAX_PEER_ADDRESSES {
            break;
        }
    }
    out
}

/// Prefer the observed remote (the path that just worked), then advertised
/// addresses, deduped and capped at [`MAX_PEER_ADDRESSES`].
pub fn collect_peer_addresses(advertised: &[String], observed: Option<SocketAddr>) -> Vec<String> {
    let extra = observed
        .map(format_socket_addr)
        .into_iter()
        .collect::<Vec<_>>();
    merge_peer_addresses(advertised, &extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn filters_loopback_and_link_local() {
        assert!(!is_advertisable_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_advertisable_ip("::1".parse().unwrap()));
        assert!(!is_advertisable_ip("169.254.10.1".parse().unwrap()));
        assert!(!is_advertisable_ip("fe80::1".parse().unwrap()));
        assert!(!is_advertisable_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_advertisable_ip("224.0.0.1".parse().unwrap()));
    }

    #[test]
    fn keeps_lan_global_and_tailscale() {
        assert!(is_advertisable_ip("192.168.1.10".parse().unwrap()));
        assert!(is_advertisable_ip("10.0.0.5".parse().unwrap()));
        assert!(is_advertisable_ip("100.64.1.2".parse().unwrap()));
        assert!(is_advertisable_ip("100.127.255.255".parse().unwrap()));
        assert!(is_advertisable_ip("8.8.8.8".parse().unwrap()));
        assert!(is_advertisable_ip("2001:4860:4860::8888".parse().unwrap()));
        let ts: Ipv6Addr = "fd7a:115c:a1e0::1".parse().unwrap();
        assert!(is_advertisable_ip(IpAddr::V6(ts)));
        let other_ula: Ipv6Addr = "fd00::1".parse().unwrap();
        assert!(!is_advertisable_ip(IpAddr::V6(other_ula)));
    }

    #[test]
    fn tailscale_helpers() {
        assert!(is_tailscale_v4("100.64.0.1".parse().unwrap()));
        assert!(!is_tailscale_v4("100.63.0.1".parse().unwrap()));
        assert!(is_tailscale_v6("fd7a:115c:a1e0:1::1".parse().unwrap()));
        assert!(!is_tailscale_v6("fd7a:115c:a1e1::1".parse().unwrap()));
    }

    #[test]
    fn merge_puts_discovered_first_and_caps() {
        let existing = vec!["100.64.1.2:47321".into(), "192.168.1.10:47321".into()];
        let discovered = vec!["192.168.1.10:47321".into(), "192.168.1.11:47321".into()];
        let merged = merge_peer_addresses(&existing, &discovered);
        assert_eq!(
            merged,
            vec![
                "192.168.1.10:47321",
                "192.168.1.11:47321",
                "100.64.1.2:47321",
            ]
        );
    }

    #[test]
    fn merge_caps_at_eight() {
        let existing: Vec<String> = (0..6).map(|i| format!("10.0.0.{i}:1")).collect();
        let discovered: Vec<String> = (10..16).map(|i| format!("10.0.0.{i}:1")).collect();
        let merged = merge_peer_addresses(&existing, &discovered);
        assert_eq!(merged.len(), 8);
        assert_eq!(merged[0], "10.0.0.10:1");
        assert!(merged.contains(&"10.0.0.0:1".into()));
        assert!(!merged.contains(&"10.0.0.5:1".into()));
    }

    #[test]
    fn collect_adds_observed_and_dedupes() {
        let observed = "192.168.1.20:47321".parse().unwrap();
        let addrs = collect_peer_addresses(
            &["192.168.1.20:47321".into(), "100.64.1.2:47321".into()],
            Some(observed),
        );
        assert_eq!(addrs, vec!["192.168.1.20:47321", "100.64.1.2:47321"]);
    }

    #[test]
    fn collect_puts_observed_first() {
        let observed = "127.0.0.1:47321".parse().unwrap();
        let addrs = collect_peer_addresses(
            &["10.0.0.5:47321".into(), "10.3.0.2:47321".into()],
            Some(observed),
        );
        assert_eq!(
            addrs,
            vec!["127.0.0.1:47321", "10.0.0.5:47321", "10.3.0.2:47321"]
        );
    }

    #[test]
    fn formats_v6_with_brackets() {
        let ip: IpAddr = "fd7a:115c:a1e0::1".parse().unwrap();
        assert_eq!(format_ip_port(ip, 47321), "[fd7a:115c:a1e0::1]:47321");
    }
}
