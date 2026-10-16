//! Pairing by QR code.
//!
//! The device showing a pairing code also shows it as a QR code holding a
//! `relay://pair` link with the code and this device's addresses. A phone's
//! camera opens the link in Relay, which joins straight to those addresses,
//! so pairing does not depend on LAN discovery (which Android restricts).

use std::path::Path;
use std::time::{Duration, Instant};

use relay_ipc::{Client, PairJoinParams};

use crate::error::error_chain;

const PREFIX: &str = "relay://pair?";
/// Addresses a link carries; more make the QR code denser.
const MAX_ADDRS: usize = 4;

/// `relay://pair?code=…&addr=…&addr=…`.
pub fn link(code: &str, addrs: &[String]) -> String {
    let mut out = format!("{PREFIX}code={}", encode(code));
    for addr in addrs {
        out.push_str("&addr=");
        out.push_str(&encode(addr));
    }
    out
}

/// The code and addresses in a pairing link, or `None` if it is not one.
pub fn parse(link: &str) -> Option<(String, Vec<String>)> {
    let query = link.trim().strip_prefix(PREFIX)?;
    let mut code = None;
    let mut addrs = Vec::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        let value = decode(value)?;
        match key {
            "code" => code = Some(value),
            "addr" if !value.is_empty() => addrs.push(value),
            _ => {}
        }
    }
    let code = code.filter(|c| !c.is_empty())?;
    Some((code, addrs))
}

/// The link as an SVG QR code.
pub fn qr_svg(link: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(link, qrcode::EcLevel::M).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .quiet_zone(true)
            .build(),
    )
}

/// This device's addresses on `port`, best first: home and office LANs,
/// then other private ranges (often VMs), then Tailscale, then IPv6.
pub fn addresses(port: u16) -> Vec<String> {
    let mut addrs = relay_net::advertised_addresses(port);
    addrs.sort_by_key(|a| rank(a));
    addrs.truncate(MAX_ADDRS);
    addrs
}

fn rank(addr: &str) -> u8 {
    if addr.starts_with("192.168.") {
        0
    } else if addr.starts_with("10.") {
        1
    } else if addr.starts_with("172.") {
        2
    } else if addr.starts_with("100.") {
        3
    } else if addr.starts_with('[') {
        5
    } else {
        4
    }
}

/// Pair with the device that showed `link`: wait up to `wait` for this
/// device's host to come up, then try each address in turn, then LAN
/// discovery. Returns the other device's name.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn join(home: &Path, link: &str, wait: Duration) -> Result<String, String> {
    let (code, addrs) = parse(link).ok_or("that is not a Relay pairing code")?;
    let deadline = Instant::now() + wait;
    let mut client = loop {
        match Client::connect(home) {
            Ok(Some(client)) => break client,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            Ok(None) => return Err("Relay is not running".into()),
            Err(err) => return Err(error_chain(&err)),
        }
    };
    let tries = addrs.into_iter().map(Some).chain(std::iter::once(None));
    let mut last = String::new();
    for addr in tries {
        let params = PairJoinParams {
            code: code.clone(),
            addr,
            allow_manage: false,
        };
        match client.pair_join(&params) {
            Ok(joined) => return Ok(joined.peer_name),
            Err(err) => last = error_chain(&err),
        }
    }
    Err(last)
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip_with_ipv6_and_several_addresses() {
        let addrs = vec!["10.0.0.9:47321".to_owned(), "[fd00::1]:47321".to_owned()];
        let made = link("3281975355", &addrs);
        assert_eq!(
            made,
            "relay://pair?code=3281975355&addr=10.0.0.9:47321&addr=%5Bfd00:1%5D:47321"
                .replace("fd00:1", "fd00::1")
        );
        assert_eq!(parse(&made), Some(("3281975355".to_owned(), addrs)));
    }

    #[test]
    fn other_links_are_not_pairing_links() {
        assert_eq!(parse("https://example.com/?code=1"), None);
        assert_eq!(parse("relay://pair?addr=10.0.0.9:1"), None);
        assert_eq!(parse("relay://pair?code=12&addr=%ZZ"), None);
    }

    #[test]
    fn lan_addresses_come_before_vm_and_vpn_ones() {
        let mut addrs = [
            "[fd00::1]:1",
            "100.64.0.2:1",
            "172.19.128.1:1",
            "10.0.0.9:1",
        ];
        addrs.sort_by_key(|a| rank(a));
        assert_eq!(addrs[0], "10.0.0.9:1");
        assert_eq!(addrs[3], "[fd00::1]:1");
    }

    #[test]
    fn qr_codes_render_as_svg() {
        let svg = qr_svg(&link("3281975355", &["10.0.0.9:47321".to_owned()])).unwrap();
        assert!(
            svg.starts_with("<?xml") || svg.starts_with("<svg"),
            "{}",
            &svg[..40]
        );
    }
}
