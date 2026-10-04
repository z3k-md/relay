use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::discovery::{Discovery, PairingAd};
use crate::pairing::PairSession;

use quinn::{AsyncUdpSocket, Connection, RecvStream, SendStream, VarInt};
use relay_core::{DeviceId, ObjectId, rank_addresses};
use relay_crypto::{DeviceIdentity, device_id_from_certificate};
use relay_proto::frame::Body;
use relay_proto::{
    ErrorFrame, FEATURE_CONTROL, Frame, Hello, ObjectHeader, ObjectRequest, PROTOCOL_VERSION, Ping,
    device_id_from_bytes, encode_frame, object_id_from_bytes,
};
use relay_store::ObjectStore;
use rustls::pki_types::CertificateDer;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore};

use crate::control::{self, ControlHandler};
use crate::io::{IoErr, read_message};
use crate::relay::{RelaySocket, resolve_relay, virtual_peer_addr};
use crate::tls::{SERVER_NAME, TlsMaterials, make_client_config};
use crate::{NetEvent, PeerConfig};

pub(crate) const CLOSE_DUPLICATE: u32 = 1;
pub(crate) const CLOSE_PROTOCOL: u32 = 2;
pub(crate) const CLOSE_MALFORMED: u32 = 3;
pub(crate) const CLOSE_SHUTDOWN: u32 = 4;
pub(crate) const CLOSE_UNTRUSTED: u32 = 5;

const MAX_CONCURRENT_FETCHES: usize = 8;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const CHUNK: usize = 64 * 1024;
const PING_INTERVAL: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DIAL_ATTEMPT: Duration = Duration::from_secs(2);
const RELAY_DIAL: Duration = Duration::from_secs(5);

pub(crate) fn close_code(code: u32) -> VarInt {
    VarInt::from_u32(code)
}

pub(crate) struct Inner {
    pub our_id: DeviceId,
    pub device_name: String,
    pub store: ObjectStore,
    pub trusted: Arc<RwLock<HashMap<DeviceId, PeerConfig>>>,
    pub sessions: Mutex<HashMap<DeviceId, LiveSession>>,
    pub session_notify: Notify,
    pub sink: Arc<dyn Fn(NetEvent) + Send + Sync>,
    pub shutdown: Notify,
    pub shutting_down: AtomicBool,
    pub tls: TlsMaterials,
    /// Signs relay BIND frames. Also keeps the key alive for the runtime thread.
    pub identity: Arc<DeviceIdentity>,
    pub listen_port: u16,
    /// `host:port` to dial after every direct address fails. `None` disables it.
    pub relay_target: Mutex<Option<String>>,
    pub relay_sock: OnceLock<Arc<RelaySocket>>,
    pub pairing: Mutex<Option<PairSession>>,
    pub pairing_ads: Mutex<HashMap<String, PairingAd>>,
    pub discovery: Mutex<Option<Discovery>>,
    /// Answers remote calls from peers with the manage grant (D37).
    pub control: Option<Arc<dyn ControlHandler>>,
}

type EstablishedSession = (
    Connection,
    UnboundedSender<Vec<u8>>,
    Arc<Semaphore>,
    Arc<Mutex<HashSet<ObjectId>>>,
);

pub(crate) struct LiveSession {
    pub conn: Connection,
    pub stable_id: usize,
    pub dialed_by: DeviceId,
    pub write_tx: Option<UnboundedSender<Vec<u8>>>,
    pub fetch_sem: Arc<Semaphore>,
    pub in_flight: Arc<Mutex<HashSet<ObjectId>>>,
    pub established: bool,
    pub name: String,
    /// `FEATURE_*` bits from the peer's `Hello`.
    pub features: u64,
    /// The peer lets this device manage it (its last `PeerGrants`).
    pub grants_us: bool,
    /// Remote calls this peer may have running here at once.
    pub control_slots: Arc<Semaphore>,
}

impl LiveSession {
    fn new(conn: &Connection, dialed_by: DeviceId) -> Self {
        Self {
            conn: conn.clone(),
            stable_id: conn.stable_id(),
            dialed_by,
            write_tx: None,
            fetch_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            established: false,
            name: String::new(),
            features: 0,
            grants_us: false,
            control_slots: Arc::new(Semaphore::new(control::MAX_CONCURRENT_CALLS)),
        }
    }
}

impl Inner {
    pub(crate) fn emit(&self, ev: NetEvent) {
        (self.sink)(ev);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    pub(crate) fn is_trusted(&self, id: &DeviceId) -> bool {
        self.trusted
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    /// Whether `peer` may manage this device, per the trusted set.
    pub(crate) fn may_manage(&self, peer: DeviceId) -> bool {
        self.trusted
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&peer)
            .is_some_and(|p| p.may_manage)
    }

    fn with_established<T>(&self, peer: DeviceId, f: impl FnOnce(&LiveSession) -> T) -> Option<T> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(&peer).filter(|s| s.established).map(f)
    }

    pub(crate) fn control_connection(&self, peer: DeviceId) -> Option<Connection> {
        self.with_established(peer, |s| s.conn.clone())
    }

    pub(crate) fn peer_has_feature(&self, peer: DeviceId, feature: u64) -> bool {
        self.with_established(peer, |s| s.features & feature != 0)
            .unwrap_or(false)
    }

    pub(crate) fn control_slots(&self, peer: DeviceId) -> Option<Arc<Semaphore>> {
        self.with_established(peer, |s| s.control_slots.clone())
    }

    fn note_grants(&self, peer: DeviceId, may_manage_you: bool) {
        let changed = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get_mut(&peer) {
                Some(s) if s.grants_us != may_manage_you => {
                    s.grants_us = may_manage_you;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.emit(NetEvent::PeerGrants {
                peer,
                may_manage_you,
            });
        }
    }

    /// Tell every connected peer that understands grants whether it may
    /// manage this device. Called after the trusted set changes.
    fn send_grants(&self) {
        let targets: Vec<(DeviceId, UnboundedSender<Vec<u8>>)> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .iter()
                .filter(|(_, s)| s.established && s.features & FEATURE_CONTROL != 0)
                .filter_map(|(id, s)| Some((*id, s.write_tx.clone()?)))
                .collect()
        };
        for (peer, write_tx) in targets {
            if let Some(bytes) = control::grants_frame(self.may_manage(peer)) {
                let _ = write_tx.send(bytes);
            }
        }
    }

    pub(crate) fn has_session(&self, peer: DeviceId) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&peer)
    }

    pub(crate) fn established_session(&self, peer: DeviceId) -> Option<EstablishedSession> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let s = sessions.get(&peer)?;
        if !s.established {
            return None;
        }
        Some((
            s.conn.clone(),
            s.write_tx.clone()?,
            s.fetch_sem.clone(),
            s.in_flight.clone(),
        ))
    }

    /// Keep the connection dialed by the lower device id. A new connection from
    /// the same dialer as the existing one replaces it: a device only dials
    /// when it has no connection, so the old one is stale (the peer restarted
    /// or its network changed) and would otherwise block reconnection until
    /// the idle timeout. Returns whether `conn` is kept.
    pub(crate) fn claim(&self, peer_id: DeviceId, conn: &Connection, we_dialed: bool) -> bool {
        let dialed_by = if we_dialed { self.our_id } else { peer_id };
        let preferred = std::cmp::min(self.our_id, peer_id);
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match sessions.get(&peer_id) {
            None => {
                sessions.insert(peer_id, LiveSession::new(conn, dialed_by));
                true
            }
            Some(existing) => {
                let replaces = existing.dialed_by == dialed_by
                    || (dialed_by == preferred && existing.dialed_by != preferred);
                if replaces {
                    let old = sessions.remove(&peer_id).expect("just looked up");
                    old.conn
                        .close(close_code(CLOSE_DUPLICATE), b"duplicate connection");
                    let was_established = old.established;
                    sessions.insert(peer_id, LiveSession::new(conn, dialed_by));
                    drop(sessions);
                    if was_established {
                        tracing::info!(
                            peer = %peer_id,
                            "superseding existing connection"
                        );
                        self.emit(NetEvent::PeerDisconnected {
                            peer: peer_id,
                            reason: "superseded by a newer or preferred connection".into(),
                        });
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    fn attach(
        &self,
        peer: DeviceId,
        stable_id: usize,
        write_tx: UnboundedSender<Vec<u8>>,
        name: String,
        features: u64,
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match sessions.get_mut(&peer) {
            Some(s) if s.stable_id == stable_id => {
                s.write_tx = Some(write_tx);
                s.established = true;
                s.name = name;
                s.features = features;
                true
            }
            _ => false,
        }
    }

    fn release(&self, peer: DeviceId, stable_id: usize, reason: String) {
        let established = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            match sessions.get(&peer) {
                Some(s) if s.stable_id == stable_id => {
                    let est = s.established;
                    sessions.remove(&peer);
                    est
                }
                _ => false,
            }
        };
        self.session_notify.notify_waiters();
        if established {
            tracing::info!(peer = %peer, %reason, "peer disconnected");
            self.emit(NetEvent::PeerDisconnected { peer, reason });
        }
    }

    pub(crate) fn refresh_pair_txt(&self, nameplate: Option<&str>) {
        let discovery = self.discovery.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(discovery) = discovery.as_ref() {
            discovery.set_pair_nameplate(nameplate, self, self.listen_port);
        }
    }

    pub(crate) fn close_peer(&self, id: DeviceId, reason: &str) {
        let conn = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.get(&id).map(|s| s.conn.clone())
        };
        if let Some(conn) = conn {
            conn.close(close_code(CLOSE_UNTRUSTED), reason.as_bytes());
        }
    }
}

pub(crate) async fn wait_shutdown(inner: &Inner) {
    loop {
        if inner.is_shutting_down() {
            return;
        }
        inner.shutdown.notified().await;
    }
}

async fn wait_until_free(inner: &Inner, peer: DeviceId) {
    loop {
        if !inner.has_session(peer) || inner.is_shutting_down() {
            return;
        }
        tokio::select! {
            _ = inner.session_notify.notified() => {}
            _ = wait_shutdown(inner) => return,
        }
    }
}

pub(crate) fn peer_device_id(conn: &Connection) -> Result<DeviceId, String> {
    let ident = conn
        .peer_identity()
        .ok_or_else(|| "no peer identity after handshake".to_owned())?;
    let certs = ident
        .downcast_ref::<Vec<CertificateDer<'static>>>()
        .ok_or_else(|| "peer identity is not a rustls certificate chain".to_owned())?;
    let cert = certs
        .first()
        .ok_or_else(|| "empty peer certificate chain".to_owned())?;
    device_id_from_certificate(cert.as_ref()).map_err(|e| e.to_string())
}

pub(crate) async fn drive_connection(inner: Arc<Inner>, conn: Connection, we_dialed: bool) {
    let peer_id = match peer_device_id(&conn) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "could not derive peer id from TLS");
            conn.close(close_code(CLOSE_PROTOCOL), b"no peer identity");
            return;
        }
    };

    // Trust is enforced here, after ALPN is known. The TLS verifier accepts
    // any Relay device cert so pairing can complete; sync data must never
    // flow to an untrusted peer. Checked before any stream is accepted.
    if !inner.is_trusted(&peer_id) {
        tracing::warn!(peer = %peer_id, "peer is not in the trusted set after handshake");
        conn.close(close_code(CLOSE_UNTRUSTED), b"untrusted peer");
        return;
    }

    if !inner.claim(peer_id, &conn, we_dialed) {
        tracing::debug!(
            peer = %peer_id,
            we_dialed,
            "closing duplicate connection (kept the one dialed by the lower device id)"
        );
        conn.close(close_code(CLOSE_DUPLICATE), b"duplicate connection");
        return;
    }

    // Release on every exit, including task abort (SetPeers drops the dialer).
    let mut guard = SessionGuard {
        inner: inner.clone(),
        peer: peer_id,
        stable_id: conn.stable_id(),
        reason: "closed".to_owned(),
    };
    if let Err(e) = run_session(inner, conn, peer_id, we_dialed).await {
        guard.reason = e;
    }
}

struct SessionGuard {
    inner: Arc<Inner>,
    peer: DeviceId,
    stable_id: usize,
    reason: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.inner
            .release(self.peer, self.stable_id, std::mem::take(&mut self.reason));
    }
}

async fn run_session(
    inner: Arc<Inner>,
    conn: Connection,
    peer_id: DeviceId,
    we_dialed: bool,
) -> Result<(), String> {
    let (mut send, mut recv) = if we_dialed {
        conn.open_bi().await.map_err(|e| e.to_string())?
    } else {
        conn.accept_bi().await.map_err(|e| e.to_string())?
    };

    let hello = Frame::new(Body::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: inner.our_id.as_bytes().to_vec(),
        device_name: inner.device_name.clone(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        features: FEATURE_CONTROL,
    }));
    let hello_bytes = encode_frame(&hello).map_err(|e| e.to_string())?;

    let (write_res, read_res) = tokio::join!(
        send.write_all(&hello_bytes),
        read_message::<Frame>(&mut recv)
    );
    write_res.map_err(|e| e.to_string())?;
    let their = match read_res {
        Ok(frame) => frame,
        Err(e) => {
            conn.close(close_code(CLOSE_MALFORMED), b"hello read failed");
            return Err(e.to_string());
        }
    };

    let Some(Body::Hello(h)) = their.body else {
        conn.close(close_code(CLOSE_PROTOCOL), b"expected hello");
        return Err("first control frame was not Hello".into());
    };

    if h.protocol_version != PROTOCOL_VERSION {
        let err = Frame::new(Body::Error(ErrorFrame {
            code: "protocol_version".into(),
            message: format!(
                "expected protocol {PROTOCOL_VERSION}, got {}",
                h.protocol_version
            ),
        }));
        if let Ok(bytes) = encode_frame(&err) {
            let _ = send.write_all(&bytes).await;
        }
        conn.close(close_code(CLOSE_PROTOCOL), b"protocol version");
        return Err(format!("protocol version mismatch: {}", h.protocol_version));
    }

    match device_id_from_bytes(&h.device_id) {
        Ok(claimed) if claimed == peer_id => {}
        Ok(claimed) => {
            tracing::warn!(
                peer = %peer_id,
                claimed = %claimed,
                "hello device_id does not match TLS identity"
            );
            conn.close(close_code(CLOSE_PROTOCOL), b"hello device_id mismatch");
            return Err("hello device_id mismatch".into());
        }
        Err(e) => {
            conn.close(close_code(CLOSE_PROTOCOL), b"hello device_id invalid");
            return Err(e.to_string());
        }
    }

    let peer_name = h.device_name.clone();
    let remote = conn.remote_address();
    let (write_tx, write_rx) = tokio::sync::mpsc::unbounded_channel();
    // A connection superseded during the handshake was never announced, so it
    // must not announce itself now; claim() already closed it.
    if !inner.attach(
        peer_id,
        conn.stable_id(),
        write_tx.clone(),
        peer_name.clone(),
        h.features,
    ) {
        return Err("superseded during handshake".into());
    }
    let supports_control = h.features & FEATURE_CONTROL != 0;
    if supports_control && let Some(bytes) = control::grants_frame(inner.may_manage(peer_id)) {
        let _ = write_tx.send(bytes);
    }

    tracing::info!(peer = %peer_id, name = %peer_name, addr = %remote, "peer connected");
    inner.emit(NetEvent::PeerConnected {
        peer: peer_id,
        name: peer_name,
        address: remote,
        supports_control,
    });

    let writer = tokio::spawn(write_loop(send, write_rx));
    let reader = tokio::spawn(read_loop(
        recv,
        write_tx.clone(),
        conn.clone(),
        peer_id,
        inner.clone(),
    ));
    let incoming = tokio::spawn(incoming_objects(conn.clone(), inner.clone(), peer_id));
    let ping = tokio::spawn(ping_loop(write_tx, inner.clone()));

    let outcome = tokio::select! {
        _ = conn.closed() => Ok(()),
        _ = wait_shutdown(&inner) => {
            conn.close(close_code(CLOSE_SHUTDOWN), b"shutdown");
            Ok(())
        }
        r = reader => match r {
            Ok(()) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    };

    writer.abort();
    incoming.abort();
    ping.abort();
    outcome
}

async fn write_loop(mut send: SendStream, mut rx: UnboundedReceiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if send.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

async fn read_loop(
    mut recv: RecvStream,
    write_tx: UnboundedSender<Vec<u8>>,
    conn: Connection,
    peer: DeviceId,
    inner: Arc<Inner>,
) {
    loop {
        match read_message::<Frame>(&mut recv).await {
            Ok(frame) => match frame.body {
                Some(Body::Hello(_)) => {
                    tracing::debug!(peer = %peer, "ignoring extra hello");
                }
                Some(Body::Ping(p)) => {
                    if let Ok(bytes) = encode_frame(&Frame::new(Body::Pong(p))) {
                        let _ = write_tx.send(bytes);
                    }
                }
                Some(Body::Pong(_)) => {}
                Some(Body::PeerGrants(grants)) => inner.note_grants(peer, grants.may_manage_you),
                Some(body) => inner.emit(NetEvent::Frame { peer, body }),
                None => {
                    tracing::warn!(peer = %peer, "empty control frame");
                    send_error_and_close(&write_tx, &conn, "malformed", "empty frame");
                    return;
                }
            },
            Err(e) if e.is_malformed() => {
                tracing::warn!(peer = %peer, error = %e, "malformed control frame");
                send_error_and_close(&write_tx, &conn, "malformed", &e.to_string());
                return;
            }
            Err(IoErr::UnexpectedEnd) | Err(IoErr::Read(_)) => return,
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "control stream ended");
                return;
            }
        }
    }
}

fn send_error_and_close(
    write_tx: &UnboundedSender<Vec<u8>>,
    conn: &Connection,
    code: &str,
    message: &str,
) {
    if let Ok(bytes) = encode_frame(&Frame::new(Body::Error(ErrorFrame {
        code: code.to_owned(),
        message: message.to_owned(),
    }))) {
        let _ = write_tx.send(bytes);
    }
    conn.close(close_code(CLOSE_MALFORMED), message.as_bytes());
}

async fn ping_loop(write_tx: UnboundedSender<Vec<u8>>, inner: Arc<Inner>) {
    let mut nonce = 0u64;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(PING_INTERVAL) => {
                nonce = nonce.wrapping_add(1);
                if let Ok(bytes) = encode_frame(&Frame::new(Body::Ping(Ping { nonce })))
                    && write_tx.send(bytes).is_err()
                {
                    return;
                }
            }
            _ = wait_shutdown(&inner) => return,
        }
    }
}

async fn incoming_objects(conn: Connection, inner: Arc<Inner>, peer: DeviceId) {
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let store = inner.store.clone();
                let conn = conn.clone();
                let progress = Arc::clone(&inner);
                tokio::spawn(async move {
                    if let Err(e) = serve_object(send, recv, &store, &conn, &progress, peer).await {
                        tracing::debug!(peer = %peer, error = %e, "object serve failed");
                    }
                });
            }
            Err(_) => return,
        }
    }
}

#[derive(Debug)]
struct ServeErr {
    message: String,
}

impl std::fmt::Display for ServeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

async fn serve_object(
    mut send: SendStream,
    mut recv: RecvStream,
    store: &ObjectStore,
    conn: &Connection,
    inner: &Inner,
    peer: DeviceId,
) -> Result<(), ServeErr> {
    let req = match read_message::<ObjectRequest>(&mut recv).await {
        Ok(req) => req,
        Err(e) if e.is_malformed() => {
            conn.close(close_code(CLOSE_MALFORMED), b"malformed object request");
            return Err(ServeErr {
                message: e.to_string(),
            });
        }
        Err(e) => {
            return Err(ServeErr {
                message: e.to_string(),
            });
        }
    };

    if let Some(control) = req.control {
        return control::serve(inner, peer, send, recv, control)
            .await
            .map_err(|message| ServeErr { message });
    }

    let id = match object_id_from_bytes(&req.object_id) {
        Ok(id) => id,
        Err(e) => {
            conn.close(close_code(CLOSE_MALFORMED), b"invalid object id");
            return Err(ServeErr {
                message: e.to_string(),
            });
        }
    };

    let found = tokio::task::spawn_blocking({
        let store = store.clone();
        move || {
            if store.contains(&id) {
                store.size_of(&id).map(Some)
            } else {
                Ok(None)
            }
        }
    })
    .await
    .map_err(|e| ServeErr {
        message: e.to_string(),
    })?
    .map_err(|e| ServeErr {
        message: e.to_string(),
    })?;

    match found {
        None => {
            let bytes = encode_frame(&ObjectHeader {
                found: false,
                size: 0,
            })
            .map_err(|e| ServeErr {
                message: e.to_string(),
            })?;
            send.write_all(&bytes).await.map_err(|e| ServeErr {
                message: e.to_string(),
            })?;
            let _ = send.finish();
        }
        Some(size) => {
            let bytes =
                encode_frame(&ObjectHeader { found: true, size }).map_err(|e| ServeErr {
                    message: e.to_string(),
                })?;
            send.write_all(&bytes).await.map_err(|e| ServeErr {
                message: e.to_string(),
            })?;
            let path = store.path_for(&id);
            let mut file = tokio::fs::File::open(&path).await.map_err(|e| ServeErr {
                message: e.to_string(),
            })?;
            let mut buf = vec![0u8; CHUNK];
            let mut sent = 0u64;
            let mut last_progress = Instant::now();
            loop {
                let n = file.read(&mut buf).await.map_err(|e| ServeErr {
                    message: e.to_string(),
                })?;
                if n == 0 {
                    break;
                }
                send.write_all(&buf[..n]).await.map_err(|e| ServeErr {
                    message: e.to_string(),
                })?;
                sent += n as u64;
                let now = Instant::now();
                if now.saturating_duration_since(last_progress) >= PROGRESS_INTERVAL {
                    last_progress = now;
                    inner.emit(NetEvent::ObjectProgress {
                        peer,
                        object: id,
                        incoming: false,
                        bytes: sent,
                    });
                }
            }
            let _ = send.finish();
        }
    }
    Ok(())
}

struct FetchFail {
    reason: String,
    not_found: bool,
}

impl FetchFail {
    fn err(reason: impl ToString) -> Self {
        Self {
            reason: reason.to_string(),
            not_found: false,
        }
    }
}

pub(crate) fn spawn_fetch(inner: Arc<Inner>, peer: DeviceId, object: ObjectId) {
    match inner.established_session(peer) {
        Some((conn, _write, sem, in_flight)) => {
            tokio::spawn(async move {
                fetch_object(inner, conn, peer, object, sem, in_flight).await;
            });
        }
        None => {
            tracing::debug!(peer = %peer, object = %object, "fetch dropped: peer not connected");
            inner.emit(NetEvent::ObjectFetchFailed {
                peer,
                object,
                reason: "peer not connected".into(),
                not_found: false,
            });
        }
    }
}

async fn fetch_object(
    inner: Arc<Inner>,
    conn: Connection,
    peer: DeviceId,
    object: ObjectId,
    sem: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<ObjectId>>>,
) {
    let present = match tokio::task::spawn_blocking({
        let store = inner.store.clone();
        move || store.contains(&object)
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            inner.emit(NetEvent::ObjectFetchFailed {
                peer,
                object,
                reason: e.to_string(),
                not_found: false,
            });
            return;
        }
    };
    if present {
        inner.emit(NetEvent::ObjectFetched { peer, object });
        return;
    }

    {
        let mut g = in_flight.lock().unwrap_or_else(|e| e.into_inner());
        if !g.insert(object) {
            return;
        }
    }

    let result = do_fetch(&inner, &conn, peer, object, &sem).await;
    in_flight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object);

    match result {
        Ok(()) => inner.emit(NetEvent::ObjectFetched { peer, object }),
        Err(FetchFail { reason, not_found }) => inner.emit(NetEvent::ObjectFetchFailed {
            peer,
            object,
            reason,
            not_found,
        }),
    }
}

async fn do_fetch(
    inner: &Inner,
    conn: &Connection,
    peer: DeviceId,
    object: ObjectId,
    sem: &Semaphore,
) -> Result<(), FetchFail> {
    let _permit = sem.acquire().await.map_err(FetchFail::err)?;

    let present = tokio::task::spawn_blocking({
        let store = inner.store.clone();
        move || store.contains(&object)
    })
    .await
    .map_err(FetchFail::err)?;
    if present {
        return Ok(());
    }

    let (mut send, mut recv) = conn.open_bi().await.map_err(FetchFail::err)?;
    let req = encode_frame(&ObjectRequest {
        object_id: object.as_bytes().to_vec(),
        control: None,
    })
    .map_err(FetchFail::err)?;
    send.write_all(&req).await.map_err(FetchFail::err)?;
    send.finish().map_err(FetchFail::err)?;

    let header: ObjectHeader = match read_message(&mut recv).await {
        Ok(h) => h,
        Err(e) => return Err(FetchFail::err(e)),
    };
    if !header.found {
        return Err(FetchFail {
            reason: "object not found".into(),
            not_found: true,
        });
    }

    let mut last_progress = Instant::now();
    let store = inner.store.clone();
    receive_object(&store, &mut recv, object, header.size, &mut |have| {
        let now = Instant::now();
        if now.saturating_duration_since(last_progress) >= PROGRESS_INTERVAL {
            last_progress = now;
            inner.emit(NetEvent::ObjectProgress {
                peer,
                object,
                incoming: true,
                bytes: have,
            });
        }
    })
    .await
}

async fn receive_object(
    store: &ObjectStore,
    recv: &mut RecvStream,
    expected: ObjectId,
    size: u64,
    on_progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), FetchFail> {
    let tmp = store.tmp_path().map_err(FetchFail::err)?;
    let outcome = write_and_import(store, recv, expected, size, &tmp, on_progress).await;
    if outcome.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    outcome
}

async fn write_and_import(
    store: &ObjectStore,
    recv: &mut RecvStream,
    expected: ObjectId,
    size: u64,
    tmp: &std::path::Path,
    on_progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), FetchFail> {
    let mut file = tokio::fs::File::create(tmp).await.map_err(FetchFail::err)?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = size;
    let mut buf = vec![0u8; CHUNK];

    while remaining > 0 {
        let want = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        match recv.read(&mut buf[..want]).await {
            Ok(Some(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buf[..n])
                    .await
                    .map_err(FetchFail::err)?;
                hasher.update(&buf[..n]);
                remaining -= n as u64;
                on_progress(size - remaining);
            }
            Ok(None) => return Err(FetchFail::err("truncated object stream")),
            Err(e) => return Err(FetchFail::err(e)),
        }
    }

    match recv.read(&mut buf[..1]).await {
        Ok(Some(0)) | Ok(None) => {}
        Ok(Some(_)) => return Err(FetchFail::err("object larger than header size")),
        Err(_) => {}
    }

    let actual = ObjectId::from(hasher.finalize());
    if actual != expected {
        return Err(FetchFail::err(format!(
            "hash mismatch: got {actual}, expected {expected}"
        )));
    }

    let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
    drop(file);

    let store = store.clone();
    let tmp = tmp.to_owned();
    tokio::task::spawn_blocking(move || store.import_verified(&tmp, &expected))
        .await
        .map_err(FetchFail::err)?
        .map_err(FetchFail::err)?;
    Ok(())
}

pub(crate) async fn dialer(inner: Arc<Inner>, endpoint: quinn::Endpoint, peer_id: DeviceId) {
    let mut delay = Duration::from_secs(1);
    loop {
        if inner.is_shutting_down() {
            return;
        }
        let addresses = {
            let trusted = inner.trusted.read().unwrap_or_else(|e| e.into_inner());
            match trusted.get(&peer_id) {
                Some(p) if !p.addresses.is_empty() => p.addresses.clone(),
                _ => return,
            }
        };

        if inner.has_session(peer_id) {
            wait_until_free(&inner, peer_id).await;
            delay = Duration::from_secs(1);
            continue;
        }

        match try_dial(&inner, &endpoint, peer_id, &addresses).await {
            Some(conn) => {
                delay = Duration::from_secs(1);
                drive_connection(inner.clone(), conn, true).await;
            }
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = wait_shutdown(&inner) => return,
                }
                delay = delay.saturating_mul(2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn try_dial(
    inner: &Inner,
    endpoint: &quinn::Endpoint,
    peer_id: DeviceId,
    addresses: &[String],
) -> Option<Connection> {
    let client = match make_client_config(&inner.tls, peer_id, &inner.trusted) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(peer = %peer_id, error = %e, "client tls config failed");
            return None;
        }
    };

    let ranked = rank_addresses(addresses);
    for addr_str in &ranked {
        if inner.is_shutting_down() || inner.has_session(peer_id) {
            return None;
        }
        let resolved = match tokio::net::lookup_host(addr_str.as_str()).await {
            Ok(iter) => iter.collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(peer = %peer_id, addr = %addr_str, error = %e, "dns resolve failed");
                continue;
            }
        };
        for sa in resolved {
            if inner.is_shutting_down() || inner.has_session(peer_id) {
                return None;
            }
            tracing::debug!(peer = %peer_id, %sa, "dialing");
            match endpoint.connect_with(client.clone(), sa, SERVER_NAME) {
                Ok(connecting) => match tokio::time::timeout(DIAL_ATTEMPT, connecting).await {
                    Ok(Ok(conn)) => return Some(conn),
                    Ok(Err(e)) => {
                        tracing::debug!(peer = %peer_id, %sa, error = %e, "dial failed");
                    }
                    Err(_) => {
                        tracing::debug!(peer = %peer_id, %sa, "dial timed out");
                    }
                },
                Err(e) => {
                    tracing::debug!(peer = %peer_id, %sa, error = %e, "connect_with failed");
                }
            }
        }
    }
    try_relay_dial(inner, endpoint, peer_id, &client).await
}

async fn try_relay_dial(
    inner: &Inner,
    endpoint: &quinn::Endpoint,
    peer_id: DeviceId,
    client: &quinn::ClientConfig,
) -> Option<Connection> {
    if inner.is_shutting_down() || inner.has_session(peer_id) {
        return None;
    }
    let target = inner
        .relay_target
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()?;
    let sock = inner.relay_sock.get()?;
    let prefer_ipv4 = sock.local_addr().ok().map(|addr| addr.is_ipv4());
    let relay_addr = resolve_relay(&target, prefer_ipv4).await?;
    sock.set_relay(Some(relay_addr));
    if let Err(err) = sock.send_bind(peer_id, &inner.identity).await {
        tracing::debug!(peer = %peer_id, error = %err, "relay bind failed");
        return None;
    }
    if inner.is_shutting_down() || inner.has_session(peer_id) {
        return None;
    }
    let virtual_addr = virtual_peer_addr(peer_id);
    tracing::debug!(peer = %peer_id, relay = %relay_addr, "dialing via relay");
    match endpoint.connect_with(client.clone(), virtual_addr, SERVER_NAME) {
        Ok(connecting) => match tokio::time::timeout(RELAY_DIAL, connecting).await {
            Ok(Ok(conn)) => Some(conn),
            Ok(Err(err)) => {
                tracing::debug!(peer = %peer_id, error = %err, "relay dial failed");
                None
            }
            Err(_) => {
                tracing::debug!(peer = %peer_id, "relay dial timed out");
                None
            }
        },
        Err(err) => {
            tracing::debug!(peer = %peer_id, error = %err, "relay connect_with failed");
            None
        }
    }
}

pub(crate) fn spawn_dialers(
    inner: &Arc<Inner>,
    endpoint: &quinn::Endpoint,
    dialers: &mut HashMap<DeviceId, tokio::task::JoinHandle<()>>,
) {
    let peers: Vec<PeerConfig> = inner
        .trusted
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();
    let wanted: HashSet<DeviceId> = peers
        .iter()
        .filter(|p| !p.addresses.is_empty() && p.id != inner.our_id)
        .map(|p| p.id)
        .collect();

    dialers.retain(|id, handle| {
        if wanted.contains(id) {
            true
        } else {
            handle.abort();
            false
        }
    });

    for p in peers {
        if p.addresses.is_empty() || p.id == inner.our_id || dialers.contains_key(&p.id) {
            continue;
        }
        let handle = tokio::spawn(dialer(inner.clone(), endpoint.clone(), p.id));
        dialers.insert(p.id, handle);
    }
}

pub(crate) fn apply_set_peers(inner: &Inner, peers: Vec<PeerConfig>) -> Vec<DeviceId> {
    let new_map: HashMap<DeviceId, PeerConfig> = peers.into_iter().map(|p| (p.id, p)).collect();
    let removed: Vec<DeviceId> = {
        let trusted = inner.trusted.read().unwrap_or_else(|e| e.into_inner());
        trusted
            .keys()
            .filter(|id| !new_map.contains_key(id))
            .copied()
            .collect()
    };
    *inner.trusted.write().unwrap_or_else(|e| e.into_inner()) = new_map;
    for id in &removed {
        inner.close_peer(*id, "removed from peer set");
    }
    inner.send_grants();
    removed
}
