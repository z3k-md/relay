//! Files other devices push to this one (D50).
//!
//! The network layer asks where a drop goes and reports it once it is
//! verified and saved. Both read the settings fresh, so turning drops off
//! or moving the inbox applies to the next drop.

use std::path::{Path, PathBuf};

use relay_core::DeviceId;
use relay_core::remote::{RemoteError, RemoteErrorCode};
use relay_engine::Engine;
use relay_ipc::ActivityItem;

use crate::host::{Host, now_ms};

/// `ActivityItem::kind` for a saved drop. `detail` is the saved path.
pub const DROP_ACTIVITY: &str = "drop";

/// The inbox for a drop from `peer`, created if missing.
pub(crate) fn inbox(home: &Path, peer: DeviceId) -> Result<PathBuf, RemoteError> {
    let engine = Engine::open_read_only(home)
        .map_err(|err| RemoteError::new(RemoteErrorCode::Busy, err.to_string()))?;
    trusted_name(&engine, peer)?;
    let settings = engine
        .drop_settings()
        .map_err(|err| RemoteError::new(RemoteErrorCode::Failed, err.to_string()))?;
    if !settings.receive {
        return Err(RemoteError::new(
            RemoteErrorCode::Forbidden,
            "that device is not taking files; turn drops on there",
        ));
    }
    let Some(inbox) = settings.inbox else {
        return Err(RemoteError::new(
            RemoteErrorCode::Failed,
            "that device has no folder for drops; choose one there",
        ));
    };
    std::fs::create_dir_all(&inbox).map_err(|err| {
        RemoteError::new(
            RemoteErrorCode::Failed,
            format!("cannot create the drop folder there: {err}"),
        )
    })?;
    Ok(inbox)
}

/// Log a saved drop. The desktop app shows it from the activity stream.
pub(crate) fn dropped(home: &Path, host: &Host, peer: DeviceId, path: &Path, size: u64) {
    let from = Engine::open_read_only(home)
        .ok()
        .and_then(|engine| trusted_name(&engine, peer).ok())
        .unwrap_or_else(|| peer.short());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    tracing::info!(peer = %from, path = %path.display(), size, "drop saved");
    host.push_activity(ActivityItem {
        at_ms: now_ms(),
        kind: DROP_ACTIVITY.into(),
        summary: format!("{from} sent {name}"),
        detail: Some(path.to_string_lossy().into_owned()),
    });
}

/// The local name of `peer` if it is trusted and not revoked.
fn trusted_name(engine: &Engine, peer: DeviceId) -> Result<String, RemoteError> {
    engine
        .peers()
        .map_err(|err| RemoteError::new(RemoteErrorCode::Failed, err.to_string()))?
        .into_iter()
        .find(|p| p.id == peer && !p.revoked)
        .map(|p| p.name)
        .ok_or_else(|| {
            RemoteError::new(
                RemoteErrorCode::Forbidden,
                "this device does not trust yours",
            )
        })
}
