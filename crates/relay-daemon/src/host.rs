use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relay_core::remote::{RemoteCall, RemoteError, RemoteErrorCode, RemoteReply, RemoteResult};
use relay_core::{ConfigApplied, ConfigChange, DeviceId, PairingCode, SpaceId};
use relay_engine::{
    Engine, EngineError, PeerInfo, Rejected, ScanReport, SyncInput, TransferDirection,
    TransferLive as EngineTransfer, WatchEvent, bookends, index_row,
};
use relay_ipc::{
    ActivityItem, EvictResult, FetchParams, FolderPairParams, Handler, Hello, HostKind, HostState,
    Idle, MountLive, OpenRemoteParams, PROTOCOL_VERSION, PairJoinParams, PairJoinResult,
    PairStartParams, PairStartResult, PairStatus, PeerLive, RemoteParams, RescanParams,
    RpcErrorBody, Status, TransferDirection as IpcDirection, TransferLive, Watching,
};
use relay_net::{NetCommand, NetSender, PeerConfig};

use crate::sizes::Sizer;

const ACTIVITY_CAP: usize = 500;
const PAIR_TTL: Duration = Duration::from_secs(10 * 60);
const PAIR_JOIN_WAIT: Duration = Duration::from_secs(60);
/// How long a config change may take on the loop, on top of a join's own wait.
const CONFIG_REPLY_WAIT: Duration = Duration::from_secs(30);

pub(crate) struct Host {
    pub home: PathBuf,
    pub kind: HostKind,
    pub pid: u32,
    pub started_at_ms: u64,
    pub use_watcher: bool,
    pub listen: Mutex<Option<String>>,
    pub state: Mutex<HostState>,
    pub message: Mutex<Option<String>>,
    pub peers: Mutex<Vec<PeerLive>>,
    pub mounts: Mutex<Vec<MountLive>>,
    pub transfers: Mutex<Vec<EngineTransfer>>,
    pub scans: Mutex<Vec<EngineTransfer>>,
    published: Mutex<Vec<EngineTransfer>>,
    pub activity: Mutex<VecDeque<ActivityItem>>,
    pub subscribers: Mutex<Vec<mpsc::Sender<ActivityItem>>>,
    pub sync_tx: Mutex<Option<mpsc::Sender<SyncInput>>>,
    pub net: Mutex<Option<NetSender>>,
    pub known_peers: Mutex<Vec<PeerConfig>>,
    pub pair: Mutex<PairPhase>,
    pub pair_cv: Condvar,
    pub pair_terms: Mutex<PairTerms>,
    /// What each connected peer supports and grants this device (D37).
    /// Kept apart from `peers`: grants can arrive before the engine reports
    /// the peer as connected.
    remote_access: Mutex<HashMap<DeviceId, RemoteAccess>>,
    /// Folder sizes counted for peers browsing this device (D42).
    pub sizer: Sizer,
    pub wake: Wake,
}

/// What the device on the other end of a pairing gets, chosen when the
/// pairing started or was joined.
#[derive(Clone, Debug, Default)]
pub(crate) struct PairTerms {
    pub share: Vec<SpaceId>,
    pub allow_manage: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct RemoteAccess {
    supports_remote: bool,
    manageable: bool,
}

/// What IPC allows on top of the network layer's own wait for a remote call
/// before giving up.
const REMOTE_CALL_MARGIN: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(crate) enum PairPhase {
    Idle,
    Waiting { expires_at_ms: u64 },
    Joining,
    Paired { peer_name: String, peer_id: String },
    Failed { reason: String },
    Expired,
}

pub(crate) struct Wake {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Wake {
    fn new() -> Self {
        Self {
            flag: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    pub fn notify(&self) {
        if let Ok(mut g) = self.flag.lock() {
            *g = true;
            self.cv.notify_all();
        }
    }

    pub fn wait(&self, timeout: Duration, stop: &AtomicBool) -> bool {
        let Ok(guard) = self.flag.lock() else {
            return false;
        };
        if *guard {
            return true;
        }
        let Ok((g, _)) = self.cv.wait_timeout_while(guard, timeout, |flag| {
            !*flag && !stop.load(Ordering::Relaxed)
        }) else {
            return false;
        };
        *g
    }

    pub fn take(&self) -> bool {
        self.flag
            .lock()
            .map(|mut g| {
                let was = *g;
                *g = false;
                was
            })
            .unwrap_or(false)
    }
}

impl Host {
    pub fn new(home: &Path, kind: HostKind, use_watcher: bool) -> Arc<Self> {
        Arc::new(Self {
            home: home.to_path_buf(),
            kind,
            pid: std::process::id(),
            started_at_ms: now_ms(),
            use_watcher,
            listen: Mutex::new(None),
            state: Mutex::new(HostState::Starting),
            message: Mutex::new(None),
            peers: Mutex::new(Vec::new()),
            mounts: Mutex::new(Vec::new()),
            transfers: Mutex::new(Vec::new()),
            scans: Mutex::new(Vec::new()),
            published: Mutex::new(Vec::new()),
            activity: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(Vec::new()),
            sync_tx: Mutex::new(None),
            net: Mutex::new(None),
            known_peers: Mutex::new(Vec::new()),
            pair: Mutex::new(PairPhase::Idle),
            pair_cv: Condvar::new(),
            pair_terms: Mutex::new(PairTerms::default()),
            remote_access: Mutex::new(HashMap::new()),
            sizer: Sizer::default(),
            wake: Wake::new(),
        })
    }

    pub fn set_net(&self, net: Option<NetSender>) {
        if let Ok(mut g) = self.net.lock() {
            *g = net;
        }
    }

    pub fn set_known_peers(&self, peers: Vec<PeerConfig>) {
        if let Ok(mut g) = self.known_peers.lock() {
            *g = peers;
        }
    }

    /// The peer list last handed to the network layer.
    pub fn known_peers(&self) -> Vec<PeerConfig> {
        self.known_peers
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn take_pair_terms(&self) -> PairTerms {
        self.pair_terms
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    fn set_pair_terms(&self, terms: PairTerms) {
        if let Ok(mut g) = self.pair_terms.lock() {
            *g = terms;
        }
    }

    pub fn note_remote_connected(&self, peer: DeviceId, supports_remote: bool) {
        if let Ok(mut map) = self.remote_access.lock() {
            map.entry(peer).or_default().supports_remote = supports_remote;
        }
    }

    pub fn note_remote_grants(&self, peer: DeviceId, manageable: bool) {
        if let Ok(mut map) = self.remote_access.lock() {
            map.entry(peer).or_default().manageable = manageable;
        }
    }

    pub fn forget_remote(&self, peer: DeviceId) {
        if let Ok(mut map) = self.remote_access.lock() {
            map.remove(&peer);
        }
    }

    /// Make a remote call on a peer by its local name.
    fn remote(&self, params: RemoteParams) -> Result<RemoteReply, RpcErrorBody> {
        let peer = self.peer_named(&params.peer)?;
        self.call_peer(peer.id, params.call)
            .map_err(|err| RpcErrorBody::new(err.code.as_str(), err.message))
    }

    pub(crate) fn peer_named(&self, name: &str) -> Result<PeerInfo, RpcErrorBody> {
        let peers = Engine::open_read_only(&self.home)
            .and_then(|engine| engine.peers())
            .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?;
        peer_with_name(peers, name)
    }

    /// Make a remote call on a connected peer and wait for the answer.
    pub(crate) fn call_peer(&self, peer: DeviceId, call: RemoteCall) -> RemoteResult {
        let net = self
            .net_sender()
            .map_err(|err| RemoteError::new(RemoteErrorCode::Offline, err.message))?;
        let wait = relay_net::call_timeout(&call) + REMOTE_CALL_MARGIN;
        let (reply, rx) = mpsc::channel();
        net.send(NetCommand::Control { peer, call, reply });
        rx.recv_timeout(wait)
            .map_err(|_| RemoteError::new(RemoteErrorCode::Timeout, "no answer from the network"))?
    }

    pub fn finish_pair(&self, result: Result<(String, String), String>) {
        if let Ok(mut g) = self.pair.lock() {
            *g = match result {
                Ok((peer_name, peer_id)) => {
                    self.push_activity(ActivityItem {
                        at_ms: now_ms(),
                        kind: "pair".into(),
                        summary: format!("paired with {peer_name}"),
                        detail: Some(peer_id.clone()),
                    });
                    PairPhase::Paired { peer_name, peer_id }
                }
                Err(reason) => {
                    let expired = reason.contains("expired");
                    self.push_activity(ActivityItem {
                        at_ms: now_ms(),
                        kind: "pair".into(),
                        summary: if expired {
                            "pairing code expired".into()
                        } else {
                            format!("pairing failed: {reason}")
                        },
                        detail: Some(reason.clone()),
                    });
                    if expired {
                        PairPhase::Expired
                    } else {
                        PairPhase::Failed { reason }
                    }
                }
            };
        }
        self.pair_cv.notify_all();
    }

    pub(crate) fn net_sender(&self) -> Result<NetSender, RpcErrorBody> {
        self.net
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| RpcErrorBody::new("unavailable", "sync loop is not running"))
    }

    fn require_running(&self) -> Result<(), RpcErrorBody> {
        let state = self.state.lock().map(|g| *g).unwrap_or(HostState::Error);
        match state {
            HostState::Paused => Err(RpcErrorBody::new(
                "paused",
                "Relay is paused; resume before pairing",
            )),
            HostState::Running => Ok(()),
            _ => Err(RpcErrorBody::new(
                "unavailable",
                "Relay is not running; resume sync and try again",
            )),
        }
    }

    fn sync_tx(&self) -> Option<mpsc::Sender<SyncInput>> {
        self.sync_tx.lock().ok().and_then(|g| g.clone())
    }

    /// Queue an input for the running loop without waiting. False when no
    /// loop runs.
    pub(crate) fn send_to_loop(&self, input: SyncInput) -> bool {
        self.sync_tx().is_some_and(|tx| tx.send(input).is_ok())
    }

    /// Send an input to the running loop and wait for its reply. `None` when
    /// no loop runs, so the caller writes directly. `wait` `None` waits as
    /// long as the loop is alive: the loop always replies, or drops the
    /// sender when it stops.
    fn on_loop<T>(
        &self,
        input: impl FnOnce(mpsc::Sender<Result<T, Rejected>>) -> SyncInput,
        wait: Option<Duration>,
    ) -> Option<Result<T, RpcErrorBody>> {
        let tx = self.sync_tx()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(input(reply_tx)).is_err() {
            return Some(Err(RpcErrorBody::new(
                "unavailable",
                "sync loop is not running",
            )));
        }
        let reply = match wait {
            Some(wait) => reply_rx
                .recv_timeout(wait)
                .map_err(|_| "timed out on the sync loop"),
            None => reply_rx.recv().map_err(|_| "the sync loop stopped"),
        };
        Some(match reply {
            Ok(result) => {
                result.map_err(|rejected| RpcErrorBody::new(rejected.code, rejected.message))
            }
            Err(message) => Err(RpcErrorBody::new("unavailable", message)),
        })
    }

    fn direct<T>(
        &self,
        f: impl FnOnce(&mut Engine) -> Result<T, EngineError>,
    ) -> Result<T, RpcErrorBody> {
        let mut engine = Engine::open_for_config(&self.home)
            .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?;
        f(&mut engine).map_err(|err| RpcErrorBody::new(err.code(), err.to_string()))
    }

    /// Apply a config change on the running engine loop when possible;
    /// otherwise write it directly (paused, or between reload cycles).
    pub(crate) fn config(&self, change: ConfigChange) -> Result<ConfigApplied, RpcErrorBody> {
        let wait = CONFIG_REPLY_WAIT
            + match &change {
                ConfigChange::JoinSpace { wait_ms, .. } => Duration::from_millis(*wait_ms),
                _ => Duration::ZERO,
            };
        let input = |reply| SyncInput::Config {
            change: change.clone(),
            reply,
        };
        let applied = match self.on_loop(input, Some(wait)) {
            Some(result) => result?,
            None => self.direct(|engine| engine.apply_config(&change))?,
        };
        // Status right after the reply should already show the change; the
        // loop's watch events follow a moment later and are idempotent.
        match (&change, &applied) {
            (ConfigChange::AddMount { space, .. }, ConfigApplied::Mount { mount, path }) => {
                self.track_mount(space, &mount.name, path.clone());
            }
            (ConfigChange::RemoveMount { space, mount }, _) => self.untrack_mount(space, mount),
            _ => {}
        }
        Ok(applied)
    }

    /// Write one demand-mode file here. Returns once it is on disk or has
    /// failed, however long the transfer takes; progress shows in `status`.
    pub(crate) fn fetch(&self, params: FetchParams) -> Result<(), RpcErrorBody> {
        let input = |reply| SyncInput::Fetch {
            space: params.space.clone(),
            mount: params.mount.clone(),
            path: params.path.clone(),
            reply,
        };
        match self.on_loop(input, None) {
            Some(result) => result,
            None => {
                self.direct(|engine| engine.fetch_path(&params.space, &params.mount, &params.path))
            }
        }
    }

    /// Index one path of a mount ahead of its scans.
    pub(crate) fn scan_first(
        &self,
        space: &str,
        mount: &str,
        path: &str,
    ) -> Result<(), RpcErrorBody> {
        let path = relay_core::LogicalPath::new(path)
            .map_err(|err| RpcErrorBody::new("invalid", err.to_string()))?;
        let input = |reply| SyncInput::ScanFirst {
            space: space.to_owned(),
            mount: mount.to_owned(),
            paths: vec![path.clone()],
            reply,
        };
        match self.on_loop(input, Some(CONFIG_REPLY_WAIT)) {
            Some(result) => result.map(drop),
            None => Err(RpcErrorBody::new(
                "unavailable",
                "Relay is not syncing right now",
            )),
        }
    }

    fn evict(&self, params: FetchParams) -> Result<EvictResult, RpcErrorBody> {
        let input = |reply| SyncInput::Evict {
            space: params.space.clone(),
            mount: params.mount.clone(),
            path: params.path.clone(),
            reply,
        };
        let evicted = match self.on_loop(input, Some(CONFIG_REPLY_WAIT)) {
            Some(result) => result?,
            None => {
                self.direct(|engine| engine.evict(&params.space, &params.mount, &params.path))?
            }
        };
        Ok(EvictResult { evicted })
    }

    fn pair_start(&self, params: PairStartParams) -> Result<PairStartResult, RpcErrorBody> {
        self.require_running()?;
        let net = self.net_sender()?;
        let engine = Engine::open_read_only(&self.home)
            .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?;
        let mut share = Vec::new();
        for name in &params.share {
            let space = engine
                .spaces()
                .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?
                .into_iter()
                .find(|s| s.name == *name)
                .ok_or_else(|| RpcErrorBody::new("not_found", format!("unknown space {name:?}")))?;
            share.push(space.id);
        }
        let code = PairingCode::generate();
        let expires_at = SystemTime::now() + PAIR_TTL;
        let expires_at_ms = expires_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.set_pair_terms(PairTerms {
            share,
            allow_manage: params.allow_manage,
        });
        if let Ok(mut g) = self.pair.lock() {
            *g = PairPhase::Waiting { expires_at_ms };
        }
        net.send(NetCommand::PairStart {
            code: code.digits().to_owned(),
            expires_at,
        });
        self.push_activity(ActivityItem {
            at_ms: now_ms(),
            kind: "pair".into(),
            summary: "waiting for a device to enter the pairing code".into(),
            detail: None,
        });
        Ok(PairStartResult {
            code: code.format(),
            expires_at_ms,
        })
    }

    fn pair_status(&self) -> PairStatus {
        let phase = self
            .pair
            .lock()
            .map(|g| g.clone())
            .unwrap_or(PairPhase::Idle);
        match phase {
            PairPhase::Idle => PairStatus::Idle,
            PairPhase::Waiting { expires_at_ms } => {
                if now_ms() >= expires_at_ms {
                    if let Ok(mut g) = self.pair.lock()
                        && matches!(*g, PairPhase::Waiting { .. })
                    {
                        *g = PairPhase::Expired;
                    }
                    PairStatus::Expired
                } else {
                    PairStatus::Waiting
                }
            }
            PairPhase::Joining => PairStatus::Waiting,
            PairPhase::Paired { peer_name, peer_id } => PairStatus::Paired { peer_name, peer_id },
            PairPhase::Failed { reason } => PairStatus::Failed { reason },
            PairPhase::Expired => PairStatus::Expired,
        }
    }

    fn pair_join(&self, params: PairJoinParams) -> Result<PairJoinResult, RpcErrorBody> {
        self.require_running()?;
        let net = self.net_sender()?;
        let code = PairingCode::parse(&params.code)
            .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
        self.set_pair_terms(PairTerms {
            share: Vec::new(),
            allow_manage: params.allow_manage,
        });
        if let Ok(mut g) = self.pair.lock() {
            *g = PairPhase::Joining;
        }
        net.send(NetCommand::PairJoin {
            code: code.digits().to_owned(),
            addr: params.addr,
        });
        let deadline = Instant::now() + PAIR_JOIN_WAIT;
        let mut guard = self
            .pair
            .lock()
            .map_err(|_| RpcErrorBody::new("internal", "pairing lock"))?;
        loop {
            match &*guard {
                PairPhase::Paired { peer_name, peer_id } => {
                    return Ok(PairJoinResult {
                        peer_name: peer_name.clone(),
                        peer_id: peer_id.clone(),
                    });
                }
                PairPhase::Failed { reason } => {
                    return Err(RpcErrorBody::new("pair_failed", reason.clone()));
                }
                PairPhase::Expired => {
                    return Err(RpcErrorBody::new("expired", "pairing code expired"));
                }
                PairPhase::Idle => {
                    return Err(RpcErrorBody::new("cancelled", "pairing cancelled"));
                }
                _ => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RpcErrorBody::new(
                    "timeout",
                    "timed out waiting to pair (60s)",
                ));
            }
            let (next, wait) = self
                .pair_cv
                .wait_timeout(guard, left)
                .map_err(|_| RpcErrorBody::new("internal", "pairing wait"))?;
            guard = next;
            if wait.timed_out()
                && !matches!(
                    *guard,
                    PairPhase::Paired { .. } | PairPhase::Failed { .. } | PairPhase::Expired
                )
            {
                return Err(RpcErrorBody::new(
                    "timeout",
                    "timed out waiting to pair (60s)",
                ));
            }
        }
    }

    fn pair_cancel(&self) -> Result<(), RpcErrorBody> {
        if let Ok(net) = self.net_sender() {
            net.send(NetCommand::PairCancel);
        }
        if let Ok(mut g) = self.pair.lock() {
            *g = PairPhase::Idle;
        }
        self.set_pair_terms(PairTerms::default());
        self.pair_cv.notify_all();
        Ok(())
    }

    pub fn set_state(&self, state: HostState, message: Option<String>) {
        if let Ok(mut g) = self.state.lock() {
            *g = state;
        }
        if let Ok(mut g) = self.message.lock() {
            *g = message;
        }
    }

    pub fn set_listen(&self, listen: Option<String>) {
        if let Ok(mut g) = self.listen.lock() {
            *g = listen;
        }
    }

    pub fn set_sync_tx(&self, tx: Option<mpsc::Sender<SyncInput>>) {
        if let Ok(mut g) = self.sync_tx.lock() {
            *g = tx;
        }
    }

    pub fn seed_mounts(&self, engine: &Engine) {
        let default = if self.use_watcher {
            Watching::Native
        } else {
            Watching::Poll
        };
        let Ok(status) = engine.status() else {
            return;
        };
        let mounts = status
            .mounts
            .into_iter()
            .map(|m| MountLive {
                watching: if m.path.is_none() {
                    Watching::None
                } else {
                    default
                },
                space: m.space,
                mount: m.mount,
                path: m.path,
                last_scan_ms: m.last_scan_ms,
                last_scan_summary: None,
                last_error: m.last_error,
            })
            .collect();
        if let Ok(mut g) = self.mounts.lock() {
            *g = mounts;
        }
    }

    pub fn apply_watch(&self, event: &WatchEvent) {
        match event {
            WatchEvent::Started { mounts } => {
                for name in mounts {
                    if let Some((space, mount)) = name.split_once('/') {
                        self.track_mount(space, mount, None);
                    }
                }
            }
            WatchEvent::MountRemoved { space, mount } => self.untrack_mount(space, mount),
            WatchEvent::Scanned {
                space,
                mount,
                report,
                ..
            } => {
                self.update_mount(space, mount, |m| {
                    m.last_scan_ms = Some(now_ms() as i64);
                    m.last_scan_summary = Some(scan_summary(report));
                    m.last_error = None;
                });
                self.clear_scan(space, mount);
            }
            WatchEvent::ScanFailed {
                space,
                mount,
                error,
            } => {
                self.update_mount(space, mount, |m| {
                    m.last_scan_ms = Some(now_ms() as i64);
                    m.last_error = Some(error.clone());
                });
                self.clear_scan(space, mount);
            }
            WatchEvent::Transfers(rows) => {
                if let Ok(mut live) = self.transfers.lock() {
                    *live = rows.clone();
                }
                self.publish_progress();
            }
            WatchEvent::ScanProgress {
                space,
                mount,
                files_seen,
                bytes_hashed,
            } => {
                self.note_scan(space, mount, *files_seen, *bytes_hashed);
            }
            WatchEvent::WatcherUnavailable { space, mount, .. } => {
                self.update_mount(space, mount, |m| {
                    m.watching = Watching::Poll;
                });
            }
            WatchEvent::PeerConnected { peer, name } => {
                if let Ok(mut peers) = self.peers.lock() {
                    peers.retain(|p| p.id != *peer);
                    peers.push(PeerLive {
                        id: peer.clone(),
                        name: name.clone(),
                        connected_at_ms: now_ms(),
                        supports_remote: false,
                        manageable: false,
                    });
                }
            }
            WatchEvent::PeerDisconnected { peer } => {
                if let Ok(mut peers) = self.peers.lock() {
                    peers.retain(|p| p.id != *peer);
                }
            }
            _ => {}
        }
        if let Some(item) = activity_from_watch(event) {
            self.push_activity(item);
        }
    }

    /// Add a mount to the live list unless it is already there.
    fn track_mount(&self, space: &str, mount: &str, path: Option<PathBuf>) {
        let Ok(mut live) = self.mounts.lock() else {
            return;
        };
        if live.iter().any(|m| m.space == space && m.mount == mount) {
            return;
        }
        live.push(MountLive {
            space: space.to_owned(),
            mount: mount.to_owned(),
            path,
            watching: if self.use_watcher {
                Watching::Native
            } else {
                Watching::Poll
            },
            last_scan_ms: None,
            last_scan_summary: None,
            last_error: None,
        });
    }

    fn untrack_mount(&self, space: &str, mount: &str) {
        if let Ok(mut live) = self.mounts.lock() {
            live.retain(|m| !(m.space == space && m.mount == mount));
        }
    }

    fn update_mount(&self, space: &str, mount: &str, f: impl FnOnce(&mut MountLive)) {
        if let Ok(mut live) = self.mounts.lock()
            && let Some(row) = live
                .iter_mut()
                .find(|m| m.space == space && m.mount == mount)
        {
            f(row);
        }
    }

    pub fn push_activity(&self, item: ActivityItem) {
        if let Ok(mut ring) = self.activity.lock() {
            ring.push_back(item.clone());
            while ring.len() > ACTIVITY_CAP {
                ring.pop_front();
            }
        }
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|tx| tx.send(item.clone()).is_ok());
        }
    }

    fn live_peers(&self) -> Vec<PeerLive> {
        let mut peers = self.peers.lock().map(|g| g.clone()).unwrap_or_default();
        if let Ok(access) = self.remote_access.lock() {
            for peer in &mut peers {
                if let Some(a) = access
                    .iter()
                    .find(|(id, _)| id.to_string() == peer.id)
                    .map(|(_, a)| *a)
                {
                    peer.supports_remote = a.supports_remote;
                    peer.manageable = a.manageable;
                }
            }
        }
        peers
    }

    pub fn snapshot(&self) -> Status {
        let state = self.state.lock().map(|g| *g).unwrap_or(HostState::Error);
        let mut transfers = self.progress_rows();
        if state == HostState::Paused {
            for row in &mut transfers {
                row.bytes_per_sec = 0;
            }
        }
        let quiet = state == HostState::Running && transfers.is_empty();
        Status {
            state,
            message: self.message.lock().ok().and_then(|g| g.clone()),
            listen: self.listen.lock().ok().and_then(|g| g.clone()),
            peers: self.live_peers(),
            mounts: self.mounts.lock().map(|g| g.clone()).unwrap_or_default(),
            transfers,
            idle: Idle {
                quiet,
                replica_behind: replica_backlog(&self.home),
            },
        }
    }

    fn progress_rows(&self) -> Vec<TransferLive> {
        let mut rows = self
            .transfers
            .lock()
            .map(|g| g.iter().map(to_ipc_transfer).collect::<Vec<_>>())
            .unwrap_or_default();
        if let Ok(scans) = self.scans.lock() {
            rows.extend(scans.iter().map(to_ipc_transfer));
        }
        rows
    }

    fn note_scan(&self, space: &str, mount: &str, files_seen: u64, bytes_hashed: u64) {
        if let Ok(mut scans) = self.scans.lock() {
            if let Some(row) = scans
                .iter_mut()
                .find(|row| row.space == space && row.mount.as_deref() == Some(mount))
            {
                row.files_done = files_seen;
                row.bytes_done = bytes_hashed;
            } else {
                let mut row = index_row(space, mount, files_seen, bytes_hashed);
                row.started_at_ms = now_ms();
                scans.push(row);
            }
        }
        self.publish_progress();
    }

    fn clear_scan(&self, space: &str, mount: &str) {
        let removed = self.scans.lock().is_ok_and(|mut scans| {
            let before = scans.len();
            scans.retain(|row| !(row.space == space && row.mount.as_deref() == Some(mount)));
            scans.len() != before
        });
        if removed {
            self.publish_progress();
        }
    }

    fn engine_progress(&self) -> Vec<EngineTransfer> {
        let mut rows = self.transfers.lock().map(|g| g.clone()).unwrap_or_default();
        if let Ok(scans) = self.scans.lock() {
            rows.extend(scans.iter().cloned());
        }
        rows
    }

    fn publish_progress(&self) {
        let next = self.engine_progress();
        let prev = self.published.lock().map(|g| g.clone()).unwrap_or_default();
        let now = now_ms();
        for item in bookends(&prev, &next, now) {
            self.push_activity(ActivityItem {
                at_ms: now,
                kind: item.kind,
                summary: item.summary,
                detail: None,
            });
        }
        if let Ok(mut published) = self.published.lock() {
            *published = next;
        }
    }

    pub fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL_VERSION,
            relay_version: env!("CARGO_PKG_VERSION").to_owned(),
            host: self.kind,
            pid: self.pid,
            started_at_ms: self.started_at_ms,
        }
    }
}

impl Handler for Host {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcErrorBody> {
        match method {
            "hello" => serde_json::to_value(self.hello()).map_err(internal),
            "status" => serde_json::to_value(self.snapshot()).map_err(internal),
            "pause" => {
                set_paused_flag(&self.home, true)?;
                self.set_state(HostState::Paused, None);
                serde_json::to_value(self.snapshot()).map_err(internal)
            }
            "resume" => {
                set_paused_flag(&self.home, false)?;
                self.wake.notify();
                serde_json::to_value(self.snapshot()).map_err(internal)
            }
            "fetch" => {
                let params: FetchParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                self.fetch(params)?;
                Ok(serde_json::json!({}))
            }
            "evict" => {
                let params: FetchParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.evict(params)?).map_err(internal)
            }
            "rescan" => {
                let state = self.state.lock().map(|g| *g).unwrap_or(HostState::Error);
                if state == HostState::Paused {
                    return Err(RpcErrorBody::new("paused", "Relay is paused"));
                }
                let params: RescanParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                let engine = Engine::open_read_only(&self.home)
                    .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?;
                let targets = engine
                    .resolve_rescan_targets(params.space.as_deref(), params.mount.as_deref())
                    .map_err(|err| RpcErrorBody::new("not_found", err.to_string()))?;
                if targets.is_empty() {
                    return Err(RpcErrorBody::new("not_found", "no matching local mounts"));
                }
                let mounts = targets.iter().map(|(s, m, _)| (*s, *m)).collect();
                let queued: Vec<String> = targets.into_iter().map(|(_, _, name)| name).collect();
                let tx = self
                    .sync_tx
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .ok_or_else(|| RpcErrorBody::new("unavailable", "sync loop is not running"))?;
                tx.send(SyncInput::Rescan { mounts })
                    .map_err(|_| RpcErrorBody::new("unavailable", "sync loop is not running"))?;
                serde_json::to_value(relay_ipc::RescanResult { queued }).map_err(internal)
            }
            "config" => {
                let change: ConfigChange = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.config(change)?).map_err(internal)
            }
            "pair_start" => {
                let params: PairStartParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.pair_start(params)?).map_err(internal)
            }
            "pair_status" => serde_json::to_value(self.pair_status()).map_err(internal),
            "open_remote" => {
                let params: OpenRemoteParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(crate::quick_open::open(self, &params)?).map_err(internal)
            }
            "quick_opens" => serde_json::to_value(crate::quick_open::list(self)?).map_err(internal),
            "quick_open_remove" => {
                let space = params
                    .get("space")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcErrorBody::new("invalid_params", "space is required"))?;
                let note = crate::quick_open::remove(self, space)?;
                Ok(serde_json::json!({ "note": note }))
            }
            "folder_pair_preview" => {
                let params: FolderPairParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(crate::folder_pair::preview(self, &params)?).map_err(internal)
            }
            "folder_pair" => {
                let params: FolderPairParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(crate::folder_pair::run(self, &params)?).map_err(internal)
            }
            "remote" => {
                let params: RemoteParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.remote(params)?).map_err(internal)
            }
            "pair_join" => {
                let params: PairJoinParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.pair_join(params)?).map_err(internal)
            }
            "pair_cancel" => {
                self.pair_cancel()?;
                Ok(serde_json::json!({}))
            }
            "activity" => {
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let items = self
                    .activity
                    .lock()
                    .map(|ring| {
                        let n = limit.unwrap_or(ring.len()).min(ring.len());
                        ring.iter().rev().take(n).rev().cloned().collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                serde_json::to_value(items).map_err(internal)
            }
            other => Err(RpcErrorBody::new(
                "unknown_method",
                format!("unknown method {other:?}"),
            )),
        }
    }

    fn subscribe(&self) -> mpsc::Receiver<ActivityItem> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.push(tx);
        }
        rx
    }
}

fn replica_backlog(home: &Path) -> Option<u64> {
    Engine::open_read_only(home)
        .ok()?
        .replica_backlog()
        .ok()
        .flatten()
}

fn set_paused_flag(home: &Path, paused: bool) -> Result<(), RpcErrorBody> {
    let mut engine = Engine::open_for_config(home)
        .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?;
    engine
        .set_paused(paused)
        .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))
}

fn internal(err: serde_json::Error) -> RpcErrorBody {
    RpcErrorBody::new("internal", err.to_string())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn to_ipc_transfer(row: &EngineTransfer) -> TransferLive {
    TransferLive {
        peer_id: row.peer_id.clone(),
        peer_name: row.peer_name.clone(),
        space: row.space.clone(),
        mount: row.mount.clone(),
        direction: match row.direction {
            TransferDirection::Receive => IpcDirection::Receive,
            TransferDirection::Send => IpcDirection::Send,
            TransferDirection::Index => IpcDirection::Index,
        },
        files_done: row.files_done,
        files_total: row.files_total,
        bytes_done: row.bytes_done,
        bytes_total: row.bytes_total,
        bytes_per_sec: row.bytes_per_sec,
        started_at_ms: row.started_at_ms,
        retries: row.retries,
        current_path: row.current_path.clone(),
    }
}

fn scan_summary(report: &ScanReport) -> String {
    format!(
        "{} created, {} modified, {} deleted",
        report.created, report.modified, report.deleted
    )
}

fn activity_from_watch(event: &WatchEvent) -> Option<ActivityItem> {
    if matches!(
        event,
        WatchEvent::Transfers(_) | WatchEvent::ScanProgress { .. }
    ) {
        return None;
    }
    // A partial rescan of an unchanged file still finishes, so the index row
    // can close. It is not activity.
    if let WatchEvent::Scanned { full, report, .. } = event
        && !*full
        && !report.has_changes()
    {
        return None;
    }
    let (kind, summary, detail) = match event {
        WatchEvent::Started { mounts } => (
            "started",
            if mounts.is_empty() {
                "watching (no local mounts)".to_owned()
            } else {
                format!("watching {}", mounts.join(", "))
            },
            None,
        ),
        WatchEvent::MountRemoved { space, mount } => (
            "mount_removed",
            format!("stopped syncing {space}/{mount}"),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::Scanned {
            space,
            mount,
            report,
            ..
        } => (
            "scan",
            format!("{space}/{mount}: {}", scan_summary(report)),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::ScanFailed {
            space,
            mount,
            error,
        } => (
            "scan_failed",
            format!("{space}/{mount}: {error}"),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::WatcherUnavailable {
            space,
            mount,
            error,
        } => (
            "warning",
            format!("{space}/{mount}: watcher unavailable: {error}"),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::PeerConnected { peer, name } => (
            "peer",
            format!("connected to {name} {peer}"),
            Some(peer.clone()),
        ),
        WatchEvent::PeerDisconnected { peer } => (
            "peer",
            format!("disconnected from {peer}"),
            Some(peer.clone()),
        ),
        WatchEvent::OffersReceived { peer, spaces } => (
            "offer",
            format!("{peer} offers: {}", spaces.join(", ")),
            Some(peer.clone()),
        ),
        WatchEvent::RemoteApplied {
            peer,
            space,
            mount,
            written,
            deleted,
            conflicts,
            skipped,
        } => (
            "sync",
            format!(
                "{space}/{mount} from {peer}: {written} written, {deleted} deleted, {conflicts} conflicts, {skipped} skipped"
            ),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::SentChanges {
            peer,
            space,
            entries,
        } => (
            "sync",
            format!("sent {entries} changes of {space} to {peer}"),
            Some(space.clone()),
        ),
        WatchEvent::SyncWarning { peer, path, reason } => {
            ("warning", format!("{peer} {path}: {reason}"), None)
        }
        WatchEvent::DeletesHeld {
            peer,
            space,
            mount,
            deletions,
            live,
        } => (
            "deletes_held",
            format!("{peer} wants to delete {deletions} of {live} files in {space}/{mount}"),
            Some(format!("{space}/{mount}")),
        ),
        WatchEvent::Stopped => ("stopped", "sync loop stopped".to_owned(), None),
        // Live progress is a snapshot, not an activity-log row (D28).
        WatchEvent::Transfers(_) | WatchEvent::ScanProgress { .. } => return None,
    };
    Some(ActivityItem {
        at_ms: now_ms(),
        kind: kind.to_owned(),
        summary,
        detail,
    })
}

/// The one peer called `name`. Names come from pairing and from the peer's
/// own `Hello`, so two peers can share one; such a name is refused rather
/// than resolved by row order, or a call meant for one device could reach
/// the other.
fn peer_with_name(peers: Vec<PeerInfo>, name: &str) -> Result<PeerInfo, RpcErrorBody> {
    let mut matching = peers.into_iter().filter(|p| p.name == name);
    let found = matching
        .next()
        .ok_or_else(|| RpcErrorBody::new("not_found", format!("unknown peer {name:?}")))?;
    if matching.next().is_some() {
        return Err(RpcErrorBody::new(
            "conflict",
            format!("ambiguous peer name {name:?}: more than one peer has it"),
        ));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            id: DeviceId::random(),
            addresses: Vec::new(),
            added_at_ms: 0,
            last_seen_ms: None,
            revoked: false,
            may_manage: false,
        }
    }

    #[test]
    fn a_name_two_peers_share_is_refused() {
        let peers = vec![peer("laptop"), peer("alice"), peer("alice")];
        let laptop = peers[0].id;
        assert_eq!(peer_with_name(peers.clone(), "laptop").unwrap().id, laptop);
        let err = peer_with_name(peers.clone(), "alice").unwrap_err();
        assert_eq!(err.code, "conflict");
        assert!(err.message.contains("ambiguous"), "{}", err.message);
        assert_eq!(
            peer_with_name(peers, "nobody").unwrap_err().code,
            "not_found"
        );
    }

    fn transfer(bytes_done: u64) -> EngineTransfer {
        EngineTransfer {
            peer_id: "peer".into(),
            peer_name: "macbook".into(),
            space: "Photos".into(),
            mount: None,
            direction: TransferDirection::Receive,
            files_done: 0,
            files_total: Some(2),
            bytes_done,
            bytes_total: Some(100),
            bytes_per_sec: 10,
            started_at_ms: 1_000,
            retries: 0,
            current_path: None,
        }
    }

    #[test]
    fn activity_records_one_start_and_one_finish_per_transfer() {
        let home = std::env::temp_dir();
        let host = Host::new(&home, HostKind::Cli, false);
        let open = transfer(10);
        host.apply_watch(&WatchEvent::Transfers(vec![open.clone()]));
        let mut tick = open.clone();
        tick.bytes_done = 40;
        tick.bytes_per_sec = 30;
        host.apply_watch(&WatchEvent::Transfers(vec![tick.clone()]));
        host.apply_watch(&WatchEvent::Transfers(vec![tick]));
        host.apply_watch(&WatchEvent::Transfers(Vec::new()));

        let activity = host.activity.lock().expect("activity");
        assert_eq!(activity.len(), 2, "{activity:?}");
        assert!(activity[0].summary.contains("started receiving"));
        assert!(activity[1].summary.contains("finished"));
        assert!(host.snapshot().transfers.is_empty());
    }
}
