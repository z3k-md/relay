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

/// Dial order for one attempt. Does not change the stored list.
///
/// Best first: loopback, RFC1918 and non-Tailscale IPv6 unique-local,
/// Tailscale, link-local, DNS names, then every other address. Order within
/// a class stays as given. Empty strings are skipped.
pub fn rank_addresses(addrs: &[String]) -> Vec<String> {
    let mut indexed: Vec<(u8, usize, &String)> = addrs
        .iter()
        .enumerate()
        .filter(|(_, addr)| !addr.is_empty())
        .map(|(index, addr)| (address_class(addr), index, addr))
        .collect();
    indexed.sort_by_key(|(class, index, _)| (*class, *index));
    indexed
        .into_iter()
        .map(|(_, _, addr)| addr.clone())
        .collect()
}

fn address_class(addr: &str) -> u8 {
    let host = host_of(addr);
    match host.parse::<IpAddr>() {
        Ok(ip) => ip_class(ip),
        Err(_) => 4,
    }
}

fn host_of(addr: &str) -> &str {
    if let Some(rest) = addr.strip_prefix('[') {
        rest.split_once("]:").map(|(host, _)| host).unwrap_or(addr)
    } else if let Some((host, _)) = addr.rsplit_once(':') {
        host
    } else {
        addr
    }
}

fn ip_class(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(ip) => {
            if ip.is_loopback() {
                0
            } else if is_rfc1918(ip) {
                1
            } else if is_tailscale_v4(ip) {
                2
            } else if ip.is_link_local() {
                3
            } else {
                5
            }
        }
        IpAddr::V6(ip) => {
            if ip.is_loopback() {
                0
            } else if is_tailscale_v6(ip) {
                2
            } else if is_ipv6_unique_local(ip) {
                1
            } else if is_ipv6_link_local(ip) {
                3
            } else {
                5
            }
        }
    }
}

fn is_rfc1918(ip: Ipv4Addr) -> bool {
    match ip.octets() {
        [10, ..] => true,
        [172, second, ..] if (16..32).contains(&second) => true,
        [192, 168, ..] => true,
        _ => false,
    }
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

    #[test]
    fn rank_orders_classes_and_keeps_relative_order() {
        let addrs = vec![
            "8.8.8.8:1".into(),
            "192.168.1.10:2".into(),
            "100.64.1.2:3".into(),
            "example.com:4".into(),
            "".into(),
            "127.0.0.1:5".into(),
            "169.254.1.1:6".into(),
            "10.1.2.3:7".into(),
            "[::1]:8".into(),
            "[fd00::1]:9".into(),
            "[fd7a:115c:a1e0::1]:10".into(),
            "[fe80::1]:11".into(),
            "172.16.0.1:12".into(),
            "172.15.0.1:13".into(),
            "100.63.0.1:14".into(),
            "203.0.113.1:9".into(),
            "vpn.example:15".into(),
            "172.31.5.5:16".into(),
            "172.32.0.1:17".into(),
        ];
        assert_eq!(
            rank_addresses(&addrs),
            vec![
                "127.0.0.1:5",
                "[::1]:8",
                "192.168.1.10:2",
                "10.1.2.3:7",
                "[fd00::1]:9",
                "172.16.0.1:12",
                "172.31.5.5:16",
                "100.64.1.2:3",
                "[fd7a:115c:a1e0::1]:10",
                "169.254.1.1:6",
                "[fe80::1]:11",
                "example.com:4",
                "vpn.example:15",
                "8.8.8.8:1",
                "172.15.0.1:13",
                "100.63.0.1:14",
                "203.0.113.1:9",
                "172.32.0.1:17",
            ]
        );
    }

    #[test]
    fn rank_is_stable_for_public_addresses() {
        let addrs = vec![
            "8.8.8.8:1".into(),
            "1.1.1.1:1".into(),
            "203.0.113.5:9".into(),
        ];
        assert_eq!(
            rank_addresses(&addrs),
            vec!["8.8.8.8:1", "1.1.1.1:1", "203.0.113.5:9"]
        );
    }
}
