//! Minimal STUN Binding client (RFC 5389) for a reflexive address.
//!
//! The request is sent on the same UDP socket Quinn will use, before that
//! socket is handed over, so the mapped port is the one peers must dial.
//! A successful simultaneous dial of those mapped addresses is the hole punch.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use rand::RngCore;

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAGIC: u32 = 0x2112_A442;
const ATTR_XOR_MAPPED: u16 = 0x0020;
const SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

/// Query public STUN servers. `per_server` bounds each attempt. `None` if every
/// server fails; callers keep LAN and manual addresses.
pub fn discover_public(socket: &UdpSocket, per_server: Duration) -> Option<SocketAddr> {
    for server in SERVERS {
        if let Some(addr) = discover_one(socket, server, per_server) {
            return Some(addr);
        }
    }
    None
}

fn discover_one(socket: &UdpSocket, server: &str, timeout: Duration) -> Option<SocketAddr> {
    let dest = server.to_socket_addrs().ok()?.next()?;
    let request = binding_request();
    let txid: [u8; 12] = request[8..20].try_into().ok()?;
    let previous = socket.read_timeout().ok()?;
    socket.set_read_timeout(Some(timeout)).ok()?;
    let result = (|| {
        socket.send_to(&request, dest).ok()?;
        let mut buf = [0u8; 1500];
        let (n, _) = socket.recv_from(&mut buf).ok()?;
        parse_xor_mapped(&buf[..n], &txid)
    })();
    let _ = socket.set_read_timeout(previous);
    result
}

fn binding_request() -> [u8; 20] {
    let mut msg = [0u8; 20];
    msg[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    msg[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    rand::rngs::OsRng.fill_bytes(&mut msg[8..20]);
    msg
}

/// Build a Binding success for `mapped`, echoing the request's transaction id.
#[cfg(test)]
fn binding_success(request: &[u8], mapped: SocketAddr) -> Option<Vec<u8>> {
    if request.len() < 20 {
        return None;
    }
    let txid = &request[8..20];
    let attr = xor_mapped_attr(mapped, txid)?;
    let mut out = Vec::with_capacity(20 + attr.len());
    out.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    out.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(txid);
    out.extend_from_slice(&attr);
    Some(out)
}

#[cfg(test)]
fn xor_mapped_attr(mapped: SocketAddr, txid: &[u8]) -> Option<Vec<u8>> {
    let body = match mapped {
        SocketAddr::V4(v4) => {
            let port = v4.port() ^ ((MAGIC >> 16) as u16);
            let ip = u32::from(*v4.ip()) ^ MAGIC;
            let mut body = Vec::with_capacity(8);
            body.push(0);
            body.push(0x01);
            body.extend_from_slice(&port.to_be_bytes());
            body.extend_from_slice(&ip.to_be_bytes());
            body
        }
        SocketAddr::V6(v6) => {
            let port = v6.port() ^ ((MAGIC >> 16) as u16);
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            if txid.len() < 12 {
                return None;
            }
            mask[4..].copy_from_slice(&txid[..12]);
            let raw = v6.ip().octets();
            let mut body = Vec::with_capacity(20);
            body.push(0);
            body.push(0x02);
            body.extend_from_slice(&port.to_be_bytes());
            for i in 0..16 {
                body.push(raw[i] ^ mask[i]);
            }
            body
        }
    };
    let mut attr = Vec::with_capacity(4 + body.len());
    attr.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
    attr.extend_from_slice(&(body.len() as u16).to_be_bytes());
    attr.extend_from_slice(&body);
    Some(attr)
}

fn parse_xor_mapped(msg: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if msg.len() < 20 {
        return None;
    }
    let typ = u16::from_be_bytes(msg[0..2].try_into().ok()?);
    if typ != BINDING_SUCCESS {
        return None;
    }
    let cookie = u32::from_be_bytes(msg[4..8].try_into().ok()?);
    if cookie != MAGIC || &msg[8..20] != txid {
        return None;
    }
    let mut i = 20;
    while i + 4 <= msg.len() {
        let at = u16::from_be_bytes(msg[i..i + 2].try_into().ok()?);
        let len = u16::from_be_bytes(msg[i + 2..i + 4].try_into().ok()?) as usize;
        i += 4;
        if i + len > msg.len() {
            return None;
        }
        let value = &msg[i..i + len];
        if at == ATTR_XOR_MAPPED {
            return decode_xor(value, txid);
        }
        i += (len + 3) & !3;
    }
    None
}

fn decode_xor(value: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let xport = u16::from_be_bytes(value[2..4].try_into().ok()?);
    let port = xport ^ ((MAGIC >> 16) as u16);
    match family {
        0x01 if value.len() >= 8 => {
            let xip = u32::from_be_bytes(value[4..8].try_into().ok()?);
            let ip = Ipv4Addr::from(xip ^ MAGIC);
            Some(SocketAddr::from((ip, port)))
        }
        0x02 if value.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC.to_be_bytes());
            mask[4..].copy_from_slice(txid);
            let mut raw = [0u8; 16];
            for i in 0..16 {
                raw[i] = value[4 + i] ^ mask[i];
            }
            Some(SocketAddr::from((Ipv6Addr::from(raw), port)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn local_stun_server_returns_the_client_address() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, src) = server.recv_from(&mut buf).unwrap();
            let resp = binding_success(&buf[..n], src).unwrap();
            server.send_to(&resp, src).unwrap();
        });
        let mapped = discover_one(&client, &server_addr.to_string(), Duration::from_secs(2))
            .expect("mapped address");
        assert_eq!(mapped, client_addr);
    }

    #[test]
    fn xor_mapped_v4_round_trip() {
        let request = binding_request();
        let txid: [u8; 12] = request[8..20].try_into().unwrap();
        let mapped: SocketAddr = "203.0.113.8:47321".parse().unwrap();
        let resp = binding_success(&request, mapped).unwrap();
        assert_eq!(parse_xor_mapped(&resp, &txid), Some(mapped));
    }
}
