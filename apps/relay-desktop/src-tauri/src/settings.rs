use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_store::StoreExt;

const STORE_FILE: &str = "settings.json";
const KEY_START_AT_LOGIN: &str = "start_at_login";
const KEY_AUTO_UPDATE: &str = "auto_update";
const KEY_PAUSED: &str = "paused";
const KEY_DEFAULTS_APPLIED: &str = "defaults_applied";
const KEY_CLI_INSTALL_ATTEMPTED: &str = "cli_install_attempted";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub start_at_login: bool,
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            start_at_login: true,
            auto_update: true,
        }
    }
}

fn store(app: &AppHandle) -> anyhow::Result<std::sync::Arc<tauri_plugin_store::Store<tauri::Wry>>> {
    app.store(STORE_FILE)
        .map_err(|err| anyhow::anyhow!("{err}"))
}

fn get_bool(app: &AppHandle, key: &str, default: bool) -> bool {
    let Ok(store) = store(app) else {
        return default;
    };
    store.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn set_bool(app: &AppHandle, key: &str, value: bool) -> anyhow::Result<()> {
    let store = store(app)?;
    store.set(key, serde_json::Value::Bool(value));
    store.save().map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

pub fn load(app: &AppHandle) -> Settings {
    Settings {
        start_at_login: get_bool(app, KEY_START_AT_LOGIN, true),
        auto_update: get_bool(app, KEY_AUTO_UPDATE, true),
    }
}

pub fn paused(app: &AppHandle) -> bool {
    get_bool(app, KEY_PAUSED, false)
}

pub fn set_paused(app: &AppHandle, value: bool) -> anyhow::Result<()> {
    set_bool(app, KEY_PAUSED, value)
}

pub fn cli_install_attempted(app: &AppHandle) -> bool {
    get_bool(app, KEY_CLI_INSTALL_ATTEMPTED, false)
}

pub fn set_cli_install_attempted(app: &AppHandle) -> anyhow::Result<()> {
    set_bool(app, KEY_CLI_INSTALL_ATTEMPTED, true)
}

pub fn apply_first_run_defaults(app: &AppHandle) -> anyhow::Result<Settings> {
    if get_bool(app, KEY_DEFAULTS_APPLIED, false) {
        return Ok(load(app));
    }
    set_bool(app, KEY_START_AT_LOGIN, true)?;
    set_bool(app, KEY_AUTO_UPDATE, true)?;
    set_bool(app, KEY_PAUSED, false)?;
    set_bool(app, KEY_DEFAULTS_APPLIED, true)?;
    Ok(Settings::default())
}

pub fn set_start_at_login(app: &AppHandle, value: bool) -> anyhow::Result<()> {
    set_bool(app, KEY_START_AT_LOGIN, value)
}

pub fn set_auto_update(app: &AppHandle, value: bool) -> anyhow::Result<()> {
    set_bool(app, KEY_AUTO_UPDATE, value)
}
