//! Open a file on a paired device that this one does not sync (D40).
//!
//! The file's folder becomes an online-only folder here, so the file opens
//! like any synced file and edits flow back. Three cases, by where the file
//! is on the other device:
//!
//! 1. In a mount this device already has: download it.
//! 2. In a mount this device does not have: that device shares the space,
//!    and it is joined here online-only.
//! 3. In no mount: its own folder is paired here online-only (D39), under
//!    `~/Relay/<device>/`.
//!
//! Then the other device indexes that one file first, and it downloads.
//! What quick-open set up is recorded in `<home>/quick-open.json` (host
//! bookkeeping, outside the database so it never reloads the engine), so it
//! can be listed and removed again.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use relay_core::remote::{Located, MountRef, RemoteCall, RemoteError, RemoteReply};
use relay_core::{ConfigChange, DeviceId, SpaceId};
use relay_engine::{CopyState, Engine, PeerInfo};
use relay_ipc::{
    FetchParams, FolderEnd, FolderPairParams, OpenRemoteParams, OpenedRemote, QuickOpen,
    RpcErrorBody,
};
use serde::{Deserialize, Serialize};

use crate::folder_pair;
use crate::host::{Host, now_ms};

const RECORDS_FILE: &str = "quick-open.json";
/// How long to wait for the opened file's index row to arrive.
const ROW_WAIT: Duration = Duration::from_secs(30);
/// Files that apps write next to an open document; never worth syncing for a
/// quick look.
const LEFT_OUT: &[&str] = &["~$*", ".~lock.*#", ".DS_Store", "Thumbs.db"];
const JOIN_WAIT_MS: u64 = 8_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    space_id: SpaceId,
    space: String,
    peer: DeviceId,
    folder: String,
    created_on_peer: bool,
    created_at_ms: u64,
    last_opened_ms: u64,
}

pub(crate) fn open(host: &Host, params: &OpenRemoteParams) -> Result<OpenedRemote, RpcErrorBody> {
    let peer = host.peer_named(&params.peer)?;
    if params.read_only {
        return Ok(OpenedRemote {
            path: crate::read_copy::copy(host, &peer, &params.path)?,
            synced: None,
        });
    }
    let located = match ask(
        host,
        &peer,
        RemoteCall::Locate {
            path: params.path.clone(),
        },
    )? {
        RemoteReply::Located { located } => located,
        other => return Err(unexpected(&other)),
    };
    let root = match &params.root {
        Some(root) => root.clone(),
        None => default_root()?,
    };
    let (space, mount, path) = match &located.mount {
        Some(at) if mounted_here(host, &at.space, &at.mount)? => {
            (at.space.clone(), at.mount.clone(), at.path.clone())
        }
        Some(at) => {
            join_theirs(host, &peer, &located, &root)?;
            (at.space.clone(), at.mount.clone(), at.path.clone())
        }
        None => {
            let space = pair_folder(host, &peer, &located, &root)?;
            (space.clone(), space, located.name.clone())
        }
    };
    ask(
        host,
        &peer,
        RemoteCall::ScanFirst {
            space: space.clone(),
            mount: mount.clone(),
            path: path.clone(),
        },
    )?;
    wait_for_row(host, &space, &mount, &path)?;
    host.fetch(FetchParams {
        space: space.clone(),
        mount: mount.clone(),
        path: path.clone(),
    })?;
    touch(host, &space);
    let local = engine(host)?
        .local_file_path(&space, &mount, &path)
        .map_err(engine_error)?;
    Ok(OpenedRemote {
        path: local,
        synced: Some(MountRef { space, mount }),
    })
}

pub(crate) fn list(host: &Host) -> Result<Vec<QuickOpen>, RpcErrorBody> {
    let engine = engine(host)?;
    let peers = engine.peers().map_err(engine_error)?;
    let mounts = engine.mounts(None).map_err(engine_error)?;
    Ok(live_records(host)?
        .into_iter()
        .map(|record| QuickOpen {
            peer: peers
                .iter()
                .find(|p| p.id == record.peer)
                .map_or_else(|| record.peer.to_string(), |p| p.name.clone()),
            local_path: mounts
                .iter()
                .find(|(space, _)| space.id == record.space_id)
                .and_then(|(_, config)| config.local_path.clone()),
            space: record.space,
            folder: record.folder,
            created_at_ms: record.created_at_ms,
            last_opened_ms: record.last_opened_ms,
            created_on_peer: record.created_on_peer,
        })
        .collect())
}

/// Whether `space` here exists only for quick-open.
pub(crate) fn is_quick_open(host: &Host, space: &str) -> bool {
    load(host).iter().any(|record| record.space == space)
}

/// Undo what quick-open set up for `space`: here it stops syncing and the
/// space goes (files stay on disk); on the other device the folder pair goes
/// too if quick-open created it, or the space is just no longer shared.
/// Returns a note when the other device could not be reached.
pub(crate) fn remove(host: &Host, space: &str) -> Result<Option<String>, RpcErrorBody> {
    let mut records = load(host);
    let Some(index) = records.iter().position(|r| r.space == space) else {
        return Err(RpcErrorBody::new(
            "not_found",
            format!("{space} was not opened from another device"),
        ));
    };
    let record = records.remove(index);
    let mounts = engine(host)?.mounts(Some(space)).map_err(engine_error)?;
    for (_, config) in mounts.iter().filter(|(_, c)| c.local_path.is_some()) {
        host.config(ConfigChange::RemoveMount {
            space: space.to_owned(),
            mount: config.mount.name.clone(),
        })?;
    }
    host.config(ConfigChange::DeleteSpace {
        space: space.to_owned(),
    })?;
    save(host, &records);

    let here = engine(host)?.device().id;
    let theirs: Vec<ConfigChange> = if record.created_on_peer {
        mounts
            .iter()
            .map(|(_, config)| ConfigChange::RemoveMount {
                space: space.to_owned(),
                mount: config.mount.name.clone(),
            })
            .chain([ConfigChange::DeleteSpace {
                space: space.to_owned(),
            }])
            .collect()
    } else {
        vec![ConfigChange::Unshare {
            space: space.to_owned(),
            peer: here.to_string(),
        }]
    };
    for change in theirs {
        if let Err(err) = host.call_peer(record.peer, RemoteCall::Apply { change }) {
            tracing::info!(%space, error = %err, "could not finish removing a quick-open folder on the other device");
            return Ok(Some(format!(
                "Removed here. The other device could not be reached, so {space} may still be set up there ({}).",
                err.message
            )));
        }
    }
    Ok(None)
}

/// Case 2: the other device syncs this folder already. It shares the space
/// with this device, which joins it online-only.
fn join_theirs(
    host: &Host,
    peer: &PeerInfo,
    located: &Located,
    root: &Path,
) -> Result<(), RpcErrorBody> {
    let at = located.mount.as_ref().expect("case 2 has a mount");
    let here = engine(host)?.device().id;
    ask(
        host,
        peer,
        RemoteCall::Apply {
            change: ConfigChange::Share {
                space: at.space.clone(),
                peer: here.to_string(),
            },
        },
    )?;
    let joined = host.config(ConfigChange::JoinSpace {
        space: at.space.clone(),
        from_peer: peer.id.to_string(),
        wait_ms: JOIN_WAIT_MS,
    })?;
    let space_id = match joined {
        relay_core::ConfigApplied::Space { space } => space.id,
        _ => return Err(RpcErrorBody::new("failed", "joining returned no space")),
    };
    host.config(online_only(&at.space, &at.mount))?;
    // A name the user already gave a rule here is fine to keep.
    let _ = host.config(left_out_rule(&at.space, &at.mount));
    let folder = new_folder(&root.join(&peer.name), &at.mount)?;
    fs::create_dir(&folder)
        .map_err(|err| RpcErrorBody::new("failed", format!("{}: {err}", folder.display())))?;
    host.config(ConfigChange::AddMount {
        space: at.space.clone(),
        mount: at.mount.clone(),
        path: folder,
        includes: Vec::new(),
        excludes: Vec::new(),
    })?;
    remember(
        host,
        Record {
            space_id,
            space: at.space.clone(),
            peer: peer.id,
            folder: located.folder.clone(),
            created_on_peer: false,
            created_at_ms: now_ms(),
            last_opened_ms: now_ms(),
        },
    );
    Ok(())
}

/// Case 3: pair the file's own folder here, online-only. Returns the space.
fn pair_folder(
    host: &Host,
    peer: &PeerInfo,
    located: &Located,
    root: &Path,
) -> Result<String, RpcErrorBody> {
    let parent = root.join(&peer.name);
    fs::create_dir_all(&parent)
        .map_err(|err| RpcErrorBody::new("failed", format!("{}: {err}", parent.display())))?;
    let folder = new_folder(&parent, &folder_pair::folder_name(&located.folder))?;
    let name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| RpcErrorBody::new("failed", "no folder name"))?;
    let made = folder_pair::run(
        host,
        &FolderPairParams {
            source: FolderEnd {
                device: Some(peer.name.clone()),
                path: located.folder.clone(),
            },
            dest: FolderEnd {
                device: None,
                path: path_string(&parent)?,
            },
            create_dest: Some(name),
            name: None,
            excludes: Vec::new(),
            exclude_patterns: LEFT_OUT.iter().map(|p| (*p).to_owned()).collect(),
            dest_online_only: true,
        },
    )?;
    let space_id = engine(host)?
        .spaces()
        .map_err(engine_error)?
        .into_iter()
        .find(|s| s.name == made.space)
        .map(|s| s.id)
        .ok_or_else(|| RpcErrorBody::new("failed", "the new space is missing"))?;
    remember(
        host,
        Record {
            space_id,
            space: made.space.clone(),
            peer: peer.id,
            folder: located.folder.clone(),
            created_on_peer: true,
            created_at_ms: now_ms(),
            last_opened_ms: now_ms(),
        },
    );
    Ok(made.space)
}

fn mounted_here(host: &Host, space: &str, mount: &str) -> Result<bool, RpcErrorBody> {
    Ok(engine(host)?
        .mounts(Some(space))
        .unwrap_or_default()
        .iter()
        .any(|(_, config)| config.mount.name == mount && config.local_path.is_some()))
}

/// Wait until the file's index row is here.
fn wait_for_row(host: &Host, space: &str, mount: &str, path: &str) -> Result<(), RpcErrorBody> {
    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
    let deadline = Instant::now() + ROW_WAIT;
    loop {
        let arrived = engine(host)?
            .list_folder(space, mount, folder)
            .is_ok_and(|view| {
                view.entries
                    .iter()
                    .any(|row| row.name == name && row.state != CopyState::MetadataOnly)
            });
        if arrived {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RpcErrorBody::new(
                "timeout",
                "the other device is still indexing that folder; try again in a moment",
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `parent/name`, or `parent/name 2`, … if that is taken. Creates `parent`,
/// not the folder itself.
fn new_folder(parent: &Path, name: &str) -> Result<PathBuf, RpcErrorBody> {
    fs::create_dir_all(parent)
        .map_err(|err| RpcErrorBody::new("failed", format!("{}: {err}", parent.display())))?;
    let candidate = (1..)
        .map(|n| match n {
            1 => parent.join(name),
            n => parent.join(format!("{name} {n}")),
        })
        .find(|path| !path.exists())
        .expect("an unused name exists");
    Ok(candidate)
}

fn online_only(space: &str, mount: &str) -> ConfigChange {
    ConfigChange::SetFolderMode {
        space: space.to_owned(),
        mount: mount.to_owned(),
        path: String::new(),
        mode: Some("demand".to_owned()),
    }
}

fn left_out_rule(space: &str, mount: &str) -> ConfigChange {
    ConfigChange::MaterializeAdd {
        space: space.to_owned(),
        name: "left-out-files".to_owned(),
        mode: "exclude".to_owned(),
        selectors: LEFT_OUT.iter().map(|p| format!("{mount}/**/{p}")).collect(),
    }
}

fn default_root() -> Result<PathBuf, RpcErrorBody> {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join("Relay"))
        .ok_or_else(|| RpcErrorBody::new("failed", "this device has no home folder"))
}

fn ask(host: &Host, peer: &PeerInfo, call: RemoteCall) -> Result<RemoteReply, RpcErrorBody> {
    host.call_peer(peer.id, call).map_err(rpc)
}

fn engine(host: &Host) -> Result<Engine, RpcErrorBody> {
    Engine::open_read_only(&host.home).map_err(engine_error)
}

fn engine_error(err: relay_engine::EngineError) -> RpcErrorBody {
    RpcErrorBody::new(err.code(), err.to_string())
}

fn rpc(err: RemoteError) -> RpcErrorBody {
    RpcErrorBody::new(err.code.as_str(), err.message)
}

fn unexpected(reply: &RemoteReply) -> RpcErrorBody {
    RpcErrorBody::new("failed", format!("unexpected reply {reply:?}"))
}

fn path_string(path: &Path) -> Result<String, RpcErrorBody> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcErrorBody::new("invalid", "that folder has no UTF-8 name"))
}

fn load(host: &Host) -> Vec<Record> {
    fs::read(host.home.join(RECORDS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Records whose space still exists here; others were removed elsewhere.
fn live_records(host: &Host) -> Result<Vec<Record>, RpcErrorBody> {
    let spaces = engine(host)?.spaces().map_err(engine_error)?;
    Ok(load(host)
        .into_iter()
        .filter(|record| spaces.iter().any(|s| s.id == record.space_id))
        .collect())
}

fn save(host: &Host, records: &[Record]) {
    let path = host.home.join(RECORDS_FILE);
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(records)
        .map_err(std::io::Error::other)
        .and_then(|bytes| fs::write(&tmp, bytes))
        .and_then(|()| fs::rename(&tmp, &path));
    if let Err(err) = written {
        tracing::warn!(path = %path.display(), error = %err, "could not save quick-open records");
    }
}

fn remember(host: &Host, record: Record) {
    let mut records = load(host);
    records.retain(|r| r.space_id != record.space_id);
    records.push(record);
    save(host, &records);
}

fn touch(host: &Host, space: &str) {
    let mut records = load(host);
    if let Some(record) = records.iter_mut().find(|r| r.space == space) {
        record.last_opened_ms = now_ms();
        save(host, &records);
    }
}
