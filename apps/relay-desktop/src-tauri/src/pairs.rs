//! Folder pairs set up from this device (remote explorer Stage 3), and files
//! opened from other devices (Stage 4). The host does the work; these pass
//! the request through.

use relay_ipc::{FolderPairParams, FolderPairPlan, FolderPairResult, OpenRemoteParams, QuickOpen};
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::commands::host_client;
use crate::error::error_chain;

/// What a folder pair would do. Changes nothing.
#[tauri::command(async)]
pub fn folder_pair_preview(
    app: AppHandle,
    params: FolderPairParams,
) -> Result<FolderPairPlan, String> {
    running(&app)?
        .folder_pair_preview(&params)
        .map_err(|err| error_chain(&err))
}

/// Set up the pair. On failure the host undoes what it already did.
#[tauri::command(async)]
pub fn folder_pair(app: AppHandle, params: FolderPairParams) -> Result<FolderPairResult, String> {
    running(&app)?
        .folder_pair(&params)
        .map_err(|err| error_chain(&err))
}

/// Get a file from another device (syncing its folder here online-only if
/// needed) and open it with its app. Returns the local path.
#[tauri::command(async)]
pub fn open_remote_file(app: AppHandle, peer: String, path: String) -> Result<String, String> {
    let opened = running(&app)?
        .open_remote(&OpenRemoteParams {
            peer,
            path,
            root: None,
        })
        .map_err(|err| error_chain(&err))?;
    let local = opened.path.to_string_lossy().to_string();
    app.opener()
        .open_path(local.clone(), None::<&str>)
        .map_err(|err| err.to_string())?;
    Ok(local)
}

#[tauri::command(async)]
pub fn list_quick_opens(app: AppHandle) -> Result<Vec<QuickOpen>, String> {
    running(&app)?
        .quick_opens()
        .map_err(|err| error_chain(&err))
}

/// Stop syncing a folder opened from another device. Returns a note when
/// that device could not be reached.
#[tauri::command(async)]
pub fn remove_quick_open(app: AppHandle, space: String) -> Result<Option<String>, String> {
    running(&app)?
        .quick_open_remove(&space)
        .map_err(|err| error_chain(&err))
}

fn running(app: &AppHandle) -> Result<relay_ipc::Client, String> {
    host_client(app)?.ok_or_else(|| "Relay is not running on this computer.".to_owned())
}
