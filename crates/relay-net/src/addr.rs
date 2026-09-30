use std::net::IpAddr;

use relay_core::{format_ip_port, is_advertisable_ip};

pub(crate) fn advertised_addresses(port: u16) -> Vec<String> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            let mut out = Vec::new();
            for iface in ifaces {
                if iface.is_loopback() {
                    continue;
                }
                let ip = iface.ip();
                if !is_advertisable_ip(ip) {
                    continue;
                }
                let formatted = format_ip_port(ip, port);
                if !out.contains(&formatted) {
                    out.push(formatted);
                }
            }
            sort_advertised(&mut out);
            out
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not list interface addresses");
            Vec::new()
        }
    }
}

pub(crate) fn advertised_ips() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter(|iface| !iface.is_loopback() && is_advertisable_ip(iface.ip()))
            .map(|iface| iface.ip())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn sort_advertised(addrs: &mut [String]) {
    addrs.sort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn advertised_addresses_never_include_loopback() {
        let port = 47321;
        for addr in advertised_addresses(port) {
            assert!(
                !addr.starts_with("127.") && !addr.starts_with("[::1]"),
                "{addr}"
            );
        }
        for ip in advertised_ips() {
            assert!(!ip.is_loopback(), "{ip}");
            if let IpAddr::V4(v) = ip {
                assert_ne!(v, Ipv4Addr::UNSPECIFIED);
            }
        }
    }
}
