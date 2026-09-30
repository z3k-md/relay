use std::collections::HashMap;
use std::path::PathBuf;

use relay_core::DeviceId;
use relay_engine::{Engine, EngineError};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;

use crate::error::{anyhow_chain, error_chain};
use crate::runner::RunnerState;
use crate::sidecar::{self, CliInstallResult, CliStatus};
use crate::updates::{self, UpdateInfo};
use crate::{AppState, settings};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub initialized: bool,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub suggested_name: String,
    pub runner: RunnerState,
    pub version: String,
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
#[serde(rename_all = "camelCase")]
pub struct ConflictView {
    pub path: String,
    pub space: String,
    pub mount: String,
    pub device_id: String,
    pub device_short: String,
    pub device_name: Option<String>,
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

fn open_ro(home: &std::path::Path) -> Result<Engine, String> {
    Engine::open_read_only(home).map_err(|err| error_chain(&err))
}

fn with_write<T>(
    app: &AppHandle,
    f: impl FnOnce(&mut Engine) -> anyhow::Result<T>,
) -> Result<T, String> {
    let state = app.state::<AppState>();
    state.runner.stop_join();
    let home = state.home.clone();
    let result = (|| {
        let mut engine = Engine::open_for_config(&home)?;
        f(&mut engine)
    })();
    state.runner.start(app);
    result.map_err(anyhow_chain)
}

#[tauri::command]
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
            peer_count: 0,
            connected_peers: 0,
            space_count: 0,
            mount_count: 0,
        }),
        Err(err) => Err(error_chain(&err)),
    }
}

#[tauri::command]
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

#[tauri::command]
pub fn list_peers(app: AppHandle) -> Result<Vec<PeerView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let connected = state.runner.connected_peers();
    let peers = engine.peers().map_err(|err| error_chain(&err))?;
    Ok(peers
        .into_iter()
        .map(|p| {
            let id = p.id.to_string();
            PeerView {
                name: p.name,
                short_id: p.id.short(),
                connected: connected.contains(&id),
                address: p.addresses.first().cloned().unwrap_or_default(),
                id,
            }
        })
        .collect())
}

#[tauri::command]
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
    with_write(&app, |engine| {
        let peer = engine.add_peer(name.trim(), id, std::slice::from_ref(&address))?;
        Ok(PeerView {
            name: peer.name,
            id: peer.id.to_string(),
            short_id: peer.id.short(),
            address,
            connected: false,
        })
    })
}

#[tauri::command]
pub fn remove_peer(app: AppHandle, name: String) -> Result<(), String> {
    with_write(&app, |engine| {
        engine.remove_peer(&name)?;
        Ok(())
    })
}

#[tauri::command]
pub fn list_spaces(app: AppHandle) -> Result<Vec<SpaceView>, String> {
    let state = app.state::<AppState>();
    let engine = open_ro(&state.home)?;
    let spaces = engine.spaces().map_err(|err| error_chain(&err))?;
    let mounts = engine.mounts(None).map_err(|err| error_chain(&err))?;
    let status = engine.status().map_err(|err| error_chain(&err))?;

    let mut shared: HashMap<String, Vec<String>> = HashMap::new();
    for peer in status.peers {
        for space in peer.spaces {
            shared
                .entry(space.space)
                .or_default()
                .push(peer.name.clone());
        }
    }

    let mut mount_state: HashMap<(String, String), (Option<String>, String, bool)> = HashMap::new();
    for m in status.mounts {
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

#[tauri::command]
pub fn create_space(app: AppHandle, name: String) -> Result<SpaceView, String> {
    let name = name.trim().to_owned();
    with_write(&app, |engine| {
        let space = engine.create_space(&name)?;
        Ok(SpaceView {
            name: space.name,
            id: space.id.to_string(),
            mounts: Vec::new(),
            shared_with: Vec::new(),
        })
    })
}

#[tauri::command]
pub fn add_mount(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
) -> Result<MountView, String> {
    let path = PathBuf::from(path);
    with_write(&app, |engine| {
        let config = engine.add_mount(&space, &mount, &path, &[], &[])?;
        Ok(MountView {
            name: config.mount.name,
            path: config.local_path.as_ref().map(|p| p.display().to_string()),
            attached: config.local_path.is_some(),
            state: "OK".to_owned(),
        })
    })
}

#[tauri::command]
pub fn share(app: AppHandle, space: String, peer: String) -> Result<(), String> {
    with_write(&app, |engine| {
        engine.share(&space, &peer)?;
        Ok(())
    })
}

#[tauri::command]
pub fn unshare(app: AppHandle, space: String, peer: String) -> Result<(), String> {
    with_write(&app, |engine| {
        engine.unshare(&space, &peer)?;
        Ok(())
    })
}

#[tauri::command]
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

#[tauri::command]
pub fn join_space(app: AppHandle, space: String, from_peer: String) -> Result<SpaceView, String> {
    with_write(&app, |engine| {
        let created = engine.join_space(&space, &from_peer)?;
        Ok(SpaceView {
            name: created.name,
            id: created.id.to_string(),
            mounts: Vec::new(),
            shared_with: vec![from_peer.clone()],
        })
    })
}

#[tauri::command]
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

    let conflicts = engine.conflicts(None).map_err(|err| error_chain(&err))?;
    Ok(conflicts
        .into_iter()
        .map(|entry| {
            let (space, mount) = listed
                .iter()
                .find(|(_, cfg)| cfg.mount.id == entry.key.mount)
                .map(|(s, cfg)| (s.name.clone(), cfg.mount.name.clone()))
                .unwrap_or_else(|| ("?".to_owned(), "?".to_owned()));
            let device_id = entry.modified_by.to_string();
            ConflictView {
                path: entry.key.path.to_string(),
                space,
                mount,
                device_short: entry.modified_by.short(),
                device_name: names.get(&device_id).cloned(),
                device_id,
            }
        })
        .collect())
}

#[tauri::command]
pub fn get_activity(app: AppHandle) -> Result<Vec<crate::runner::ActivityItem>, String> {
    let state = app.state::<AppState>();
    Ok(state.runner.activity())
}

#[tauri::command]
pub fn pause_sync(app: AppHandle) -> Result<RunnerState, String> {
    let state = app.state::<AppState>();
    state.runner.pause(&app).map_err(anyhow_chain)?;
    Ok(state.runner.state())
}

#[tauri::command]
pub fn resume_sync(app: AppHandle) -> Result<RunnerState, String> {
    let state = app.state::<AppState>();
    state.runner.resume(&app).map_err(anyhow_chain)?;
    Ok(state.runner.state())
}

#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> Result<UpdateInfo, String> {
    updates::check_and_maybe_install(&app, true).await
}

#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<UpdateInfo, String> {
    updates::install(&app).await
}

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

#[tauri::command]
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

#[tauri::command]
pub fn cli_status() -> Result<CliStatus, String> {
    Ok(sidecar::cli_status())
}

#[tauri::command]
pub fn install_cli(app: AppHandle) -> Result<CliInstallResult, String> {
    let result = sidecar::install_cli().map_err(anyhow_chain)?;
    let _ = settings::set_cli_install_attempted(&app);
    Ok(result)
}

pub fn apply_autostart(app: &AppHandle, enable: bool) {
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

pub fn maybe_install_cli(app: &AppHandle) {
    if settings::cli_install_attempted(app) {
        return;
    }
    let _ = settings::set_cli_install_attempted(app);
    if let Err(err) = sidecar::install_cli() {
        log::info!("automatic CLI install skipped: {err:#}");
    }
}
