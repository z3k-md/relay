use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use rand::RngCore;
use relay_core::remote::{
    RemoteCall, RemoteError, RemoteErrorCode, RemoteReply, RemoteResult, RemoteRoot,
};
use relay_core::{DeviceId, ObjectId, PairingCode};
use relay_crypto::DeviceIdentity;
use relay_net::{
    ControlHandler, NetCommand, NetConfig, NetEvent, NetHandle, PeerConfig, serve_relay, start,
};
use relay_proto::{Ack, frame};
use relay_store::ObjectStore;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);

struct Events {
    rx: Receiver<NetEvent>,
    got: Vec<NetEvent>,
    next: usize,
}

impl Events {
    fn new(rx: Receiver<NetEvent>) -> Self {
        Self {
            rx,
            got: Vec::new(),
            next: 0,
        }
    }

    fn wait_match<T>(&mut self, timeout: Duration, f: impl FnMut(&NetEvent) -> Option<T>) -> T {
        match self.poll_match(timeout, f) {
            Some(t) => t,
            None => panic!("timeout waiting for event; have {:#?}", self.got),
        }
    }

    fn poll_match<T>(
        &mut self,
        timeout: Duration,
        mut f: impl FnMut(&NetEvent) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            while self.next < self.got.len() {
                let ev = &self.got[self.next];
                self.next += 1;
                if let Some(t) = f(ev) {
                    return Some(t);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(left) {
                Ok(ev) => self.got.push(ev),
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("event channel closed; have {:#?}", self.got)
                }
            }
        }
    }

    fn collect_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(ev) => self.got.push(ev),
                Err(_) => break,
            }
        }
    }

    fn connected_count(&self) -> usize {
        self.got
            .iter()
            .filter(|e| matches!(e, NetEvent::PeerConnected { .. }))
            .count()
    }

    fn has_connected(&self) -> bool {
        self.connected_count() > 0
    }
}

struct Node {
    handle: NetHandle,
    events: Events,
    id: DeviceId,
    identity_dir: TempDir,
    store: ObjectStore,
    _store_dir: TempDir,
}

fn listen() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn start_from_identity(
    name: &str,
    identity: Arc<DeviceIdentity>,
    identity_dir: TempDir,
    peers: Vec<PeerConfig>,
    listen: SocketAddr,
    relay: Option<String>,
) -> Node {
    start_node(name, identity, identity_dir, peers, listen, relay, None)
}

fn start_node(
    name: &str,
    identity: Arc<DeviceIdentity>,
    identity_dir: TempDir,
    peers: Vec<PeerConfig>,
    listen: SocketAddr,
    relay: Option<String>,
    control: Option<Arc<dyn ControlHandler>>,
) -> Node {
    let store_dir = TempDir::new().unwrap();
    let store = ObjectStore::open(store_dir.path()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = start(
        NetConfig {
            identity: identity.clone(),
            device_name: name.to_owned(),
            listen,
            peers,
            store_root: store_dir.path().to_owned(),
            enable_stun: false,
            relay,
            serve_relay: false,
            control,
        },
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )
    .expect("start net");
    Node {
        id: identity.device_id(),
        handle,
        events: Events::new(rx),
        identity_dir,
        store,
        _store_dir: store_dir,
    }
}

fn spawn(name: &str, peers: Vec<PeerConfig>) -> Node {
    let identity_dir = TempDir::new().unwrap();
    let identity = Arc::new(DeviceIdentity::generate(identity_dir.path()).unwrap());
    start_from_identity(name, identity, identity_dir, peers, listen(), None)
}

fn trust(id: DeviceId, name: &str, addr: Option<SocketAddr>) -> PeerConfig {
    PeerConfig {
        id,
        name: name.to_owned(),
        addresses: addr.map(|a| a.to_string()).into_iter().collect(),
        may_manage: false,
    }
}

fn wait_connected(events: &mut Events, peer: DeviceId, name: &str) {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::PeerConnected {
            peer: p, name: n, ..
        } if *p == peer && n == name => Some(()),
        _ => None,
    });
}

fn wait_disconnected(events: &mut Events, peer: DeviceId) {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::PeerDisconnected { peer: p, .. } if *p == peer => Some(()),
        _ => None,
    });
}

fn wait_ack(events: &mut Events, from: DeviceId, seq: u64) {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::Frame {
            peer,
            body: frame::Body::Ack(ack),
        } if *peer == from && ack.through_sequence == seq => Some(()),
        _ => None,
    });
}

/// Frames are delivered at most once: one sent while a duplicate connection
/// is being replaced is dropped. The engine re-requests on every reconnect;
/// this resends until the frame arrives.
fn deliver_ack(from: &NetHandle, from_id: DeviceId, to: &mut Events, to_id: DeviceId, seq: u64) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        send_ack(from, to_id, seq);
        let got = to.poll_match(Duration::from_millis(500), |ev| match ev {
            NetEvent::Frame {
                peer,
                body: frame::Body::Ack(ack),
            } if *peer == from_id && ack.through_sequence == seq => Some(()),
            _ => None,
        });
        if got.is_some() {
            return;
        }
    }
    panic!("ack {seq} never delivered; have {:#?}", to.got);
}

fn send_ack(handle: &NetHandle, peer: DeviceId, seq: u64) {
    handle.send(NetCommand::Send {
        peer,
        body: frame::Body::Ack(Ack {
            space_id: vec![1],
            through_sequence: seq,
        }),
    });
}

fn pair_alice_dials_bob() -> (Node, Node) {
    let mut bob = spawn("bob", vec![]);
    let bob_id = bob.id;
    let bob_addr = bob.handle.local_addr();
    let mut alice = spawn("alice", vec![trust(bob_id, "bob", Some(bob_addr))]);
    bob.handle
        .send(NetCommand::SetPeers(vec![trust(alice.id, "alice", None)]));
    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice.id, "alice");
    (alice, bob)
}

#[test]
fn two_nodes_connect_and_exchange_frames() {
    let (alice, bob) = pair_alice_dials_bob();
    let alice_id = alice.id;
    let bob_id = bob.id;
    let mut alice = alice;
    let mut bob = bob;

    send_ack(&alice.handle, bob_id, 1);
    send_ack(&alice.handle, bob_id, 2);
    wait_ack(&mut bob.events, alice_id, 1);
    wait_ack(&mut bob.events, alice_id, 2);

    send_ack(&bob.handle, alice_id, 10);
    send_ack(&bob.handle, alice_id, 11);
    wait_ack(&mut alice.events, bob_id, 10);
    wait_ack(&mut alice.events, bob_id, 11);
}

#[test]
fn simultaneous_dial_keeps_one_connection() {
    let mut alice = spawn("alice", vec![]);
    let mut bob = spawn("bob", vec![]);
    let alice_id = alice.id;
    let bob_id = bob.id;
    let alice_addr = alice.handle.local_addr();
    let bob_addr = bob.handle.local_addr();

    alice.handle.send(NetCommand::SetPeers(vec![trust(
        bob_id,
        "bob",
        Some(bob_addr),
    )]));
    bob.handle.send(NetCommand::SetPeers(vec![trust(
        alice_id,
        "alice",
        Some(alice_addr),
    )]));

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");

    deliver_ack(&alice.handle, alice_id, &mut bob.events, bob_id, 1);
    deliver_ack(&bob.handle, bob_id, &mut alice.events, alice_id, 2);

    alice.events.collect_for(Duration::from_secs(3));
    bob.events.collect_for(Duration::from_secs(3));
    assert!(
        alice.events.connected_count() <= 3,
        "alice reconnect churn: {:#?}",
        alice.events.got
    );
    assert!(
        bob.events.connected_count() <= 3,
        "bob reconnect churn: {:#?}",
        bob.events.got
    );

    deliver_ack(&alice.handle, alice_id, &mut bob.events, bob_id, 3);
    deliver_ack(&bob.handle, bob_id, &mut alice.events, alice_id, 4);
}

#[test]
fn untrusted_node_cannot_connect() {
    let mut alice = spawn("alice", vec![]);
    let alice_addr = alice.handle.local_addr();

    let mut eve = spawn("eve", vec![trust(alice.id, "alice", Some(alice_addr))]);

    eve.events.collect_for(Duration::from_secs(2));
    alice.events.collect_for(Duration::from_secs(2));
    assert!(
        !eve.events.has_connected(),
        "eve should not connect: {:#?}",
        eve.events.got
    );
    assert!(
        !alice.events.has_connected(),
        "alice should not connect: {:#?}",
        alice.events.got
    );
}

/// Counts, per peer id, how often this process refused a peer after the
/// handshake. Neither side emits an event for that, so the test reads the
/// warning the accepting side logs.
#[derive(Default)]
struct Rejections {
    by_peer: Mutex<HashMap<String, usize>>,
}

static REJECTIONS: OnceLock<Arc<Rejections>> = OnceLock::new();

struct RejectionLayer(Arc<Rejections>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RejectionLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        #[derive(Default)]
        struct Fields {
            message: String,
            peer: String,
        }
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = format!("{value:?}"),
                    "peer" => self.peer = format!("{value:?}"),
                    _ => {}
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.message == "peer is not in the trusted set after handshake" {
            *self
                .0
                .by_peer
                .lock()
                .unwrap()
                .entry(fields.peer)
                .or_default() += 1;
        }
    }
}

fn rejections() -> Arc<Rejections> {
    REJECTIONS
        .get_or_init(|| {
            use tracing_subscriber::layer::SubscriberExt;
            let counter = Arc::new(Rejections::default());
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(RejectionLayer(counter.clone())),
            )
            .expect("no other global tracing subscriber in this test binary");
            counter
        })
        .clone()
}

/// A peer that completes the handshake and then closes (here: it does not
/// trust the dialer) is redialed with backoff, not in a hot loop.
#[test]
fn rejected_dialer_backs_off() {
    let counter = rejections();
    let alice = spawn("alice", vec![]);
    let alice_addr = alice.handle.local_addr();
    let eve = spawn("eve", vec![trust(alice.id, "alice", Some(alice_addr))]);
    let attempts = || {
        counter
            .by_peer
            .lock()
            .unwrap()
            .get(&eve.id.to_string())
            .copied()
            .unwrap_or(0)
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    while attempts() == 0 {
        assert!(Instant::now() < deadline, "eve never reached alice");
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = attempts();
    std::thread::sleep(Duration::from_secs(3));
    let redials = attempts() - first;
    // Delays of 1 s and 2 s allow at most three redials in the 3 s after the
    // first rejection; the unfixed loop made about a hundred per second.
    assert!(
        (1..=3).contains(&redials),
        "eve redialed {redials} times in the 3 s after her first rejection"
    );
}

/// The daemon stores and shows a name from the wire, so it is held to
/// the same rule as a pairing name: one that would not pass locally
/// becomes the id's short form.
#[test]
fn hello_name_that_fails_validation_becomes_the_short_id() {
    let mut bob = spawn("bob\u{7}", vec![]);
    let bob_id = bob.id;
    let bob_addr = bob.handle.local_addr();
    let mut alice = spawn(&"a".repeat(65), vec![trust(bob_id, "bob", Some(bob_addr))]);
    bob.handle
        .send(NetCommand::SetPeers(vec![trust(alice.id, "alice", None)]));
    wait_connected(&mut alice.events, bob_id, &bob_id.short());
    wait_connected(&mut bob.events, alice.id, &alice.id.short());
}

/// Emptying a trusted peer's addresses stops its dialer. The connection it
/// drove must close for real, or the peer keeps a session the dialing
/// engine was told is gone.
#[test]
fn stopping_a_dialer_closes_its_connection() {
    let (mut alice, mut bob) = pair_alice_dials_bob();
    let alice_id = alice.id;
    let bob_id = bob.id;

    alice
        .handle
        .send(NetCommand::SetPeers(vec![trust(bob_id, "bob", None)]));
    wait_disconnected(&mut alice.events, bob_id);
    wait_disconnected(&mut bob.events, alice_id);
}

#[test]
fn wrong_key_at_dialed_address_is_rejected() {
    let bob_dir = TempDir::new().unwrap();
    let bob_id = DeviceIdentity::generate(bob_dir.path())
        .unwrap()
        .device_id();

    let charlie = spawn("charlie", vec![]);
    let mut alice = spawn(
        "alice",
        vec![trust(bob_id, "bob", Some(charlie.handle.local_addr()))],
    );

    alice.events.collect_for(Duration::from_secs(2));
    assert!(
        !alice.events.has_connected(),
        "alice connected to the wrong key: {:#?}",
        alice.events.got
    );
}

#[test]
fn object_fetch_round_trip_and_missing() {
    let (alice, mut bob) = pair_alice_dials_bob();
    let alice_id = alice.id;

    let mut data = vec![0u8; 3 * 1024 * 1024];
    rand::thread_rng().fill_bytes(&mut data);
    let id = alice.store.put_bytes(&data).unwrap();

    bob.handle.send(NetCommand::FetchObject {
        peer: alice_id,
        object: id,
    });
    bob.events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::ObjectFetched { peer, object } if *peer == alice_id && *object == id => Some(()),
        NetEvent::ObjectFetchFailed {
            peer,
            object,
            reason,
            ..
        } if *peer == alice_id && *object == id => {
            panic!("fetch failed: {reason}")
        }
        _ => None,
    });
    assert_eq!(bob.store.read(&id).unwrap(), data);

    let missing = ObjectId::of(b"definitely-not-stored");
    bob.handle.send(NetCommand::FetchObject {
        peer: alice_id,
        object: missing,
    });
    bob.events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::ObjectFetchFailed {
            peer,
            object,
            not_found,
            ..
        } if *peer == alice_id && *object == missing && *not_found => Some(()),
        _ => None,
    });
}

#[test]
fn shutdown_returns_promptly_with_live_peer() {
    // Previously waited up to 2s on Quinn wait_idle's close timer. A short
    // SHUTDOWN_IDLE_WAIT must keep this well under a second with a live session.
    let mut bob = spawn("bob", vec![]);
    let bob_id = bob.id;
    let bob_addr = bob.handle.local_addr();
    let mut alice = spawn("alice", vec![trust(bob_id, "bob", Some(bob_addr))]);
    let alice_id = alice.id;
    bob.handle
        .send(NetCommand::SetPeers(vec![trust(alice_id, "alice", None)]));

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");

    let started = Instant::now();
    alice.handle.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(750),
        "shutdown with live peer took {elapsed:?}; likely still awaiting long QUIC wait_idle"
    );
}

#[test]
fn reconnects_after_peer_restart() {
    let bob_id_dir = TempDir::new().unwrap();
    let identity = Arc::new(DeviceIdentity::generate(bob_id_dir.path()).unwrap());
    let bob_id = identity.device_id();
    let mut bob = start_from_identity("bob", identity, bob_id_dir, vec![], listen(), None);
    let restart_addr = bob.handle.local_addr();

    let mut alice = spawn("alice", vec![trust(bob_id, "bob", Some(restart_addr))]);
    let alice_id = alice.id;
    bob.handle
        .send(NetCommand::SetPeers(vec![trust(alice_id, "alice", None)]));

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");

    let restart_dir = bob.identity_dir;
    bob.handle.shutdown();
    wait_disconnected(&mut alice.events, bob_id);

    let identity = Arc::new(DeviceIdentity::load(restart_dir.path()).unwrap());
    assert_eq!(identity.device_id(), bob_id);
    let mut bob = start_from_identity(
        "bob",
        identity,
        restart_dir,
        vec![trust(alice_id, "alice", None)],
        restart_addr,
        None,
    );

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");
}

fn wait_paired(events: &mut Events, peer: DeviceId) -> (String, Vec<String>, bool) {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::Paired {
            peer: p,
            name,
            addresses,
            initiator,
        } if *p == peer => Some((name.clone(), addresses.clone(), *initiator)),
        _ => None,
    })
}

fn wait_pair_failed(events: &mut Events) -> String {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::PairFailed { reason } => Some(reason.clone()),
        _ => None,
    })
}

fn pair_code_with_same_nameplate(code: &PairingCode) -> String {
    let mut digits = code.digits().to_owned();
    let last = digits.pop().unwrap();
    digits.push(if last == '0' { '1' } else { '0' });
    digits
}

#[test]
fn pairing_correct_code_exchanges_ids_and_addresses() {
    let mut alice = spawn("alice", vec![]);
    let mut bob = spawn("bob", vec![]);
    let alice_id = alice.id;
    let bob_id = bob.id;
    let alice_addr = alice.handle.local_addr().to_string();
    let code = PairingCode::generate();

    alice.handle.send(NetCommand::PairStart {
        code: code.digits().to_owned(),
        expires_at: SystemTime::now() + Duration::from_secs(60),
    });
    std::thread::sleep(Duration::from_millis(50));
    bob.handle.send(NetCommand::PairJoin {
        code: code.digits().to_owned(),
        addr: Some(alice_addr),
    });

    let (bob_name, bob_addrs, alice_init) = wait_paired(&mut alice.events, bob_id);
    let (alice_name, alice_addrs, bob_init) = wait_paired(&mut bob.events, alice_id);
    assert!(alice_init);
    assert!(!bob_init);
    assert_eq!(bob_name, "bob");
    assert_eq!(alice_name, "alice");
    assert!(
        bob_addrs
            .iter()
            .any(|a| a.contains(&bob.handle.local_addr().port().to_string())
                || a.contains("127.0.0.1")),
        "bob addrs: {bob_addrs:?}"
    );
    assert!(
        alice_addrs
            .iter()
            .any(|a| a.contains("127.0.0.1")
                || a.contains(&alice.handle.local_addr().ip().to_string())),
        "alice addrs: {alice_addrs:?}"
    );
}

#[test]
fn pairing_wrong_code_aborts_session() {
    let mut alice = spawn("alice", vec![]);
    let mut bob = spawn("bob", vec![]);
    let alice_addr = alice.handle.local_addr().to_string();
    let code = PairingCode::generate();
    let wrong = pair_code_with_same_nameplate(&code);

    alice.handle.send(NetCommand::PairStart {
        code: code.digits().to_owned(),
        expires_at: SystemTime::now() + Duration::from_secs(60),
    });
    std::thread::sleep(Duration::from_millis(50));
    bob.handle.send(NetCommand::PairJoin {
        code: wrong,
        addr: Some(alice_addr.clone()),
    });

    let reason = wait_pair_failed(&mut bob.events);
    assert!(
        reason.contains("confirm") || reason.contains("pairing"),
        "{reason}"
    );
    let _ = wait_pair_failed(&mut alice.events);

    bob.handle.send(NetCommand::PairJoin {
        code: code.digits().to_owned(),
        addr: Some(alice_addr),
    });
    let again = wait_pair_failed(&mut bob.events);
    assert!(
        !again.is_empty(),
        "correct retry after a failed attempt must fail: {again}"
    );
    alice.events.collect_for(Duration::from_millis(400));
    assert!(
        !alice
            .events
            .got
            .iter()
            .any(|e| matches!(e, NetEvent::Paired { .. })),
        "initiator must not pair after a failed attempt: {:#?}",
        alice.events.got
    );
}

#[test]
fn pairing_expired_session_is_rejected() {
    let mut alice = spawn("alice", vec![]);
    let mut bob = spawn("bob", vec![]);
    let alice_addr = alice.handle.local_addr().to_string();
    let code = PairingCode::generate();

    alice.handle.send(NetCommand::PairStart {
        code: code.digits().to_owned(),
        expires_at: SystemTime::now() - Duration::from_secs(1),
    });
    let _ = wait_pair_failed(&mut alice.events);

    bob.handle.send(NetCommand::PairJoin {
        code: code.digits().to_owned(),
        addr: Some(alice_addr),
    });
    let reason = wait_pair_failed(&mut bob.events);
    assert!(!reason.is_empty(), "{reason}");
}

#[test]
fn pairing_alpn_without_session_is_closed() {
    let alice = spawn("alice", vec![]);
    let mut bob = spawn("bob", vec![]);
    let alice_addr = alice.handle.local_addr().to_string();
    let code = PairingCode::generate();

    bob.handle.send(NetCommand::PairJoin {
        code: code.digits().to_owned(),
        addr: Some(alice_addr),
    });
    let reason = wait_pair_failed(&mut bob.events);
    assert!(!reason.is_empty(), "{reason}");
}

#[test]
fn set_peers_removing_peer_disconnects() {
    let (mut alice, mut bob) = pair_alice_dials_bob();
    let alice_id = alice.id;
    let bob_id = bob.id;

    alice.handle.send(NetCommand::SetPeers(vec![]));
    wait_disconnected(&mut alice.events, bob_id);
    wait_disconnected(&mut bob.events, alice_id);
}

#[test]
fn relay_connects_when_direct_address_is_a_blackhole() {
    let server = serve_relay("127.0.0.1:0".parse().unwrap()).unwrap();
    let relay = server.local_addr().to_string();
    let blackhole = vec!["203.0.113.1:9".to_owned()];

    let alice_dir = TempDir::new().unwrap();
    let alice_identity = Arc::new(DeviceIdentity::generate(alice_dir.path()).unwrap());
    let bob_dir = TempDir::new().unwrap();
    let bob_identity = Arc::new(DeviceIdentity::generate(bob_dir.path()).unwrap());
    let alice_id = alice_identity.device_id();
    let bob_id = bob_identity.device_id();

    let mut alice = start_from_identity(
        "alice",
        alice_identity,
        alice_dir,
        vec![PeerConfig {
            id: bob_id,
            name: "bob".to_owned(),
            addresses: blackhole.clone(),
            may_manage: false,
        }],
        listen(),
        Some(relay.clone()),
    );
    let mut bob = start_from_identity(
        "bob",
        bob_identity,
        bob_dir,
        vec![PeerConfig {
            id: alice_id,
            name: "alice".to_owned(),
            addresses: blackhole,
            may_manage: false,
        }],
        listen(),
        Some(relay),
    );

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");
    deliver_ack(&alice.handle, alice_id, &mut bob.events, bob_id, 7);
}

/// Answers `Roots` with the caller's id and counts how often it ran.
#[derive(Default)]
struct EchoHandler {
    calls: std::sync::atomic::AtomicUsize,
}

impl ControlHandler for EchoHandler {
    fn handle(&self, peer: DeviceId, call: RemoteCall) -> RemoteResult {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match call {
            RemoteCall::Roots => Ok(RemoteReply::Roots {
                roots: vec![RemoteRoot {
                    name: "caller".into(),
                    path: peer.to_string(),
                }],
            }),
            _ => Err(RemoteError::new(RemoteErrorCode::Unsupported, "echo")),
        }
    }

    fn open_file(
        &self,
        _peer: DeviceId,
        path: &str,
        max_bytes: u64,
    ) -> Result<std::fs::File, RemoteError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let file = std::fs::File::open(path)
            .map_err(|e| RemoteError::new(RemoteErrorCode::NotFound, e.to_string()))?;
        if file.metadata().unwrap().len() > max_bytes {
            return Err(RemoteError::new(RemoteErrorCode::Invalid, "too large"));
        }
        Ok(file)
    }
}

/// Alice answers calls; bob calls her. `grant` is whether alice lets bob
/// manage her.
fn managed_pair(grant: bool) -> (Node, Node, Arc<EchoHandler>) {
    let handler = Arc::new(EchoHandler::default());
    let mut bob = spawn("bob", vec![]);
    let alice_dir = TempDir::new().unwrap();
    let alice_identity = Arc::new(DeviceIdentity::generate(alice_dir.path()).unwrap());
    let mut bob_for_alice = trust(bob.id, "bob", Some(bob.handle.local_addr()));
    bob_for_alice.may_manage = grant;
    let mut alice = start_node(
        "alice",
        alice_identity,
        alice_dir,
        vec![bob_for_alice],
        listen(),
        None,
        Some(handler.clone() as Arc<dyn ControlHandler>),
    );
    bob.handle
        .send(NetCommand::SetPeers(vec![trust(alice.id, "alice", None)]));
    wait_connected(&mut alice.events, bob.id, "bob");
    wait_connected(&mut bob.events, alice.id, "alice");
    (alice, bob, handler)
}

fn remote_call(caller: &Node, peer: DeviceId, call: RemoteCall) -> RemoteResult {
    let (reply, rx) = std::sync::mpsc::channel();
    caller
        .handle
        .send(NetCommand::Control { peer, call, reply });
    rx.recv_timeout(TIMEOUT).expect("control reply")
}

fn wait_grants(events: &mut Events, from: DeviceId, expected: bool) {
    events.wait_match(TIMEOUT, |ev| match ev {
        NetEvent::PeerGrants {
            peer,
            may_manage_you,
        } if *peer == from && *may_manage_you == expected => Some(()),
        _ => None,
    });
}

#[test]
fn granted_peer_can_call_and_learns_the_grant() {
    let (alice, mut bob, handler) = managed_pair(true);
    wait_grants(&mut bob.events, alice.id, true);
    let reply = remote_call(&bob, alice.id, RemoteCall::Roots).expect("roots");
    let RemoteReply::Roots { roots } = reply else {
        panic!("unexpected reply {reply:?}");
    };
    assert_eq!(roots[0].path, bob.id.to_string(), "handler saw the caller");
    assert_eq!(handler.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn call_without_grant_is_refused_before_the_handler() {
    let (alice, bob, handler) = managed_pair(false);
    let err = remote_call(&bob, alice.id, RemoteCall::Roots).unwrap_err();
    assert_eq!(err.code, RemoteErrorCode::Forbidden, "{err}");
    assert_eq!(handler.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn granting_later_is_pushed_to_the_peer() {
    let (alice, mut bob, _) = managed_pair(false);
    let mut bob_for_alice = trust(bob.id, "bob", Some(bob.handle.local_addr()));
    bob_for_alice.may_manage = true;
    alice.handle.send(NetCommand::SetPeers(vec![bob_for_alice]));
    wait_grants(&mut bob.events, alice.id, true);
    assert!(remote_call(&bob, alice.id, RemoteCall::Roots).is_ok());
}

#[test]
fn call_to_a_device_that_is_not_connected_is_offline() {
    let bob = spawn("bob", vec![]);
    let stranger = DeviceId::random();
    let err = remote_call(&bob, stranger, RemoteCall::Roots).unwrap_err();
    assert_eq!(err.code, RemoteErrorCode::Offline);
}

fn read_copy(
    caller: &Node,
    peer: DeviceId,
    path: &std::path::Path,
    max_bytes: u64,
    dest: &std::path::Path,
) -> Result<relay_core::remote::CopiedFile, RemoteError> {
    let (reply, rx) = std::sync::mpsc::channel();
    caller.handle.send(NetCommand::ReadFile {
        peer,
        path: path.to_str().unwrap().to_owned(),
        max_bytes,
        dest: dest.to_path_buf(),
        reply,
    });
    rx.recv_timeout(TIMEOUT).expect("read reply")
}

#[test]
fn read_only_copy_streams_a_file_and_respects_grant_and_size() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("report.docx");
    let bytes: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(&source, &bytes).unwrap();

    let (alice, bob, handler) = managed_pair(true);
    let dest = dir.path().join("copy.docx");
    let copied = read_copy(&bob, alice.id, &source, 1 << 20, &dest).expect("copy");
    assert_eq!(copied.size, bytes.len() as u64);
    assert!(copied.modified_ms.is_some());
    assert_eq!(std::fs::read(&dest).unwrap(), bytes);

    let err = read_copy(&bob, alice.id, &source, 10, &dir.path().join("small")).unwrap_err();
    assert_eq!(err.code, RemoteErrorCode::Invalid, "{err}");
    assert!(!dir.path().join("small").exists());
    assert!(!dir.path().join("small.relay-partial").exists());
    assert_eq!(handler.calls.load(std::sync::atomic::Ordering::SeqCst), 2);

    let (alice, bob, handler) = managed_pair(false);
    let err = read_copy(&bob, alice.id, &source, 1 << 20, &dir.path().join("x")).unwrap_err();
    assert_eq!(err.code, RemoteErrorCode::Forbidden, "{err}");
    assert_eq!(handler.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn speed_test(
    caller: &Node,
    peer: DeviceId,
    duration_ms: u32,
) -> Result<relay_core::speed::SpeedReport, RemoteError> {
    let (reply, rx) = std::sync::mpsc::channel();
    caller.handle.send(NetCommand::SpeedTest {
        peer,
        duration_ms,
        reply,
    });
    rx.recv_timeout(TIMEOUT).expect("speed test reply")
}

#[test]
fn connection_test_measures_both_directions_without_a_grant() {
    let (alice, bob, handler) = managed_pair(false);
    let report = speed_test(&bob, alice.id, 300).expect("speed test");
    assert_eq!(report.path, relay_core::speed::PathKind::Loopback);
    for (name, leg) in [("download", &report.download), ("upload", &report.upload)] {
        assert!(leg.bytes > 0, "{name}: {leg:?}");
        assert!(leg.bits_per_sec > 0, "{name}: {leg:?}");
        assert!(!leg.samples.is_empty(), "{name}: {leg:?}");
        assert!(leg.elapsed_ms <= 2_000, "{name}: {leg:?}");
    }
    assert_eq!(
        report.download.samples.iter().sum::<u64>(),
        report.download.bytes,
        "download samples add up to what arrived"
    );
    assert!(report.sent_packets > 0);
    assert!(report.mtu > 0);
    assert_eq!(handler.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn connection_test_to_a_device_that_is_not_connected_is_offline() {
    let bob = spawn("bob", vec![]);
    let err = speed_test(&bob, DeviceId::random(), 100).unwrap_err();
    assert_eq!(err.code, RemoteErrorCode::Offline);
}
