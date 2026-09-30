use serde::Serialize;
use tauri::AppHandle;
use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

use crate::AppState;
use crate::error::anyhow_chain;
use crate::settings;

const PLACEHOLDER_PUBKEY: &str = "REPLACE_WITH_TAURI_UPDATER_PUBKEY";
const PLACEHOLDER_ENDPOINT: &str = "OWNER/REPO";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub configured: bool,
    pub available: bool,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub message: String,
    pub installing: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAvailable {
    pub version: String,
    pub notes: String,
}

pub fn updater_configured() -> bool {
    let config = include_str!("../tauri.conf.json");
    !config.contains(PLACEHOLDER_PUBKEY) && !config.contains(PLACEHOLDER_ENDPOINT)
}

pub async fn check_and_maybe_install(
    app: &AppHandle,
    interactive: bool,
) -> Result<UpdateInfo, String> {
    if !updater_configured() {
        return Ok(UpdateInfo {
            configured: false,
            available: false,
            version: None,
            notes: None,
            message: "Updates are not configured yet (placeholder GitHub URL and pubkey)."
                .to_owned(),
            installing: false,
        });
    }

    let updater = match app.updater() {
        Ok(u) => u,
        Err(err) => {
            return Ok(UpdateInfo {
                configured: false,
                available: false,
                version: None,
                notes: None,
                message: format!("Updates are not configured: {}", anyhow_chain(err.into())),
                installing: false,
            });
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
                return Ok(UpdateInfo {
                    configured: false,
                    available: false,
                    version: None,
                    notes: None,
                    message: "Updates are not configured yet.".to_owned(),
                    installing: false,
                });
            }
            return Err(anyhow_chain(err.into()));
        }
    };

    let Some(update) = update else {
        return Ok(UpdateInfo {
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
        });
    };

    let notes = update.body.clone().unwrap_or_default();
    let version = update.version.clone();
    let auto = settings::load(app).auto_update;

    if auto {
        if let Some(state) = app.try_state::<AppState>() {
            state.runner.stop_join();
        }
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|err| anyhow_chain(err.into()))?;
        app.request_restart();
    }

    let payload = UpdateAvailable {
        version: version.clone(),
        notes: notes.clone(),
    };
    let _ = app.emit_update(&payload);

    Ok(UpdateInfo {
        configured: true,
        available: true,
        version: Some(version.clone()),
        notes: Some(notes),
        message: format!("Relay {version} is available."),
        installing: false,
    })
}

pub async fn install(app: &AppHandle) -> Result<UpdateInfo, String> {
    if !updater_configured() {
        return Err("Updates are not configured yet.".to_owned());
    }
    let updater = app.updater().map_err(|err| anyhow_chain(err.into()))?;
    let update = updater
        .check()
        .await
        .map_err(|err| anyhow_chain(err.into()))?
        .ok_or_else(|| "No update available.".to_owned())?;
    if let Some(state) = app.try_state::<AppState>() {
        state.runner.stop_join();
    }
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| anyhow_chain(err.into()))?;
    app.request_restart();
    Ok(UpdateInfo {
        configured: true,
        available: true,
        version: Some(update.version.clone()),
        notes: update.body.clone(),
        message: format!("Installing Relay {}…", update.version),
        installing: true,
    })
}

trait EmitUpdate {
    fn emit_update(&self, payload: &UpdateAvailable) -> tauri::Result<()>;
}

impl EmitUpdate for AppHandle {
    fn emit_update(&self, payload: &UpdateAvailable) -> tauri::Result<()> {
        use tauri::Emitter;
        self.emit("relay://update-available", payload)
    }
}

pub fn spawn_periodic_checks(app: &AppHandle) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("relay-updater".to_owned())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            loop {
                let handle = app.clone();
                let _ = tauri::async_runtime::block_on(async move {
                    check_and_maybe_install(&handle, false).await
                });
                std::thread::sleep(std::time::Duration::from_secs(30 * 60));
            }
        })
        .ok();
}
