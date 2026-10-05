//! Accountless UDP forwarder. QUIC stays end to end; this only moves datagrams
//! between two devices that have bound the same session.

use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use relay_core::DeviceId;
use relay_crypto::DeviceIdentity;

use crate::PeerConfig;
use crate::error::NetError;

const MAGIC: &[u8] = b"RLY1";
const KIND_BIND: u8 = 1;
const KIND_DATA: u8 = 2;
const SESSION_LEN: usize = 16;
const ID_LEN: usize = 32;
const SIG_LEN: usize = 64;

/// `magic || kind || session`.
pub(crate) const RELAY_HEADER_LEN: usize = 4 + 1 + SESSION_LEN;
/// BIND adds the sender id, the other device id, and a signature.
const BIND_LEN: usize = RELAY_HEADER_LEN + ID_LEN + ID_LEN + SIG_LEN;

/// Largest QUIC packet accepted inside a DATA frame.
pub(crate) const MAX_RELAY_PAYLOAD: usize = 1400;

/// A session that carried nothing for this long is forgotten; a device
/// binds again on its next dial.
const SESSION_IDLE: Duration = Duration::from_secs(60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);
/// Sessions a forwarder keeps at once. Any keypair can bind, so without a
/// cap the table is a memory sink for a public relay.
const MAX_SESSIONS: usize = 4096;

/// Quinn sizes its receive buffer from [`quinn::EndpointConfig::max_udp_payload_size`].
/// Keeping that at 1400 means a wrapped datagram must stay within 1400 bytes,
/// so the path MTU is 1400 minus the relay header.
pub(crate) const ENDPOINT_MAX_UDP_PAYLOAD: u16 = 1400;
pub(crate) const RELAY_PATH_MTU: u16 = ENDPOINT_MAX_UDP_PAYLOAD - RELAY_HEADER_LEN as u16;

/// Running forwarder. Dropping it stops the socket.
pub struct RelayServer {
    local_addr: SocketAddr,
    shutdown: Option<tokio::sync::mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl RelayServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        self.shutdown.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bind a UDP socket and forward between two peers per session.
pub fn serve_relay(bind: SocketAddr) -> Result<RelayServer, NetError> {
    let socket =
        std::net::UdpSocket::bind(bind).map_err(|source| NetError::Bind { addr: bind, source })?;
    socket.set_nonblocking(true)?;
    let local_addr = socket.local_addr()?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);

    let thread = std::thread::Builder::new()
        .name("relay-fwd".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = ready_tx.send(Err(NetError::Runtime(err.to_string())));
                    return;
                }
            };
            rt.block_on(async move {
                let sock = match tokio::net::UdpSocket::from_std(socket) {
                    Ok(sock) => sock,
                    Err(err) => {
                        let _ = ready_tx.send(Err(NetError::from(err)));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                run_relay(sock, shutdown_rx).await;
            });
        })
        .map_err(|err| NetError::Runtime(err.to_string()))?;

    ready_rx
        .recv()
        .map_err(|_| NetError::Runtime("relay thread exited during startup".into()))??;

    Ok(RelayServer {
        local_addr,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    })
}

/// Port of a `host:port` or `[ipv6]:port` string. `None` when the port is absent.
pub(crate) fn relay_port(addr: &str) -> Option<u16> {
    let addr = addr.trim();
    if addr.is_empty() {
        return None;
    }
    if let Ok(parsed) = addr.parse::<SocketAddr>() {
        return Some(parsed.port());
    }
    let (host, port) = addr.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    port.parse().ok()
}

/// First resolved address, preferring the QUIC socket's family when given.
pub(crate) async fn resolve_relay(target: &str, prefer_ipv4: Option<bool>) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host(target).await {
        Ok(iter) => iter.collect(),
        Err(err) => {
            tracing::debug!(addr = %target, error = %err, "relay address did not resolve");
            return None;
        }
    };
    if let Some(ipv4) = prefer_ipv4
        && let Some(addr) = addrs.iter().copied().find(|addr| addr.is_ipv4() == ipv4)
    {
        return Some(addr);
    }
    addrs.into_iter().next()
}

/// IPv4 in `198.18.0.0/15` derived from the device id. Never sent on the wire.
pub(crate) fn virtual_peer_addr(id: DeviceId) -> SocketAddr {
    let hash = blake3::hash(id.as_bytes());
    let bytes = hash.as_bytes();
    let second = 18u8 | (bytes[0] & 0x01);
    let ip = Ipv4Addr::new(198, second, bytes[1], bytes[2]);
    let mut port = u16::from_be_bytes([bytes[3], bytes[4]]);
    if port == 0 {
        port = 1;
    }
    SocketAddr::new(IpAddr::V4(ip), port)
}

pub(crate) fn session_id(a: DeviceId, b: DeviceId) -> [u8; SESSION_LEN] {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut material = [0u8; 64];
    material[..32].copy_from_slice(lo.as_bytes());
    material[32..].copy_from_slice(hi.as_bytes());
    let derived = blake3::derive_key("relay-fwd/1 session", &material);
    derived[..SESSION_LEN]
        .try_into()
        .expect("session id is 16 bytes")
}

fn bind_message(session: &[u8; SESSION_LEN], sender: &DeviceId, peer: &DeviceId) -> Vec<u8> {
    let mut message = Vec::with_capacity(16 + SESSION_LEN + ID_LEN + ID_LEN);
    message.extend_from_slice(b"relay-fwd/1 bind");
    message.extend_from_slice(session);
    message.extend_from_slice(sender.as_bytes());
    message.extend_from_slice(peer.as_bytes());
    message
}

fn encode_bind(our_id: DeviceId, peer: DeviceId, identity: &DeviceIdentity) -> io::Result<Vec<u8>> {
    let session = session_id(our_id, peer);
    let signature = identity
        .sign(&bind_message(&session, &our_id, &peer))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    let mut out = Vec::with_capacity(BIND_LEN);
    out.extend_from_slice(MAGIC);
    out.push(KIND_BIND);
    out.extend_from_slice(&session);
    out.extend_from_slice(our_id.as_bytes());
    out.extend_from_slice(peer.as_bytes());
    out.extend_from_slice(&signature);
    Ok(out)
}

fn encode_data(session: &[u8; SESSION_LEN], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RELAY_HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(KIND_DATA);
    out.extend_from_slice(session);
    out.extend_from_slice(payload);
    out
}

fn parse_data(packet: &[u8]) -> Option<([u8; SESSION_LEN], &[u8])> {
    if packet.len() <= RELAY_HEADER_LEN || &packet[..4] != MAGIC || packet[4] != KIND_DATA {
        return None;
    }
    let mut session = [0u8; SESSION_LEN];
    session.copy_from_slice(&packet[5..5 + SESSION_LEN]);
    let payload = &packet[RELAY_HEADER_LEN..];
    if payload.len() > MAX_RELAY_PAYLOAD {
        return None;
    }
    Some((session, payload))
}

struct Session {
    slots: Vec<Slot>,
    /// Last bind or forwarded datagram.
    last_seen: Instant,
}

struct Slot {
    device: DeviceId,
    addr: SocketAddr,
}

type Sessions = HashMap<[u8; SESSION_LEN], Session>;

async fn run_relay(sock: tokio::net::UdpSocket, mut shutdown: tokio::sync::mpsc::Receiver<()>) {
    let mut sessions = Sessions::new();
    let mut buf = vec![0u8; 65535];
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => break,
            _ = sweep.tick() => sweep_sessions(&mut sessions, Instant::now()),
            recv = sock.recv_from(&mut buf) => {
                match recv {
                    Ok((len, src)) => handle_datagram(&sock, &mut sessions, &buf[..len], src).await,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "relay recv failed");
                        break;
                    }
                }
            }
        }
    }
}

fn sweep_sessions(sessions: &mut Sessions, now: Instant) {
    sessions.retain(|_, session| now.duration_since(session.last_seen) < SESSION_IDLE);
}

async fn handle_datagram(
    sock: &tokio::net::UdpSocket,
    sessions: &mut Sessions,
    packet: &[u8],
    src: SocketAddr,
) {
    if packet.len() < 5 || &packet[..4] != MAGIC {
        return;
    }
    match packet[4] {
        KIND_BIND => apply_bind(sessions, packet, src),
        KIND_DATA => forward_data(sock, sessions, packet, src).await,
        _ => {}
    }
}

// Follow-up: a bind has no timestamp or nonce, so a captured one replayed
// from another source address moves the slot there until the device binds
// again. A signed timestamp in `bind_message` would stop that, but old
// devices do not send one, so it waits for a protocol bump (see D34).
fn apply_bind(sessions: &mut Sessions, packet: &[u8], src: SocketAddr) {
    if packet.len() != BIND_LEN {
        return;
    }
    let mut session = [0u8; SESSION_LEN];
    session.copy_from_slice(&packet[5..5 + SESSION_LEN]);
    let mut raw_id = [0u8; ID_LEN];
    raw_id.copy_from_slice(&packet[RELAY_HEADER_LEN..RELAY_HEADER_LEN + ID_LEN]);
    let sender = DeviceId::from_bytes(raw_id);
    let mut raw_peer = [0u8; ID_LEN];
    raw_peer
        .copy_from_slice(&packet[RELAY_HEADER_LEN + ID_LEN..RELAY_HEADER_LEN + ID_LEN + ID_LEN]);
    let peer = DeviceId::from_bytes(raw_peer);
    let signature = &packet[RELAY_HEADER_LEN + ID_LEN + ID_LEN..];
    if session_id(sender, peer) != session {
        tracing::debug!(%src, "relay bind rejected: session does not match the two devices");
        return;
    }
    if !relay_crypto::verify_device(&sender, &bind_message(&session, &sender, &peer), signature) {
        tracing::debug!(%src, "relay bind rejected: bad signature");
        return;
    }
    if !sessions.contains_key(&session) && sessions.len() >= MAX_SESSIONS {
        tracing::debug!(%src, "relay bind ignored: session table is full");
        return;
    }
    let now = Instant::now();
    let entry = sessions.entry(session).or_insert_with(|| Session {
        slots: Vec::new(),
        last_seen: now,
    });
    entry.last_seen = now;
    if let Some(slot) = entry.slots.iter_mut().find(|slot| slot.device == sender) {
        slot.addr = src;
        return;
    }
    if entry.slots.len() >= 2 || entry.slots.iter().any(|slot| slot.device != peer) {
        tracing::debug!(%src, "relay bind ignored: session already has two devices");
        return;
    }
    entry.slots.push(Slot {
        device: sender,
        addr: src,
    });
}

async fn forward_data(
    sock: &tokio::net::UdpSocket,
    sessions: &mut Sessions,
    packet: &[u8],
    src: SocketAddr,
) {
    let Some((session, payload)) = parse_data(packet) else {
        return;
    };
    let Some(state) = sessions.get_mut(&session) else {
        return;
    };
    let Some(from) = state.slots.iter().find(|slot| slot.addr == src) else {
        return;
    };
    state.last_seen = Instant::now();
    let Some(dest) = state
        .slots
        .iter()
        .find(|slot| slot.device != from.device)
        .map(|slot| slot.addr)
    else {
        return;
    };
    let frame = encode_data(&session, payload);
    if let Err(err) = sock.send_to(&frame, dest).await {
        tracing::debug!(error = %err, %dest, "relay forward failed");
    }
}

/// Quinn socket that wraps datagrams for a configured relay.
pub(crate) struct RelaySocket {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    relay: Mutex<Option<SocketAddr>>,
    our_id: DeviceId,
    trusted: Arc<RwLock<HashMap<DeviceId, PeerConfig>>>,
}

impl RelaySocket {
    pub(crate) fn new(
        inner: Arc<dyn quinn::AsyncUdpSocket>,
        our_id: DeviceId,
        trusted: Arc<RwLock<HashMap<DeviceId, PeerConfig>>>,
    ) -> Self {
        Self {
            inner,
            relay: Mutex::new(None),
            our_id,
            trusted,
        }
    }

    pub(crate) fn set_relay(&self, addr: Option<SocketAddr>) {
        *self.relay.lock().unwrap_or_else(|err| err.into_inner()) = addr;
    }

    fn relay_addr(&self) -> Option<SocketAddr> {
        *self.relay.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// One BIND on the Quinn socket. Not a QUIC packet.
    pub(crate) async fn send_bind(
        &self,
        peer: DeviceId,
        identity: &DeviceIdentity,
    ) -> io::Result<()> {
        let Some(relay) = self.relay_addr() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "relay address is not set",
            ));
        };
        let packet = encode_bind(self.our_id, peer, identity)?;
        let mut poller = self.inner.clone().create_io_poller();
        std::future::poll_fn(|cx| {
            let transmit = quinn::udp::Transmit {
                destination: relay,
                ecn: None,
                contents: &packet,
                segment_size: None,
                src_ip: None,
            };
            match self.inner.try_send(&transmit) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    match poller.as_mut().poll_writable(cx) {
                        Poll::Ready(Ok(())) => {
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                        Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                        Poll::Pending => Poll::Pending,
                    }
                }
                Err(err) => Poll::Ready(Err(err)),
            }
        })
        .await
    }

    fn virtual_for_session(&self, session: &[u8; SESSION_LEN]) -> Option<SocketAddr> {
        let trusted = self.trusted.read().unwrap_or_else(|err| err.into_inner());
        trusted.keys().find_map(|id| {
            if *id == self.our_id {
                return None;
            }
            if session_id(self.our_id, *id) == *session {
                Some(virtual_peer_addr(*id))
            } else {
                None
            }
        })
    }

    fn peer_for_virtual(&self, dest: SocketAddr) -> Option<DeviceId> {
        let trusted = self.trusted.read().unwrap_or_else(|err| err.into_inner());
        trusted
            .keys()
            .copied()
            .find(|id| *id != self.our_id && virtual_peer_addr(*id) == dest)
    }
}

impl std::fmt::Debug for RelaySocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySocket")
            .field("relay", &self.relay_addr())
            .field("our_id", &self.our_id)
            .finish_non_exhaustive()
    }
}

impl quinn::AsyncUdpSocket for RelaySocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        let relay = self.relay_addr();
        let peer = relay.and_then(|_| self.peer_for_virtual(transmit.destination));
        if let (Some(relay), Some(peer)) = (relay, peer) {
            let session = session_id(self.our_id, peer);
            let wrapped = encode_data(&session, transmit.contents);
            let outbound = quinn::udp::Transmit {
                destination: relay,
                ecn: transmit.ecn,
                contents: &wrapped,
                segment_size: None,
                src_ip: transmit.src_ip,
            };
            return self.inner.try_send(&outbound);
        }
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let count = std::task::ready!(self.inner.poll_recv(cx, bufs, meta))?;
            if count == 0 {
                return Poll::Ready(Ok(0));
            }
            let relay = self.relay_addr();
            let from_relay =
                relay.is_some_and(|addr| meta[..count].iter().any(|item| item.addr == addr));
            if !from_relay {
                return Poll::Ready(Ok(count));
            }
            let mut kept: Vec<Kept> = Vec::new();
            for index in 0..count {
                self.collect_datagram(&bufs[index], &meta[index], relay, &mut kept);
            }
            if kept.is_empty() {
                continue;
            }
            let written = kept.len().min(bufs.len());
            for (index, packet) in kept.into_iter().take(written).enumerate() {
                let len = packet.bytes.len().min(bufs[index].len());
                bufs[index][..len].copy_from_slice(&packet.bytes[..len]);
                meta[index] = packet.meta;
                meta[index].len = len;
                if meta[index].stride == 0 || meta[index].stride > len {
                    meta[index].stride = len;
                }
            }
            return Poll::Ready(Ok(written));
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

struct Kept {
    bytes: Vec<u8>,
    meta: quinn::udp::RecvMeta,
}

impl RelaySocket {
    fn collect_datagram(
        &self,
        buf: &[u8],
        meta: &quinn::udp::RecvMeta,
        relay: Option<SocketAddr>,
        kept: &mut Vec<Kept>,
    ) {
        let len = meta.len.min(buf.len());
        let from_relay = relay.is_some_and(|addr| meta.addr == addr);
        if !from_relay {
            kept.push(Kept {
                bytes: buf[..len].to_vec(),
                meta: *meta,
            });
            return;
        }
        let stride = if meta.stride == 0 || meta.stride > len {
            len
        } else {
            meta.stride
        };
        if stride == 0 {
            return;
        }
        let mut offset = 0;
        while offset < len {
            let seg_len = stride.min(len - offset);
            if let Some((session, payload)) = parse_data(&buf[offset..offset + seg_len])
                && let Some(addr) = self.virtual_for_session(&session)
            {
                let mut rewritten = *meta;
                rewritten.addr = addr;
                rewritten.len = payload.len();
                rewritten.stride = payload.len();
                kept.push(Kept {
                    bytes: payload.to_vec(),
                    meta: rewritten,
                });
            }
            offset += seg_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_sessions_are_swept_and_the_table_is_capped() {
        let mut sessions = Sessions::new();
        let bound = |n: u8| {
            let dir = tempfile::tempdir().unwrap();
            let identity = DeviceIdentity::generate(dir.path()).unwrap();
            let peer = DeviceId::from_bytes([n; 32]);
            encode_bind(identity.device_id(), peer, &identity).unwrap()
        };
        let src: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        apply_bind(&mut sessions, &bound(1), src);
        apply_bind(&mut sessions, &bound(2), src);
        assert_eq!(sessions.len(), 2);

        // Nothing is swept while sessions are fresh; everything idle goes.
        sweep_sessions(&mut sessions, Instant::now());
        assert_eq!(sessions.len(), 2);
        let later = Instant::now() + SESSION_IDLE + Duration::from_secs(1);
        sessions.values_mut().next().unwrap().last_seen = later;
        sweep_sessions(&mut sessions, later);
        assert_eq!(sessions.len(), 1, "only the touched session survives");

        // A full table takes re-binds for known sessions but no new ones.
        let kept = *sessions.keys().next().unwrap();
        for n in 0..MAX_SESSIONS {
            let mut key = [0u8; SESSION_LEN];
            key[..2].copy_from_slice(&(n as u16).to_be_bytes());
            key[2] = 7;
            sessions.insert(
                key,
                Session {
                    slots: Vec::new(),
                    last_seen: Instant::now(),
                },
            );
        }
        let full = sessions.len();
        assert!(full >= MAX_SESSIONS);
        apply_bind(&mut sessions, &bound(3), src);
        assert_eq!(sessions.len(), full, "a bind past the cap is ignored");
        assert!(sessions.contains_key(&kept));
    }

    #[test]
    fn session_id_ignores_device_order() {
        let a = DeviceId::from_bytes([1; 32]);
        let b = DeviceId::from_bytes([2; 32]);
        assert_eq!(session_id(a, b), session_id(b, a));
        assert_ne!(session_id(a, a), session_id(a, b));
    }

    #[test]
    fn virtual_addr_is_benchmark_prefix_and_nonzero_port() {
        let id = DeviceId::from_bytes([0xab; 32]);
        let addr = virtual_peer_addr(id);
        let IpAddr::V4(ip) = addr.ip() else {
            panic!("virtual peer address is ipv4");
        };
        let octets = ip.octets();
        assert_eq!(octets[0], 198);
        assert!(octets[1] == 18 || octets[1] == 19, "{ip}");
        assert_ne!(addr.port(), 0);
    }

    #[test]
    fn forwards_only_between_the_two_bound_devices() {
        let server = serve_relay("127.0.0.1:0".parse().unwrap()).unwrap();
        let relay = server.local_addr();
        let alice_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let bob_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        alice_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        bob_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let alice_dir = tempfile::tempdir().unwrap();
        let bob_dir = tempfile::tempdir().unwrap();
        let alice = DeviceIdentity::generate(alice_dir.path()).unwrap();
        let bob = DeviceIdentity::generate(bob_dir.path()).unwrap();
        let alice_bind = encode_bind(alice.device_id(), bob.device_id(), &alice).unwrap();
        let bob_bind = encode_bind(bob.device_id(), alice.device_id(), &bob).unwrap();
        let session = session_id(alice.device_id(), bob.device_id());
        let payload = b"quic-packet";
        let data = encode_data(&session, payload);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = None;
        while Instant::now() < deadline {
            alice_sock.send_to(&alice_bind, relay).unwrap();
            bob_sock.send_to(&bob_bind, relay).unwrap();
            alice_sock.send_to(&data, relay).unwrap();
            let mut buf = [0u8; 2048];
            if let Ok((len, _)) = bob_sock.recv_from(&mut buf) {
                got = Some(buf[..len].to_vec());
                break;
            }
        }
        let got = got.expect("bob did not receive a forwarded datagram");
        let (_, forwarded) = parse_data(&got).expect("forwarded DATA frame");
        assert_eq!(forwarded, payload);

        let carol_dir = tempfile::tempdir().unwrap();
        let carol = DeviceIdentity::generate(carol_dir.path()).unwrap();
        let carol_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let carol_bind = encode_bind(carol.device_id(), alice.device_id(), &carol).unwrap();
        carol_sock.send_to(&carol_bind, relay).unwrap();
        let carol_data = encode_data(&session_id(carol.device_id(), alice.device_id()), b"nope");
        carol_sock.send_to(&carol_data, relay).unwrap();
        alice_sock
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        let mut buf = [0u8; 2048];
        assert!(
            alice_sock.recv_from(&mut buf).is_err(),
            "a third device was forwarded"
        );

        let sig = carol
            .sign(&bind_message(
                &session,
                &carol.device_id(),
                &alice.device_id(),
            ))
            .unwrap();
        let mut squat = Vec::new();
        squat.extend_from_slice(MAGIC);
        squat.push(KIND_BIND);
        squat.extend_from_slice(&session);
        squat.extend_from_slice(carol.device_id().as_bytes());
        squat.extend_from_slice(alice.device_id().as_bytes());
        squat.extend_from_slice(&sig);
        carol_sock.send_to(&squat, relay).unwrap();
        let again = encode_data(&session, b"still-bob");
        alice_sock.send_to(&again, relay).unwrap();
        let mut forwarded = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok((len, _)) = bob_sock.recv_from(&mut buf) {
                forwarded = Some(buf[..len].to_vec());
                break;
            }
        }
        let forwarded = forwarded.expect("bob still receives after a squatted bind");
        let (_, payload) = parse_data(&forwarded).expect("DATA frame");
        assert_eq!(payload, b"still-bob");
    }
}
