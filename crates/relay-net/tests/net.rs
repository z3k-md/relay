use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rand::RngCore;
use relay_core::{DeviceId, ObjectId};
use relay_crypto::DeviceIdentity;
use relay_net::{NetCommand, NetConfig, NetEvent, NetHandle, PeerConfig, start};
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

    fn wait_match<T>(&mut self, timeout: Duration, mut f: impl FnMut(&NetEvent) -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            while self.next < self.got.len() {
                let ev = &self.got[self.next];
                self.next += 1;
                if let Some(t) = f(ev) {
                    return t;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("timeout waiting for event; have {:#?}", self.got);
            }
            match self.rx.recv_timeout(left) {
                Ok(ev) => self.got.push(ev),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timeout waiting for event; have {:#?}", self.got)
                }
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
    start_from_identity(name, identity, identity_dir, peers, listen())
}

fn trust(id: DeviceId, name: &str, addr: Option<SocketAddr>) -> PeerConfig {
    PeerConfig {
        id,
        name: name.to_owned(),
        addresses: addr.map(|a| a.to_string()).into_iter().collect(),
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

    send_ack(&alice.handle, bob_id, 1);
    send_ack(&bob.handle, alice_id, 2);
    wait_ack(&mut bob.events, alice_id, 1);
    wait_ack(&mut alice.events, bob_id, 2);

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

    send_ack(&alice.handle, bob_id, 3);
    send_ack(&bob.handle, alice_id, 4);
    wait_ack(&mut bob.events, alice_id, 3);
    wait_ack(&mut alice.events, bob_id, 4);
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
fn reconnects_after_peer_restart() {
    let bob_id_dir = TempDir::new().unwrap();
    let identity = Arc::new(DeviceIdentity::generate(bob_id_dir.path()).unwrap());
    let bob_id = identity.device_id();
    let mut bob = start_from_identity("bob", identity, bob_id_dir, vec![], listen());
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
    );

    wait_connected(&mut alice.events, bob_id, "bob");
    wait_connected(&mut bob.events, alice_id, "alice");
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
