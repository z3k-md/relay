use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relay_core::{PairingCode, SpaceId};
use relay_engine::{Engine, ScanReport, SyncInput, WatchEvent};
use relay_ipc::{
    ActivityItem, Handler, Hello, HostKind, HostState, MountLive, PROTOCOL_VERSION, PairJoinParams,
    PairJoinResult, PairStartParams, PairStartResult, PairStatus, PeerLive, RescanParams,
    RpcErrorBody, Status, Watching,
};
use relay_net::{NetCommand, NetSender, PeerConfig};

const ACTIVITY_CAP: usize = 500;
const PAIR_TTL: Duration = Duration::from_secs(10 * 60);
const PAIR_JOIN_WAIT: Duration = Duration::from_secs(60);

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
    pub activity: Mutex<VecDeque<ActivityItem>>,
    pub subscribers: Mutex<Vec<mpsc::Sender<ActivityItem>>>,
    pub sync_tx: Mutex<Option<mpsc::Sender<SyncInput>>>,
    pub net: Mutex<Option<NetSender>>,
    pub known_peers: Mutex<Vec<PeerConfig>>,
    pub pair: Mutex<PairPhase>,
    pub pair_cv: Condvar,
    pub pair_share: Mutex<Vec<SpaceId>>,
    pub wake: Wake,
}

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
            activity: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(Vec::new()),
            sync_tx: Mutex::new(None),
            net: Mutex::new(None),
            known_peers: Mutex::new(Vec::new()),
            pair: Mutex::new(PairPhase::Idle),
            pair_cv: Condvar::new(),
            pair_share: Mutex::new(Vec::new()),
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

    pub fn take_pair_share(&self) -> Vec<SpaceId> {
        self.pair_share
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
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

    fn net_sender(&self) -> Result<NetSender, RpcErrorBody> {
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
        if let Ok(mut g) = self.pair_share.lock() {
            *g = share;
        }
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
        if let Ok(mut g) = self.pair_share.lock() {
            g.clear();
        }
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
                if let Ok(mut live) = self.mounts.lock() {
                    for name in mounts {
                        if let Some((space, mount)) = name.split_once('/')
                            && !live.iter().any(|m| m.space == space && m.mount == mount)
                        {
                            live.push(MountLive {
                                space: space.to_owned(),
                                mount: mount.to_owned(),
                                path: None,
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
                    }
                }
            }
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

    pub fn snapshot(&self) -> Status {
        Status {
            state: self.state.lock().map(|g| *g).unwrap_or(HostState::Error),
            message: self.message.lock().ok().and_then(|g| g.clone()),
            listen: self.listen.lock().ok().and_then(|g| g.clone()),
            peers: self.peers.lock().map(|g| g.clone()).unwrap_or_default(),
            mounts: self.mounts.lock().map(|g| g.clone()).unwrap_or_default(),
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
            "pair_start" => {
                let params: PairStartParams = serde_json::from_value(params)
                    .map_err(|err| RpcErrorBody::new("invalid_params", err.to_string()))?;
                serde_json::to_value(self.pair_start(params)?).map_err(internal)
            }
            "pair_status" => serde_json::to_value(self.pair_status()).map_err(internal),
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

fn scan_summary(report: &ScanReport) -> String {
    format!(
        "{} created, {} modified, {} deleted",
        report.created, report.modified, report.deleted
    )
}

fn activity_from_watch(event: &WatchEvent) -> Option<ActivityItem> {
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
    };
    Some(ActivityItem {
        at_ms: now_ms(),
        kind: kind.to_owned(),
        summary,
        detail,
    })
}
