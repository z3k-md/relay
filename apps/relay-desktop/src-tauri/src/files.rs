//! Files view: this device's synced folders, with online-only files that
//! download when opened (remote explorer Stage 1).

use relay_core::ConfigChange;
use relay_engine::{CopyState, Engine, FolderView};
use relay_ipc::IpcError;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::AppState;
use crate::commands::{apply_config, host_client, open_ro};
use crate::error::error_chain;

/// One folder of a mount. `path` `""` is the mount itself.
#[tauri::command(async)]
pub fn list_folder(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
) -> Result<FolderView, String> {
    open_ro(&app.state::<AppState>().home)?
        .list_folder(&space, &mount, &path)
        .map_err(|err| error_chain(&err))
}

/// Download one online-only file. Returns once it is on disk; progress shows
/// in the transfers the app already receives.
#[tauri::command(async)]
pub fn download_file(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
) -> Result<(), String> {
    fetch(&app, &space, &mount, &path)
}

/// Download a file if it is not here yet, then open it with its app.
#[tauri::command(async)]
pub fn open_file(app: AppHandle, space: String, mount: String, path: String) -> Result<(), String> {
    let home = app.state::<AppState>().home.clone();
    let here = open_ro(&home)?
        .list_folder(&space, &mount, parent_of(&path))
        .map_err(|err| error_chain(&err))?
        .entries
        .into_iter()
        .find(|row| row.path == path)
        .is_some_and(|row| row.state == CopyState::Local);
    if !here {
        fetch(&app, &space, &mount, &path)?;
    }
    let target = open_ro(&home)?
        .local_file_path(&space, &mount, &path)
        .map_err(|err| error_chain(&err))?;
    app.opener()
        .open_path(target.to_string_lossy().to_string(), None::<&str>)
        .map_err(|err| err.to_string())
}

/// Free this device's copy of a file, or of every downloaded online-only
/// file under a folder. Returns how many were freed.
#[tauri::command(async)]
pub fn free_up_space(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
) -> Result<usize, String> {
    if let Some(mut client) = host_client(&app)? {
        return client.evict(&space, &mount, &path).map_err(friendly);
    }
    Engine::open_for_config(&app.state::<AppState>().home)
        .and_then(|mut engine| engine.evict(&space, &mount, &path))
        .map_err(|err| error_chain(&err))
}

/// "Always keep on this device" (`full`), "Online only" (`demand`), or
/// `None` to follow the enclosing folder.
#[tauri::command(async)]
pub fn set_folder_mode(
    app: AppHandle,
    space: String,
    mount: String,
    path: String,
    mode: Option<String>,
) -> Result<(), String> {
    apply_config(
        &app,
        ConfigChange::SetFolderMode {
            space,
            mount,
            path,
            mode,
        },
    )
    .map(drop)
}

fn fetch(app: &AppHandle, space: &str, mount: &str, path: &str) -> Result<(), String> {
    if let Some(mut client) = host_client(app)? {
        return client.fetch(space, mount, path).map_err(friendly);
    }
    // No host: only the local store or the mailbox can supply the bytes.
    Engine::open_for_config(&app.state::<AppState>().home)
        .and_then(|mut engine| engine.fetch_path(space, mount, path))
        .map_err(|err| error_chain(&err))
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn friendly(err: IpcError) -> String {
    match err {
        IpcError::Remote { code, .. } if code == "unavailable" => {
            "No connected device has this file right now. It can download once a device that has it is online.".to_owned()
        }
        other => error_chain(&other),
    }
}
