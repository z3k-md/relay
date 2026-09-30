use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

use crate::AppState;
use crate::error::anyhow_chain;
use crate::settings;

const PLACEHOLDER_PUBKEY: &str = "REPLACE_WITH_TAURI_UPDATER_PUBKEY";
const PLACEHOLDER_ENDPOINT: &str = "OWNER/REPO";
const RESTART_DELAY: Duration = Duration::from_secs(5);
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(100);

static UPDATE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static RESTART_PENDING: AtomicBool = AtomicBool::new(false);
static NEXT_OP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub op_id: u64,
    pub configured: bool,
    pub available: bool,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub message: String,
    pub installing: bool,
    pub restart_at_ms: Option<u64>,
    pub error: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailable {
    pub version: String,
    pub notes: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum UpdateProgress {
    Checking {
        op_id: u64,
    },
    Downloading {
        op_id: u64,
        downloaded: u64,
        total: Option<u64>,
    },
    Installing {
        op_id: u64,
    },
    Ready {
        op_id: u64,
        version: String,
        restart_at_ms: u64,
    },
    Finished {
        op_id: u64,
        message: String,
        error: bool,
    },
}

struct UpdateGuard {
    release: bool,
}

impl UpdateGuard {
    fn try_acquire() -> Option<Self> {
        if UPDATE_IN_PROGRESS.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Self { release: true })
        }
    }

    fn hold_until_restart(&mut self) {
        self.release = false;
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        if self.release {
            UPDATE_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
}

pub fn updater_configured() -> bool {
    let config = include_str!("../tauri.conf.json");
    !config.contains(PLACEHOLDER_PUBKEY) && !config.contains(PLACEHOLDER_ENDPOINT)
}

pub async fn check_and_maybe_install(
    app: &AppHandle,
    interactive: bool,
) -> Result<UpdateInfo, String> {
    let op_id = if interactive { next_op() } else { 0 };

    if !updater_configured() {
        return Ok(finish_interactive(
            app,
            interactive,
            unconfigured(
                op_id,
                "Updates are not configured yet (placeholder GitHub URL and pubkey).",
            ),
        ));
    }

    let Some(mut guard) = UpdateGuard::try_acquire() else {
        let info = if interactive {
            failed(op_id, "An update is already in progress.")
        } else {
            idle_message(op_id, "An update is already in progress.")
        };
        return Ok(finish_interactive(app, interactive, info));
    };

    if interactive {
        emit_progress(app, &UpdateProgress::Checking { op_id });
    }

    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(err) => {
            return Ok(finish_interactive(
                app,
                interactive,
                unconfigured(
                    op_id,
                    format!("Updates are not configured: {}", anyhow_chain(err.into())),
                ),
            ));
        }
    };

    let update = match updater.check().await {
        Ok(found) => found,
        Err(err) => {
            let text = err.to_string();
            if text.contains(PLACEHOLDER_PUBKEY)
                || text.contains(PLACEHOLDER_ENDPOINT)
                || text.to_ascii_lowercase().contains("public key")
            {
                return Ok(finish_interactive(
                    app,
                    interactive,
                    unconfigured(op_id, "Updates are not configured yet."),
                ));
            }
            return Ok(finish_interactive(
                app,
                interactive,
                failed(op_id, anyhow_chain(err.into())),
            ));
        }
    };

    let Some(update) = update else {
        return Ok(finish_interactive(
            app,
            interactive,
            UpdateInfo {
                op_id,
                configured: true,
                available: false,
                version: None,
                notes: None,
                message: if interactive {
                    "You're on the latest version.".to_owned()
                } else {
                    "No update available.".to_owned()
                },
                installing: false,
                restart_at_ms: None,
                error: false,
            },
        ));
    };

    let notes = update.body.clone().unwrap_or_default();
    let version = update.version.clone();
    let auto = settings::load(app).auto_update;

    if interactive || auto {
        let info = install_downloaded(app, update, op_id, interactive).await?;
        if info.restart_at_ms.is_some() {
            guard.hold_until_restart();
        }
        return Ok(info);
    }

    let payload = UpdateAvailable {
        version: version.clone(),
        notes: notes.clone(),
    };
    let _ = app.emit_update(&payload);

    Ok(finish_interactive(
        app,
        interactive,
        UpdateInfo {
            op_id,
            configured: true,
            available: true,
            version: Some(version.clone()),
            notes: Some(notes),
            message: format!("Relay {version} is available."),
            installing: false,
            restart_at_ms: None,
            error: false,
        },
    ))
}

pub async fn install(app: &AppHandle) -> Result<UpdateInfo, String> {
    let op_id = next_op();
    if !updater_configured() {
        return Ok(finish_interactive(
            app,
            true,
            failed(op_id, "Updates are not configured yet."),
        ));
    }
    let Some(mut guard) = UpdateGuard::try_acquire() else {
        return Ok(finish_interactive(
            app,
            true,
            failed(op_id, "An update is already in progress."),
        ));
    };
    emit_progress(app, &UpdateProgress::Checking { op_id });
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(err) => {
            return Ok(finish_interactive(
                app,
                true,
                failed(op_id, anyhow_chain(err.into())),
            ));
        }
    };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            return Ok(finish_interactive(
                app,
                true,
                failed(op_id, "No update available."),
            ));
        }
        Err(err) => {
            return Ok(finish_interactive(
                app,
                true,
                failed(op_id, anyhow_chain(err.into())),
            ));
        }
    };
    let info = install_downloaded(app, update, op_id, true).await?;
    if info.restart_at_ms.is_some() {
        guard.hold_until_restart();
    }
    Ok(info)
}

pub fn restart_now(app: &AppHandle) {
    if RESTART_PENDING.swap(false, Ordering::SeqCst) {
        app.request_restart();
    }
}

async fn install_downloaded(
    app: &AppHandle,
    update: tauri_plugin_updater::Update,
    op_id: u64,
    prompt_restart: bool,
) -> Result<UpdateInfo, String> {
    if let Some(state) = app.try_state::<AppState>() {
        state.runner.stop_join();
    }
    let version = update.version.clone();
    let notes = update.body.clone();

    if prompt_restart {
        emit_progress(
            app,
            &UpdateProgress::Downloading {
                op_id,
                downloaded: 0,
                total: None,
            },
        );
        if let Err(err) = download_with_progress(app, &update, op_id).await {
            return Ok(finish_interactive(app, true, failed(op_id, err)));
        }
        let restart_at_ms = schedule_restart(app);
        emit_progress(
            app,
            &UpdateProgress::Ready {
                op_id,
                version: version.clone(),
                restart_at_ms,
            },
        );
        return Ok(UpdateInfo {
            op_id,
            configured: true,
            available: true,
            version: Some(version.clone()),
            notes,
            message: format!("Relay {version} installed. Restarting shortly."),
            installing: true,
            restart_at_ms: Some(restart_at_ms),
            error: false,
        });
    }

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| anyhow_chain(err.into()))?;
    app.request_restart();
    Ok(UpdateInfo {
        op_id,
        configured: true,
        available: true,
        version: Some(version.clone()),
        notes,
        message: format!("Installing Relay {version}…"),
        installing: true,
        restart_at_ms: None,
        error: false,
    })
}

async fn download_with_progress(
    app: &AppHandle,
    update: &tauri_plugin_updater::Update,
    op_id: u64,
) -> Result<(), String> {
    let progress_app = app.clone();
    let done_app = app.clone();
    let mut downloaded: u64 = 0;
    let mut last_emit = Instant::now()
        .checked_sub(PROGRESS_EMIT_INTERVAL)
        .unwrap_or_else(Instant::now);

    update
        .download_and_install(
            move |chunk, total| {
                downloaded = downloaded.saturating_add(chunk as u64);
                let now = Instant::now();
                let complete = total.is_some_and(|total| downloaded >= total);
                if complete || now.saturating_duration_since(last_emit) >= PROGRESS_EMIT_INTERVAL {
                    last_emit = now;
                    emit_progress(
                        &progress_app,
                        &UpdateProgress::Downloading {
                            op_id,
                            downloaded,
                            total,
                        },
                    );
                }
            },
            move || {
                emit_progress(&done_app, &UpdateProgress::Installing { op_id });
            },
        )
        .await
        .map_err(|err| anyhow_chain(err.into()))
}

fn schedule_restart(app: &AppHandle) -> u64 {
    let restart_at = SystemTime::now() + RESTART_DELAY;
    let restart_at_ms = system_time_ms(restart_at);
    RESTART_PENDING.store(true, Ordering::SeqCst);
    let app = app.clone();
    let restart_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("relay-update-restart".to_owned())
        .spawn(move || {
            loop {
                if !RESTART_PENDING.load(Ordering::SeqCst) {
                    return;
                }
                let now = SystemTime::now();
                if now >= restart_at {
                    break;
                }
                let remaining = restart_at.duration_since(now).unwrap_or(Duration::ZERO);
                std::thread::sleep(remaining.min(Duration::from_millis(200)));
            }
            if RESTART_PENDING.swap(false, Ordering::SeqCst) {
                restart_app.request_restart();
            }
        });
    if spawned.is_err() {
        RESTART_PENDING.store(false, Ordering::SeqCst);
        app.request_restart();
    }
    restart_at_ms
}

fn finish_interactive(app: &AppHandle, interactive: bool, info: UpdateInfo) -> UpdateInfo {
    if interactive && info.restart_at_ms.is_none() {
        emit_progress(
            app,
            &UpdateProgress::Finished {
                op_id: info.op_id,
                message: info.message.clone(),
                error: info.error,
            },
        );
    }
    info
}

fn next_op() -> u64 {
    NEXT_OP.fetch_add(1, Ordering::Relaxed)
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn emit_progress(app: &AppHandle, progress: &UpdateProgress) {
    let _ = app.emit("relay://update-progress", progress);
}

fn unconfigured(op_id: u64, message: impl Into<String>) -> UpdateInfo {
    UpdateInfo {
        op_id,
        configured: false,
        available: false,
        version: None,
        notes: None,
        message: message.into(),
        installing: false,
        restart_at_ms: None,
        error: false,
    }
}

fn failed(op_id: u64, message: impl Into<String>) -> UpdateInfo {
    UpdateInfo {
        op_id,
        configured: true,
        available: false,
        version: None,
        notes: None,
        message: message.into(),
        installing: false,
        restart_at_ms: None,
        error: true,
    }
}

fn idle_message(op_id: u64, message: impl Into<String>) -> UpdateInfo {
    UpdateInfo {
        op_id,
        configured: true,
        available: false,
        version: None,
        notes: None,
        message: message.into(),
        installing: false,
        restart_at_ms: None,
        error: false,
    }
}

trait EmitUpdate {
    fn emit_update(&self, payload: &UpdateAvailable) -> tauri::Result<()>;
}

impl EmitUpdate for AppHandle {
    fn emit_update(&self, payload: &UpdateAvailable) -> tauri::Result<()> {
        self.emit("relay://update-available", payload)
    }
}

pub fn spawn_periodic_checks(app: &AppHandle) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("relay-updater".to_owned())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(20));
            loop {
                let handle = app.clone();
                let _ = tauri::async_runtime::block_on(async move {
                    check_and_maybe_install(&handle, false).await
                });
                std::thread::sleep(Duration::from_secs(30 * 60));
            }
        })
        .ok();
}
