//! Quinn/TLS transport for Relay peers.
//!
//! [`start`] binds a UDP socket on the calling thread (so bind errors such as
//! "port in use" are returned immediately) and spawns one dedicated OS thread
//! running a multi-thread Tokio runtime. The engine talks to that thread with
//! [`NetCommand`] / [`NetEvent`].
//!
//! Bind failures are returned from [`start`]. If the accept loop dies later,
//! [`NetEvent::ListenFailed`] is emitted.

mod addr;
mod discovery;
mod error;
mod io;
mod pairing;
mod session;
mod tls;

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use relay_core::{DeviceId, ObjectId};
use relay_crypto::DeviceIdentity;
use relay_proto::encode_frame;
use relay_store::ObjectStore;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

pub use error::NetError;
use session::{
    CLOSE_SHUTDOWN, Inner, apply_set_peers, close_code, drive_connection, spawn_dialers,
    spawn_fetch,
};
use tls::{TlsMaterials, install_ring_provider, make_server_config};

/// A peer the local device is willing to talk to.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub id: DeviceId,
    pub name: String,
    /// `"host:port"` strings. DNS names are resolved on every dial attempt.
    pub addresses: Vec<String>,
}

/// Configuration for [`start`].
pub struct NetConfig {
    pub identity: Arc<DeviceIdentity>,
    pub device_name: String,
    /// Address to bind. Default `0.0.0.0:47321` is the caller's job.
    pub listen: SocketAddr,
    /// Trusted set. Only these peers may connect or be dialed.
    pub peers: Vec<PeerConfig>,
    /// [`ObjectStore`] root, used to serve and receive objects.
    pub store_root: PathBuf,
}

/// Notifications delivered on the network thread via the callback passed to [`start`].
#[derive(Debug, Clone)]
pub enum NetEvent {
    PeerConnected {
        peer: DeviceId,
        name: String,
        address: SocketAddr,
    },
    PeerDisconnected {
        peer: DeviceId,
        reason: String,
    },
    /// Any control-stream frame other than Hello/Ping/Pong (those are handled internally).
    Frame {
        peer: DeviceId,
        body: relay_proto::frame::Body,
    },
    /// Object is now present and hash-verified in the store.
    ObjectFetched {
        peer: DeviceId,
        object: ObjectId,
    },
    ObjectFetchFailed {
        peer: DeviceId,
        object: ObjectId,
        reason: String,
        not_found: bool,
    },
    /// Accept loop failed after a successful [`start`]. Bind errors are returned from `start`.
    ListenFailed {
        error: String,
    },
    Paired {
        peer: DeviceId,
        name: String,
        addresses: Vec<String>,
        initiator: bool,
    },
    PairFailed {
        reason: String,
    },
    /// Merged address list for an already-trusted peer (LAN discovery).
    PeerAddresses {
        peer: DeviceId,
        addresses: Vec<String>,
    },
}

/// Commands sent from the engine (or tests) into the network thread.
#[derive(Debug)]
pub enum NetCommand {
    /// Dropped with a debug log if the peer is not connected.
    Send {
        peer: DeviceId,
        body: relay_proto::frame::Body,
    },
    FetchObject {
        peer: DeviceId,
        object: ObjectId,
    },
    /// Replace the trusted set live; disconnect removed peers.
    SetPeers(Vec<PeerConfig>),
    /// Begin listening for one pairing attempt (initiator).
    PairStart {
        code: String,
        expires_at: SystemTime,
    },
    /// Dial the initiator (joiner). Without `addr`, use the mDNS nameplate.
    PairJoin {
        code: String,
        addr: Option<String>,
    },
    PairCancel,
    Shutdown,
}

/// Handle to a running network runtime. `send` never blocks and never panics
/// after shutdown. [`Drop`] signals shutdown and joins the runtime thread
/// (expected to be brief after the endpoint closes).
/// Cloneable sink for [`NetCommand`]s (used by the host IPC thread).
#[derive(Clone)]
pub struct NetSender {
    tx: UnboundedSender<NetCommand>,
}

impl NetSender {
    pub fn send(&self, cmd: NetCommand) {
        if self.tx.send(cmd).is_err() {
            tracing::debug!("net command dropped: runtime stopped");
        }
    }
}

pub struct NetHandle {
    cmd_tx: UnboundedSender<NetCommand>,
    local_addr: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl NetHandle {
    /// Queue a command. Never blocks. After shutdown the command is dropped.
    pub fn send(&self, cmd: NetCommand) {
        self.sender().send(cmd);
    }

    pub fn sender(&self) -> NetSender {
        NetSender {
            tx: self.cmd_tx.clone(),
        }
    }

    /// Actual bound address (useful when the caller passed port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Close connections, wait briefly for idle, and join the runtime thread.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.cmd_tx.send(NetCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for NetHandle {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// Bind `config.listen`, spawn the network thread, and return a handle.
///
/// Bind errors (including port-in-use) are returned here. TLS/identity/store
/// setup errors are also returned from this function.
pub fn start(
    config: NetConfig,
    sink: Box<dyn Fn(NetEvent) + Send + Sync>,
) -> Result<NetHandle, NetError> {
    install_ring_provider();

    let socket = UdpSocket::bind(config.listen).map_err(|source| NetError::Bind {
        addr: config.listen,
        source,
    })?;
    let _ = socket.set_nonblocking(true);
    let local_addr = socket.local_addr()?;

    let store = ObjectStore::open(&config.store_root)?;
    let tls = TlsMaterials::from_identity(&config.identity)?;
    let trusted = Arc::new(RwLock::new(
        config.peers.into_iter().map(|p| (p.id, p)).collect(),
    ));
    let server = make_server_config(&tls)?;

    let inner = Arc::new(Inner {
        our_id: config.identity.device_id(),
        device_name: config.device_name,
        store,
        trusted,
        sessions: Mutex::new(HashMap::new()),
        session_notify: Notify::new(),
        sink: Arc::from(sink),
        shutdown: Notify::new(),
        shutting_down: AtomicBool::new(false),
        tls,
        identity: config.identity,
        listen_port: local_addr.port(),
        pairing: Mutex::new(None),
        pairing_ads: Mutex::new(HashMap::new()),
        discovery: Mutex::new(None),
    });

    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let thread = std::thread::Builder::new()
        .name("relay-net".into())
        .spawn({
            let inner = inner.clone();
            move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("relay-net-worker")
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(NetError::Runtime(e.to_string())));
                        return;
                    }
                };
                rt.block_on(async move {
                    // Endpoint construction needs a Tokio reactor (Quinn's TokioRuntime).
                    let endpoint = match quinn::Endpoint::new(
                        quinn::EndpointConfig::default(),
                        Some(server),
                        socket,
                        Arc::new(quinn::TokioRuntime),
                    ) {
                        Ok(ep) => {
                            let _ = ready_tx.send(Ok(()));
                            ep
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(NetError::from(e)));
                            return;
                        }
                    };
                    run(inner, endpoint, cmd_rx).await;
                });
            }
        })
        .map_err(|e| NetError::Runtime(e.to_string()))?;

    ready_rx
        .recv()
        .map_err(|_| NetError::Runtime("net thread exited during startup".into()))??;

    Ok(NetHandle {
        cmd_tx,
        local_addr,
        thread: Some(thread),
    })
}

async fn run(
    inner: Arc<Inner>,
    endpoint: quinn::Endpoint,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<NetCommand>,
) {
    let mut dialers = HashMap::new();
    spawn_dialers(&inner, &endpoint, &mut dialers);
    if let Some(discovery) = discovery::start(&inner, inner.listen_port) {
        *inner.discovery.lock().unwrap_or_else(|e| e.into_inner()) = Some(discovery);
    }

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    None | Some(NetCommand::Shutdown) => break,
                    Some(NetCommand::Send { peer, body }) => handle_send(&inner, peer, body),
                    Some(NetCommand::FetchObject { peer, object }) => {
                        spawn_fetch(inner.clone(), peer, object);
                    }
                    Some(NetCommand::SetPeers(peers)) => {
                        apply_set_peers(&inner, peers);
                        spawn_dialers(&inner, &endpoint, &mut dialers);
                    }
                    Some(NetCommand::PairStart { code, expires_at }) => {
                        pairing::start_session(&inner, code, expires_at);
                    }
                    Some(NetCommand::PairJoin { code, addr }) => {
                        tokio::spawn(pairing::join(inner.clone(), endpoint.clone(), code, addr));
                    }
                    Some(NetCommand::PairCancel) => pairing::cancel_session(&inner),
                }
            }
            incoming = endpoint.accept() => {
                match incoming {
                    None => {
                        if !inner.is_shutting_down() {
                            inner.emit(NetEvent::ListenFailed {
                                error: "endpoint closed".into(),
                            });
                        }
                        break;
                    }
                    Some(incoming) => {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            match incoming.await {
                                Ok(conn) => handle_incoming(inner, conn).await,
                                Err(e) => {
                                    tracing::warn!(error = %e, "incoming handshake failed");
                                }
                            }
                        });
                    }
                }
            }
        }
    }

    inner
        .shutting_down
        .store(true, std::sync::atomic::Ordering::Relaxed);
    inner.shutdown.notify_waiters();
    if let Some(discovery) = inner
        .discovery
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        discovery.shutdown();
    }
    for (_, handle) in dialers.drain() {
        handle.abort();
    }
    endpoint.close(close_code(CLOSE_SHUTDOWN), b"shutdown");
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.wait_idle()).await;
}

async fn handle_incoming(inner: Arc<Inner>, conn: quinn::Connection) {
    let alpn = pairing::negotiated_alpn(&conn);
    if alpn.as_deref() == Some(relay_proto::PAIR_ALPN) {
        pairing::accept_incoming(inner, conn).await;
        return;
    }
    drive_connection(inner, conn, false).await;
}

fn handle_send(inner: &Inner, peer: DeviceId, body: relay_proto::frame::Body) {
    match inner.established_session(peer) {
        Some((_, write_tx, _, _)) => match encode_frame(&relay_proto::Frame::new(body)) {
            Ok(bytes) => {
                if write_tx.send(bytes).is_err() {
                    tracing::debug!(peer = %peer, "control writer gone");
                }
            }
            Err(e) => tracing::debug!(peer = %peer, error = %e, "encode failed"),
        },
        None => tracing::debug!(peer = %peer, "send dropped: peer not connected"),
    }
}
