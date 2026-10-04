//! Folder pairs set up from this device (remote explorer Stage 3). The host
//! does the work; these pass the request through.

use relay_ipc::{FolderPairParams, FolderPairPlan, FolderPairResult};
use tauri::AppHandle;

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

fn running(app: &AppHandle) -> Result<relay_ipc::Client, String> {
    host_client(app)?.ok_or_else(|| "Relay is not running on this computer.".to_owned())
}
