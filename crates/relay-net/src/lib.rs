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
mod control;
mod discovery;
mod error;
mod io;
mod pairing;
mod relay;
mod session;
mod stun;
mod tls;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use quinn::AsyncUdpSocket;
use relay_core::remote::{CopiedFile, RemoteCall, RemoteError, RemoteResult};
use relay_core::{DeviceId, ObjectId};
use relay_crypto::DeviceIdentity;
use relay_proto::encode_frame;
use relay_store::ObjectStore;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

pub use addr::advertised_addresses;
pub use control::ControlHandler;
pub use error::NetError;
pub use relay::{RelayServer, serve_relay};
use session::{
    CLOSE_SHUTDOWN, Inner, apply_set_peers, close_code, drive_connection, spawn_dialers,
    spawn_fetch,
};
use tls::{TlsMaterials, install_ring_provider, make_server_config};

/// Upper bound for Quinn `wait_idle` during shutdown. The drain itself can take
/// multiple seconds; waiting that long blocked desktop quit via `stop_join`.
const SHUTDOWN_IDLE_WAIT: Duration = Duration::from_millis(150);

/// A peer the local device is willing to talk to.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub id: DeviceId,
    pub name: String,
    /// `"host:port"` strings. DNS names are resolved on every dial attempt.
    pub addresses: Vec<String>,
    /// This peer may manage this device (D37). Checked before any remote
    /// call reaches the [`ControlHandler`].
    pub may_manage: bool,
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
    /// When set, query a public STUN server on the listen socket before Quinn
    /// takes it, and remember the reflexive address for NAT hole punching.
    pub enable_stun: bool,
    /// `host:port` peers dial after every direct address fails. `None` disables relay dial.
    pub relay: Option<String>,
    /// Bind `0.0.0.0:<port>` and serve. The port comes from [`Self::relay`].
    pub serve_relay: bool,
    /// Answers remote calls. `None` refuses them as unsupported.
    pub control: Option<Arc<dyn ControlHandler>>,
}

/// Notifications delivered on the network thread via the callback passed to [`start`].
#[derive(Debug, Clone)]
pub enum NetEvent {
    PeerConnected {
        peer: DeviceId,
        name: String,
        address: SocketAddr,
        /// The peer answers remote calls (`FEATURE_CONTROL`).
        supports_control: bool,
    },
    /// The peer says whether this device may manage it.
    PeerGrants {
        peer: DeviceId,
        may_manage_you: bool,
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
    /// Bytes of one object received (`incoming`) or served so far.
    ObjectProgress {
        peer: DeviceId,
        object: ObjectId,
        incoming: bool,
        bytes: u64,
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
    /// Replace the relay address used for dialing. Does not restart the endpoint.
    SetRelay(Option<String>),
    /// Make a remote call on a peer (D37). The answer, or why there is
    /// none, arrives on `reply`.
    Control {
        peer: DeviceId,
        call: RemoteCall,
        reply: std::sync::mpsc::Sender<RemoteResult>,
    },
    /// Copy one file from a peer into `dest`, which must not exist (D41).
    ReadFile {
        peer: DeviceId,
        path: String,
        max_bytes: u64,
        dest: PathBuf,
        reply: std::sync::mpsc::Sender<Result<CopiedFile, RemoteError>>,
    },
    Shutdown,
}

/// Handle to a running network runtime. `send` never blocks and never panics
/// after shutdown. [`Drop`] signals shutdown and joins the runtime thread
/// (endpoint close with a short idle bound, not the full QUIC drain).
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
    /// Public address learned from STUN, if `enable_stun` succeeded.
    reflexive: Option<SocketAddr>,
    relay: Option<RelayServer>,
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

    /// Reflexive UDP address from STUN, when discovery ran and a server answered.
    pub fn reflexive(&self) -> Option<SocketAddr> {
        self.reflexive
    }

    /// Close connections and join the runtime thread.
    ///
    /// Caps Quinn's optional `wait_idle` drain: the close timer routinely takes
    /// multiple seconds, which made the desktop app hang on quit. A short bound
    /// is enough to flush CONNECTION_CLOSE without blocking the UI.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.cmd_tx.send(NetCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.relay.take();
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
    let local_addr = socket.local_addr()?;
    // STUN has to run before the socket is nonblocking and owned by Quinn.
    // The mapped port matches this socket, so a peer dialing it punches the NAT.
    let reflexive = if config.enable_stun {
        stun::discover_public(&socket, Duration::from_millis(400))
    } else {
        None
    };
    let _ = socket.set_nonblocking(true);

    let relay_server = relay_server_for(config.serve_relay, config.relay.as_deref())?;
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
        lan_discovery: !local_addr.ip().is_loopback(),
        relay_target: Mutex::new(config.relay),
        relay_sock: OnceLock::new(),
        pairing: Mutex::new(None),
        pairing_ads: Mutex::new(HashMap::new()),
        discovery: Mutex::new(None),
        control: config.control,
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
                    let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
                    let wrapped = match runtime.wrap_udp_socket(socket) {
                        Ok(sock) => sock,
                        Err(err) => {
                            let _ = ready_tx.send(Err(NetError::from(err)));
                            return;
                        }
                    };
                    let relay_sock = Arc::new(relay::RelaySocket::new(
                        wrapped,
                        inner.our_id,
                        Arc::clone(&inner.trusted),
                    ));
                    if inner.relay_sock.set(Arc::clone(&relay_sock)).is_err() {
                        let _ = ready_tx.send(Err(NetError::Runtime(
                            "relay socket already installed".into(),
                        )));
                        return;
                    }
                    let mut endpoint_config = quinn::EndpointConfig::default();
                    endpoint_config
                        .max_udp_payload_size(relay::ENDPOINT_MAX_UDP_PAYLOAD)
                        .expect("1400 is within Quinn's UDP payload bounds");
                    let endpoint = match quinn::Endpoint::new_with_abstract_socket(
                        endpoint_config,
                        Some(server),
                        relay_sock,
                        runtime,
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
        reflexive,
        relay: relay_server,
        thread: Some(thread),
    })
}

/// Resolve the configured relay and install it on the socket. DNS can take
/// seconds on a broken resolver, so this runs as its own task; the command
/// and accept loop never waits on it.
fn spawn_relay_resolve(inner: &Arc<Inner>) {
    let target = inner
        .relay_target
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let Some(sock) = inner.relay_sock.get().cloned() else {
        return;
    };
    let Some(target) = target else {
        sock.set_relay(None);
        return;
    };
    let inner = inner.clone();
    tokio::spawn(async move {
        let prefer_ipv4 = sock.local_addr().ok().map(|bound| bound.is_ipv4());
        let resolved = relay::resolve_relay(&target, prefer_ipv4).await;
        // A newer `SetRelay` may have replaced the target meanwhile.
        let current = inner
            .relay_target
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        if current.as_deref() == Some(target.as_str()) {
            sock.set_relay(resolved);
        }
    });
}

fn relay_server_for(serve: bool, relay: Option<&str>) -> Result<Option<RelayServer>, NetError> {
    if !serve {
        return Ok(None);
    }
    let Some(target) = relay else {
        tracing::error!("serve_relay is set but no relay address is configured");
        return Ok(None);
    };
    let Some(port) = relay::relay_port(target) else {
        tracing::error!(addr = %target, "serve_relay is set but the relay address has no port");
        return Ok(None);
    };
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let server = serve_relay(addr)?;
    tracing::info!(%addr, "serving udp relay");
    Ok(Some(server))
}

async fn run(
    inner: Arc<Inner>,
    endpoint: quinn::Endpoint,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<NetCommand>,
) {
    spawn_relay_resolve(&inner);
    let mut dialers = HashMap::new();
    spawn_dialers(&inner, &endpoint, &mut dialers);
    if inner.lan_discovery
        && let Some(discovery) = discovery::start(&inner, inner.listen_port)
    {
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
                    Some(NetCommand::ReadFile { peer, path, max_bytes, dest, reply }) => {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            let copied = control::read_file(inner, peer, path, max_bytes, dest).await;
                            let _ = reply.send(copied);
                        });
                    }
                    Some(NetCommand::Control { peer, call, reply }) => {
                        let inner = inner.clone();
                        tokio::spawn(async move {
                            let _ = reply.send(control::call(inner, peer, call).await);
                        });
                    }
                    Some(NetCommand::SetRelay(addr)) => {
                        *inner
                            .relay_target
                            .lock()
                            .unwrap_or_else(|err| err.into_inner()) = addr;
                        spawn_relay_resolve(&inner);
                    }
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
    // Quinn's close/drain timer often runs for seconds; keep this brief so app
    // quit (which joins this thread) returns promptly while still giving
    // CONNECTION_CLOSE a moment to leave the socket.
    let _ = tokio::time::timeout(SHUTDOWN_IDLE_WAIT, endpoint.wait_idle()).await;
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
