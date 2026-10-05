use std::collections::HashMap;
use std::path::PathBuf;

use relay_core::remote::{RemoteCall, RemoteError, RemoteErrorCode, RemoteReply};
use relay_core::speed::SpeedReport;
use relay_core::{ConfigApplied, ConfigChange, DeviceId};
use relay_engine::{
    ConflictClass, DeleteHoldDecision, Engine, EngineError, Resolution,
    resolve_conflict as engine_resolve_conflict, resolve_git_conflicts as engine_resolve_git,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
#[cfg(not(target_os = "android"))]
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;

use crate::error::{anyhow_chain, error_chain};
use crate::runner::RunnerState;
#[cfg(not(target_os = "android"))]
use crate::sidecar::{self, CliInstallResult, CliStatus, ShellKind};
#[cfg(not(target_os = "android"))]
use crate::updates::{self, UpdateInfo};
use crate::{AppState, settings};
use relay_ipc::{Client, PairJoinParams, PairStartParams, PeerLive, SpeedTestParams};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub initialized: bool,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub suggested_name: String,
    pub runner: RunnerState,
    pub version: String,
    /// Android builds hide desktop-only settings (updater, autostart, CLI).
    pub mobile: bool,
    pub peer_count: u32,
    pub connected_peers: u32,
    pub space_count: u32,
    pub mount_count: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub name: String,
    pub id: String,
    pub short_id: String,
    pub address: String,
    pub connected: bool,
    /// When the current session started. Set only while `connected`.
    pub connected_since_ms: Option<i64>,
    /// Last live contact. `None` until this peer has connected once.
    /// While offline, this is when that contact ended.
    pub last_seen_ms: Option<i64>,
    /// This peer may browse this device and set up sync on it.
    pub allowed_to_manage: bool,
    /// This peer lets this device manage it. Known only while connected.
    pub can_manage: bool,
    /// This peer's Relay answers remote calls. Known only while connected.
    pub supports_remote: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountView {
    pub name: String,
    pub path: Option<String>,
    pub attached: bool,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceView {
    pub name: String,
    pub id: String,
    pub mounts: Vec<MountView>,
    pub shared_with: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferedMountView {
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferView {
    pub peer: String,
    pub peer_id: String,
    pub name: String,
    pub mounts: Vec<OfferedMountView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConflictClassView {
    File { original: String },
    Git { git_dir: String, is_ref: bool },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteHoldView {
    pub peer: String,
    pub peer_name: String,
    pub space: String,
    pub space_id: String,
    pub mount: String,
    pub mount_id: String,
    pub deletions: u32,
    pub live: u32,
    pub held_at_ms: i64,
    pub decision: Option<DeleteHoldDecision>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictView {
    pub path: String,
    pub space: String,
    pub mount: String,
    pub device_id: String,
    pub device_short: String,
    pub device_name: Option<String>,
    pub class: ConflictClassView,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveReportView {
    pub space: String,
    pub mount: String,
    pub copy: String,
    pub original: String,
    pub resolution: String,
    pub scanned: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitResolveReportView {
    pub space: String,
    pub mount: String,
    pub git_dir: String,
    pub deleted: Vec<String>,
    pub kept: Vec<String>,
    pub scanned: bool,
}

pub fn version_string() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("RELAY_GIT_REV"))
}

pub fn suggested_device_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if relay_core::validate_name(trimmed).is_ok() {
                return trimmed.to_owned();
            }
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output()
        && output.status.success()
    {
        let value = String::from_utf8_lossy(&output.stdout);
        let trimmed = value.trim();
        if relay_core::validate_name(trimmed).is_ok() {
            return trimmed.to_owned();
        }
    }
    "this-device".to_owned()
}

pub(crate) fn open_ro(home: &std::path::Path) -> Result<Engine, String> {
    Engine::open_read_only(home).map_err(|err| error_chain(&err))
}

#[tauri::command(async)]
pub fn get_overview(app: AppHandle) -> Result<Overview, String> {
    let state = app.state::<AppState>();
    let runner = state.runner.state();
    let connected = state.runner.connected_count() as u32;
    let suggested_name = suggested_device_name();

    match Engine::open_read_only(&state.home) {
        Ok(engine) => {
            let peers = engine.peers().map_err(|err| error_chain(&err))?;
            let spaces = engine.spaces().map_err(|err| error_chain(&err))?;
            let mounts = engine.mounts(None).map_err(|err| error_chain(&err))?;
            Ok(Overview {
                initialized: true,
                device_name: Some(engine.device().name.clone()),
                device_id: Some(engine.device().id.to_string()),
                suggested_name,
                runner,
                version: version_string(),
                mobile: cfg!(target_os = "android"),
                peer_count: peers.len() as u32,
                connected_peers: connected,
                space_count: spaces.len() as u32,
                mount_count: mounts.len() as u32,
            })
        }
        Err(EngineError::NotInitialized) => Ok(Overview {
            initialized: false,
            device_name: None,
            device_id: None,
            suggested_name,
            runner: RunnerState::NotInitialized,
            version: version_string(),
            mobile: cfg!(target_os = "android"),
            peer_count: 0,
            connected_peers: 0,
            space_count: 0,
            mount_count: 0,
        }),
        Err(err) => Err(error_chain(&err)),
    }
}

#[tauri::command(async)]
pub fn init_device(app: AppHandle, name: String) -> Result<Overview, String> {
    let state = app.state::<AppState>();
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err("Device name cannot be empty.".to_owned());
    }
    Engine::init(&state.home, &name).map_err(|err| error_chain(&err))?;
    let _ = settings::apply_first_run_defaults(&app);
    apply_autostart(&app, true);
    maybe_install_cli(&app);
    state.runner.start(&app);
    get_overview(app)
}

/// Live sessions from the running host when one is listening, otherwise the
/// in-app runner. The host is the source of truth for the background service.
/// Connected peers by device id, from the running host when there is one.
fn live_sessions(app: &AppHandle) -> HashMap<String, PeerLive> {
    if let Ok(Some(mut client)) = host_client(app)
        && let Ok(status) = client.status()
    {
        return status
            .peers
            .into_iter()
            .map(|peer| (peer.id.clone(), peer))
            .collect();
    }
    app.state::<AppState>()
        .runner
        .connected_since()
        .into_iter()
        .map(|(id, since)| {
            let peer = PeerLive {
                id: id.clone(),
                name: String::new(),
                connected_at_ms: u64::try_from(since).unwrap_or_default(),
                supports_remote: false,
                manageable: false,
            };
            (id, peer)
        })
        .collect()
}

#[tauri::command(async)]
pub fn list_peers(app: AppHandle) -> Result<Vec<PeerView>, String> {
    let sessions = live_sessions(&app);
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let peers = engine.peers().map_err(|err| error_chain(&err))?;
    Ok(peers
        .into_iter()
        .map(|p| {
            let id = p.id.to_string();
            let live = sessions.get(&id);
            PeerView {
                name: p.name,
                short_id: p.id.short(),
                connected: live.is_some(),
                connected_since_ms: live.and_then(|l| i64::try_from(l.connected_at_ms).ok()),
                last_seen_ms: p.last_seen_ms,
                address: p.addresses.first().cloned().unwrap_or_default(),
                allowed_to_manage: p.may_manage,
                can_manage: live.is_some_and(|l| l.manageable),
                supports_remote: live.is_some_and(|l| l.supports_remote),
                id,
            }
        })
        .collect())
}

#[tauri::command(async)]
pub fn set_peer_manage(app: AppHandle, name: String, allowed: bool) -> Result<(), String> {
    apply_config(
        &app,
        ConfigChange::SetPeerManage {
            peer: name,
            allowed,
        },
    )
    .map(drop)
}

/// A remote call on a paired device. Errors keep their stable code so the UI
/// can tell "needs permission" from "offline".
#[tauri::command(async)]
pub fn remote_call(
    app: AppHandle,
    peer: String,
    call: RemoteCall,
) -> Result<RemoteReply, RemoteError> {
    // Not `Offline`: that reads as the other device being down.
    let mut client = host_client(&app).ok().flatten().ok_or_else(|| {
        RemoteError::new(
            RemoteErrorCode::Failed,
            "Relay is not running on this computer. Resume sync and try again.",
        )
    })?;
    client.remote(&peer, &call).map_err(|err| match err {
        relay_ipc::IpcError::Remote { code, message } => {
            RemoteError::new(RemoteErrorCode::parse(&code), message)
        }
        other => RemoteError::new(RemoteErrorCode::Failed, error_chain(&other)),
    })
}

/// Measure the connection to a connected peer (D48). Blocks for the test,
/// about ten seconds.
#[tauri::command(async)]
pub fn speed_test(app: AppHandle, peer: String) -> Result<SpeedReport, RemoteError> {
    let mut client = host_client(&app).ok().flatten().ok_or_else(|| {
        RemoteError::new(
            RemoteErrorCode::Failed,
            "Relay is not running on this computer. Resume sync and try again.",
        )
    })?;
    let params = SpeedTestParams {
        peer,
        duration_ms: None,
    };
    client.speed_test(&params).map_err(|err| match err {
        relay_ipc::IpcError::Remote { code, message } => {
            RemoteError::new(RemoteErrorCode::parse(&code), message)
        }
        other => RemoteError::new(RemoteErrorCode::Failed, error_chain(&other)),
    })
}

#[tauri::command(async)]
pub fn add_peer(
    app: AppHandle,
    name: String,
    device_id: String,
    address: String,
) -> Result<PeerView, String> {
    let id: DeviceId = device_id
        .trim()
        .parse()
        .map_err(|err: relay_core::CoreError| error_chain(&err))?;
    let address = address.trim().to_owned();
    if address.is_empty() {
        return Err("Address is required (for example 192.168.1.20:47321).".to_owned());
    }
    let applied = apply_config(
        &app,
        ConfigChange::AddPeer {
            peer: name.trim().to_owned(),
            id,
            addresses: vec![address.clone()],
        },
    )?;
    let ConfigApplied::Peer { device } = applied else {
        return Err(format!("unexpected result {applied:?}"));
    };
    Ok(PeerView {
        name: device.name,
        id: device.id.to_string(),
        short_id: device.id.short(),
        address,
        connected: false,
        connected_since_ms: None,
        last_seen_ms: None,
        allowed_to_manage: false,
        can_manage: false,
        supports_remote: false,
    })
}

#[tauri::command(async)]
pub fn remove_peer(app: AppHandle, name: String) -> Result<(), String> {
    apply_config(&app, ConfigChange::RemovePeer { peer: name }).map(drop)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairStartView {
    pub code: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PairStatusView {
    Idle,
    Waiting,
    Paired { peer_name: String, peer_id: String },
    Failed { reason: String },
    Expired,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairJoinView {
    pub peer_name: String,
    pub peer_id: String,
}

fn pairing_client(app: &AppHandle) -> Result<Client, String> {
    let state = app.state::<AppState>();
    if matches!(state.runner.state(), RunnerState::Paused) {
        return Err("Relay is paused; resume before pairing.".to_owned());
    }
    match Client::connect(&state.home) {
        Ok(Some(client)) => Ok(client),
        Ok(None) => Err("Relay is not running; resume sync and try again.".to_owned()),
        Err(err) => Err(error_chain(&err)),
    }
}

/// Prefer the running host's IPC so config writes do not `stop_join` the sync
/// thread. Returns `Ok(None)` only when nothing is listening on the socket.
pub(crate) fn host_client(app: &AppHandle) -> Result<Option<Client>, String> {
    let state = app.state::<AppState>();
    match Client::connect(&state.home) {
        Ok(client) => Ok(client),
        Err(err) => Err(error_chain(&err)),
    }
}

#[tauri::command(async)]
pub fn pair_start(
    app: AppHandle,
    share: Vec<String>,
    allow_manage: bool,
) -> Result<PairStartView, String> {
    let mut client = pairing_client(&app)?;
    let started = client
        .pair_start(&PairStartParams {
            share,
            allow_manage,
        })
        .map_err(|err| error_chain(&err))?;
    Ok(PairStartView {
        code: started.code,
        expires_at_ms: started.expires_at_ms,
    })
}

#[tauri::command(async)]
pub fn pair_status(app: AppHandle) -> Result<PairStatusView, String> {
    let mut client = pairing_client(&app)?;
    let status = client.pair_status().map_err(|err| error_chain(&err))?;
    Ok(match status {
        relay_ipc::PairStatus::Idle => PairStatusView::Idle,
        relay_ipc::PairStatus::Waiting => PairStatusView::Waiting,
        relay_ipc::PairStatus::Paired { peer_name, peer_id } => {
            PairStatusView::Paired { peer_name, peer_id }
        }
        relay_ipc::PairStatus::Failed { reason } => PairStatusView::Failed { reason },
        relay_ipc::PairStatus::Expired => PairStatusView::Expired,
    })
}

#[tauri::command(async)]
pub fn pair_join(
    app: AppHandle,
    code: String,
    addr: Option<String>,
    allow_manage: bool,
) -> Result<PairJoinView, String> {
    let mut client = pairing_client(&app)?;
    let addr = addr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let joined = client
        .pair_join(&PairJoinParams {
            code: code.trim().to_owned(),
            addr,
            allow_manage,
        })
        .map_err(|err| error_chain(&err))?;
    Ok(PairJoinView {
        peer_name: joined.peer_name,
        peer_id: joined.peer_id,
    })
}

#[tauri::command(async)]
pub fn pair_cancel(app: AppHandle) -> Result<(), String> {
    let mut client = pairing_client(&app)?;
    client.pair_cancel().map_err(|err| error_chain(&err))
}

#[tauri::command(async)]
pub fn list_spaces(app: AppHandle) -> Result<Vec<SpaceView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let spaces = engine.spaces().map_err(|err| error_chain(&err))?;
    let mounts = engine.mounts(None).map_err(|err| error_chain(&err))?;
    // Not `engine.status()`: its entry counts and object store walk grow
    // with the files synced, and this runs on every visit to Spaces/Peers.
    let health = engine.mount_health().map_err(|err| error_chain(&err))?;
    let peer_shares = engine.peer_shares().map_err(|err| error_chain(&err))?;

    let mut shared: HashMap<String, Vec<String>> = HashMap::new();
    for (peer, spaces) in peer_shares {
        for space in spaces {
            shared.entry(space).or_default().push(peer.clone());
        }
    }

    let mut mount_state: HashMap<(String, String), (Option<String>, String, bool)> = HashMap::new();
    for m in health {
        mount_state.insert(
            (m.space, m.mount),
            (
                m.path.map(|p| p.display().to_string()),
                m.last_error.unwrap_or(m.marker_state),
                m.marker_ok,
            ),
        );
    }

    let mut by_space: HashMap<String, Vec<MountView>> = HashMap::new();
    for (space, config) in mounts {
        let key = (space.name.clone(), config.mount.name.clone());
        let (path, state_label, attached) = mount_state.get(&key).cloned().unwrap_or_else(|| {
            (
                config.local_path.as_ref().map(|p| p.display().to_string()),
                if config.local_path.is_some() {
                    "OK".to_owned()
                } else {
                    "NO_PATH".to_owned()
                },
                config.local_path.is_some(),
            )
        });
        by_space.entry(space.name).or_default().push(MountView {
            name: config.mount.name,
            path,
            attached,
            state: state_label,
        });
    }

    Ok(spaces
        .into_iter()
        .map(|space| SpaceView {
            shared_with: shared.remove(&space.name).unwrap_or_default(),
            mounts: by_space.remove(&space.name).unwrap_or_default(),
            id: space.id.to_string(),
            name: space.name,
        })
        .collect())
}

#[tauri::command(async)]
pub fn create_space(app: AppHandle, name: String) -> Result<SpaceView, String> {
    let name = name.trim().to_owned();
    relay_core::validate_name(&name).map_err(|err| error_chain(&err))?;
    let exists = open_ro(&app.state::<AppState>().home)?
        .spaces()
        .map_err(|err| error_chain(&err))?
        .iter()
        .any(|space| space.name == name);
    if exists {
        return Err(format!("a space named {name:?} already exists"));
    }
    let applied = apply_config(&app, ConfigChange::CreateSpace { space: name })?;
    let ConfigApplied::Space { space } = applied else {
        return Err(format!("unexpected result {applied:?}"));
    };
    Ok(SpaceView {
        name: space.name,
        id: space.id.to_string(),
        mounts: Vec::new(),
        shared_with: Vec::new(),
    })
}

#[tauri::command(async)]
pub fn add_mount(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
) -> Result<MountView, String> {
    let applied = apply_config(
        &app,
        ConfigChange::AddMount {
            space,
            mount,
            path: PathBuf::from(path),
            includes: Vec::new(),
            excludes: Vec::new(),
        },
    )?;
    let ConfigApplied::Mount { mount, path } = applied else {
        return Err(format!("unexpected result {applied:?}"));
    };
    Ok(MountView {
        name: mount.name,
        attached: path.is_some(),
        path: path.map(|p| p.display().to_string()),
        state: "OK".to_owned(),
    })
}

#[tauri::command(async)]
pub fn remove_mount(app: AppHandle, space: String, mount: String) -> Result<(), String> {
    apply_config(&app, ConfigChange::RemoveMount { space, mount }).map(drop)
}

#[tauri::command(async)]
pub fn share(app: AppHandle, space: String, peer: String) -> Result<(), String> {
    apply_config(&app, ConfigChange::Share { space, peer }).map(drop)
}

#[tauri::command(async)]
pub fn unshare(app: AppHandle, space: String, peer: String) -> Result<(), String> {
    apply_config(&app, ConfigChange::Unshare { space, peer }).map(drop)
}

/// Apply a config change through the running host so live sessions survive.
/// With no host running, write it directly.
pub(crate) fn apply_config(app: &AppHandle, change: ConfigChange) -> Result<ConfigApplied, String> {
    if let Some(mut client) = host_client(app)? {
        return client.config(&change).map_err(|err| error_chain(&err));
    }
    write_directly(&app.state::<AppState>().home, &change)
}

/// Write a change when no host is running to apply it live. Retries briefly
/// while another process holds the writer lock.
fn write_directly(home: &std::path::Path, change: &ConfigChange) -> Result<ConfigApplied, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match Engine::open_for_config(home) {
            Ok(mut engine) => return engine.apply_config(change).map_err(|err| error_chain(&err)),
            Err(EngineError::Busy { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(err) => return Err(error_chain(&err)),
        }
    }
}

#[tauri::command(async)]
pub fn list_offers(app: AppHandle) -> Result<Vec<OfferView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let offers = engine.offers().map_err(|err| error_chain(&err))?;
    Ok(offers
        .into_iter()
        .map(|o| OfferView {
            peer: o.peer,
            peer_id: o.peer_id.to_string(),
            name: o.name,
            mounts: o
                .mounts
                .into_iter()
                .map(|m| OfferedMountView { name: m.name })
                .collect(),
        })
        .collect())
}

#[tauri::command(async)]
pub fn join_space(app: AppHandle, space: String, from_peer: String) -> Result<SpaceView, String> {
    let applied = apply_config(
        &app,
        ConfigChange::JoinSpace {
            space,
            from_peer: from_peer.clone(),
            wait_ms: 0,
        },
    )?;
    let ConfigApplied::Space { space } = applied else {
        return Err(format!("unexpected result {applied:?}"));
    };
    Ok(SpaceView {
        name: space.name,
        id: space.id.to_string(),
        mounts: Vec::new(),
        shared_with: vec![from_peer],
    })
}

#[tauri::command(async)]
pub fn delete_space(app: AppHandle, space: String) -> Result<(), String> {
    apply_config(&app, ConfigChange::DeleteSpace { space }).map(drop)
}

#[tauri::command(async)]
pub fn list_delete_holds(app: AppHandle) -> Result<Vec<DeleteHoldView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let holds = engine.delete_holds().map_err(|err| error_chain(&err))?;
    Ok(holds
        .into_iter()
        .map(|h| DeleteHoldView {
            peer: h.peer.to_string(),
            peer_name: h.peer_name,
            space: h.space,
            space_id: h.space_id.to_string(),
            mount: h.mount,
            mount_id: h.mount_id.to_string(),
            deletions: h.deletions as u32,
            live: h.live as u32,
            held_at_ms: h.held_at_ms,
            decision: h.decision,
        })
        .collect())
}

#[tauri::command(async)]
pub fn decide_delete_hold(
    app: AppHandle,
    space: String,
    mount: Option<String>,
    peer: Option<String>,
    decision: DeleteHoldDecision,
) -> Result<u32, String> {
    let applied = apply_config(
        &app,
        ConfigChange::DecideDeleteHold {
            space,
            mount,
            peer,
            decision,
        },
    )?;
    let ConfigApplied::Holds { decided } = applied else {
        return Err(format!("unexpected result {applied:?}"));
    };
    Ok(decided as u32)
}

#[tauri::command(async)]
pub fn list_conflicts(app: AppHandle) -> Result<Vec<ConflictView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let listed = engine.mounts(None).map_err(|err| error_chain(&err))?;
    let peers = engine.peers().map_err(|err| error_chain(&err))?;
    let local = engine.device();
    let mut names: HashMap<String, String> = peers
        .into_iter()
        .map(|p| (p.id.to_string(), p.name))
        .collect();
    names.insert(local.id.to_string(), local.name.clone());

    let conflicts = engine
        .conflict_infos(None)
        .map_err(|err| error_chain(&err))?;
    Ok(conflicts
        .into_iter()
        .map(|info| {
            let (space, mount) = listed
                .iter()
                .find(|(_, cfg)| cfg.mount.id == info.record.key.mount)
                .map(|(s, cfg)| (s.name.clone(), cfg.mount.name.clone()))
                .unwrap_or_else(|| (info.space.clone(), info.mount.clone()));
            let device_id = info.record.modified_by.to_string();
            ConflictView {
                path: info.record.key.path.to_string(),
                space,
                mount,
                device_short: info.record.modified_by.short(),
                device_name: names.get(&device_id).cloned(),
                device_id,
                class: match info.class {
                    ConflictClass::File { original } => ConflictClassView::File {
                        original: original.to_string(),
                    },
                    ConflictClass::Git { git_dir, is_ref } => ConflictClassView::Git {
                        git_dir: git_dir.to_string(),
                        is_ref,
                    },
                },
            }
        })
        .collect())
}

#[tauri::command(async)]
pub fn resolve_conflict(
    app: AppHandle,
    space: String,
    mount: String,
    copy_path: String,
    keep: String,
) -> Result<ResolveReportView, String> {
    let state = app.state::<AppState>();
    let path = copy_path
        .parse()
        .map_err(|err: relay_core::CoreError| error_chain(&err))?;
    let resolution = match keep.as_str() {
        "current" => Resolution::KeepCurrent,
        "copy" => Resolution::UseCopy,
        other => return Err(format!("keep must be current or copy, not {other:?}")),
    };
    let report = engine_resolve_conflict(&state.home, &space, &mount, &path, resolution)
        .map_err(|err| error_chain(&err))?;
    Ok(ResolveReportView {
        space: report.space,
        mount: report.mount,
        copy: report.copy.to_string(),
        original: report.original.to_string(),
        resolution: match report.resolution {
            Resolution::KeepCurrent => "current".into(),
            Resolution::UseCopy => "copy".into(),
        },
        scanned: report.scanned,
    })
}

#[tauri::command(async)]
pub fn resolve_git_conflicts(
    app: AppHandle,
    space: String,
    mount: String,
    git_dir: String,
    include_branches: bool,
) -> Result<GitResolveReportView, String> {
    let state = app.state::<AppState>();
    let path = git_dir
        .parse()
        .map_err(|err: relay_core::CoreError| error_chain(&err))?;
    let report = engine_resolve_git(&state.home, &space, &mount, &path, include_branches)
        .map_err(|err| error_chain(&err))?;
    Ok(GitResolveReportView {
        space: report.space,
        mount: report.mount,
        git_dir: report.git_dir.to_string(),
        deleted: report.deleted.iter().map(ToString::to_string).collect(),
        kept: report.kept.iter().map(ToString::to_string).collect(),
        scanned: report.scanned,
    })
}

#[tauri::command]
pub fn get_activity(app: AppHandle) -> Result<Vec<crate::runner::ActivityItem>, String> {
    let state = app.state::<AppState>();
    Ok(state.runner.activity())
}

#[tauri::command(async)]
pub fn pause_sync(app: AppHandle) -> Result<RunnerState, String> {
    let state = app.state::<AppState>();
    state.runner.pause(&app).map_err(anyhow_chain)?;
    Ok(state.runner.state())
}

#[tauri::command(async)]
pub fn resume_sync(app: AppHandle) -> Result<RunnerState, String> {
    let state = app.state::<AppState>();
    state.runner.resume(&app).map_err(anyhow_chain)?;
    Ok(state.runner.state())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateInfo, String> {
    updates::check_and_maybe_install(&app, true).await
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn pending_update() -> Option<updates::UpdateAvailable> {
    updates::pending_update()
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<UpdateInfo, String> {
    updates::install(&app).await
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn releases_url() -> Option<String> {
    updates::releases_page()
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn restart_app(app: AppHandle) {
    updates::restart_now(&app);
}

#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    op_id: u64,
    configured: bool,
    available: bool,
    version: Option<String>,
    notes: Option<String>,
    message: String,
    installing: bool,
    restart_at_ms: Option<u64>,
    error: bool,
}

#[cfg(target_os = "android")]
fn android_updates_unavailable(op_id: u64) -> UpdateInfo {
    UpdateInfo {
        op_id,
        configured: false,
        available: false,
        version: None,
        notes: None,
        message: "Updates are not available on Android.".to_owned(),
        installing: false,
        restart_at_ms: None,
        error: false,
    }
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn check_for_updates() -> Result<UpdateInfo, String> {
    Ok(android_updates_unavailable(0))
}

#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailable {
    version: String,
    notes: String,
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn pending_update() -> Option<UpdateAvailable> {
    None
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn install_update() -> Result<UpdateInfo, String> {
    Err("Updates are not available on Android.".to_owned())
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn releases_url() -> Option<String> {
    None
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn restart_app() {}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> Result<settings::Settings, String> {
    Ok(settings::load(&app))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub start_at_login: Option<bool>,
    pub auto_update: Option<bool>,
}

#[tauri::command(async)]
pub fn set_settings(app: AppHandle, patch: SettingsPatch) -> Result<settings::Settings, String> {
    if let Some(value) = patch.start_at_login {
        settings::set_start_at_login(&app, value).map_err(anyhow_chain)?;
        apply_autostart(&app, value);
    }
    if let Some(value) = patch.auto_update {
        settings::set_auto_update(&app, value).map_err(anyhow_chain)?;
    }
    Ok(settings::load(&app))
}

#[tauri::command]
pub fn open_logs_folder(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let logs = state.home.join("logs");
    std::fs::create_dir_all(&logs).map_err(|err| error_chain(&err))?;
    app.opener()
        .open_path(logs.to_string_lossy().to_string(), None::<&str>)
        .map_err(|err| anyhow_chain(err.into()))
}

/// Whether this Mac lets Relay read every folder. `None` off macOS.
#[tauri::command]
pub fn full_disk_access() -> Option<bool> {
    crate::privacy::full_disk_access()
}

#[tauri::command]
pub fn open_full_disk_access(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        app.opener()
            .open_url(crate::privacy::FULL_DISK_ACCESS_PANE, None::<&str>)
            .map_err(|err| anyhow_chain(err.into()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Err("Full Disk Access is a macOS setting".to_owned())
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command(async)]
pub fn cli_status() -> Result<CliStatus, String> {
    Ok(sidecar::cli_status())
}

#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    sidecar_path: Option<String>,
    install_path: Option<String>,
    on_path: bool,
    hint: Option<String>,
    detected_shell: Option<String>,
    shell_hints: Vec<String>,
    path_configured: bool,
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn cli_status() -> Result<CliStatus, String> {
    Ok(CliStatus {
        sidecar_path: None,
        install_path: None,
        on_path: false,
        hint: Some("The command-line tool is not available on Android.".to_owned()),
        detected_shell: None,
        shell_hints: Vec::new(),
        path_configured: false,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command(async)]
pub fn install_cli(app: AppHandle, shell: Option<String>) -> Result<CliInstallResult, String> {
    let shell = match shell.as_deref() {
        None => None,
        Some(value) => {
            Some(ShellKind::parse(value).ok_or_else(|| format!("unsupported shell: {value}"))?)
        }
    };
    let result = sidecar::install_cli(shell, true).map_err(anyhow_chain)?;
    let _ = settings::set_cli_install_attempted(&app);
    Ok(result)
}

#[cfg(target_os = "android")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliInstallResult {
    path: String,
    on_path: bool,
    hint: Option<String>,
    message: String,
    detected_shell: Option<String>,
    path_configured: bool,
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn install_cli() -> Result<CliInstallResult, String> {
    Err("The command-line tool is not available on Android.".to_owned())
}

pub fn apply_autostart(app: &AppHandle, enable: bool) {
    #[cfg(not(target_os = "android"))]
    {
        let manager = app.autolaunch();
        let result = if enable {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(err) = result {
            log::warn!("autostart: {err}");
        }
    }
    #[cfg(target_os = "android")]
    let _ = (app, enable);
}

pub fn maybe_install_cli(app: &AppHandle) {
    #[cfg(not(target_os = "android"))]
    {
        if settings::cli_install_attempted(app) {
            return;
        }
        let _ = settings::set_cli_install_attempted(app);
        // Symlink only — do not rewrite shell rc files during automatic install.
        if let Err(err) = sidecar::install_cli(None, false) {
            log::info!("automatic CLI install skipped: {err:#}");
        }
    }
    #[cfg(target_os = "android")]
    let _ = app;
}
