//! Answers remote calls from peers allowed to manage this device (D37).
//!
//! The network layer already refused peers without the grant; this checks the
//! database again, because that is the record the user changes. Everything
//! here is read-only: listing folders, describing one path, and listing
//! spaces. Relay's own data folder is never listed, since it holds the
//! device key.

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use relay_core::DeviceId;
use relay_core::remote::{
    DEFAULT_LISTING, DirEntry, DirEntryKind, DirListing, MAX_LISTING, MountRef, RemoteCall,
    RemoteError, RemoteErrorCode, RemoteMount, RemoteReply, RemoteResult, RemoteRoot, RemoteSpace,
};
use std::sync::Arc;

use relay_core::ConfigChange;
use relay_core::remote::PathPreview;
use relay_engine::Engine;
use relay_ipc::ActivityItem;
use relay_net::ControlHandler;

use crate::host::{Host, now_ms};

pub(crate) struct Browser {
    home: PathBuf,
    host: Arc<Host>,
}

impl Browser {
    pub(crate) fn new(home: &Path, host: Arc<Host>) -> Self {
        Self {
            home: dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
            host,
        }
    }
}

impl ControlHandler for Browser {
    fn handle(&self, peer: DeviceId, call: RemoteCall) -> RemoteResult {
        let engine = Engine::open_read_only(&self.home)
            .map_err(|err| RemoteError::new(RemoteErrorCode::Busy, err.to_string()))?;
        let name = authorize(&engine, peer)?;
        drop(engine);
        answer(&self.host, &self.home, call, &name)
    }
}

/// Answer `call` on this device. `by` names who asked, for the activity log.
/// Peers reach this through [`ControlHandler::handle`] after the grant check;
/// this device's own folder-pair steps call it directly.
pub(crate) fn answer(host: &Host, home: &Path, call: RemoteCall, by: &str) -> RemoteResult {
    let engine = || {
        Engine::open_read_only(home)
            .map_err(|err| RemoteError::new(RemoteErrorCode::Busy, err.to_string()))
    };
    match call {
        RemoteCall::Roots => Ok(RemoteReply::Roots { roots: roots() }),
        RemoteCall::ListDir {
            path,
            cursor,
            limit,
        } => Ok(RemoteReply::Listing {
            listing: Context::load(&engine()?, home)?.list(&path, cursor, limit)?,
        }),
        RemoteCall::Stat { path } => Ok(RemoteReply::Stat {
            entry: Context::load(&engine()?, home)?.stat(&path)?,
        }),
        RemoteCall::Spaces => Ok(RemoteReply::Spaces {
            spaces: spaces(&engine()?)?,
        }),
        RemoteCall::Preview { path } => Ok(RemoteReply::Preview {
            preview: Context::load(&engine()?, home)?.preview(&path)?,
        }),
        RemoteCall::CreateDir { parent, name } => {
            let entry = Context::load(&engine()?, home)?.create_dir(&parent, &name)?;
            log(host, by, &format!("created folder {}", entry.path));
            Ok(RemoteReply::Created { entry })
        }
        RemoteCall::Apply { change } => {
            if !change.allowed_remotely() {
                return Err(RemoteError::new(
                    RemoteErrorCode::Forbidden,
                    "a managing device cannot change peers, grants, groups, or policies",
                ));
            }
            let summary = describe(&change);
            let applied = host
                .config(change)
                .map_err(|err| RemoteError::new(RemoteErrorCode::parse(&err.code), err.message))?;
            log(host, by, &summary);
            Ok(RemoteReply::Applied { applied })
        }
    }
}

fn log(host: &Host, by: &str, summary: &str) {
    host.push_activity(ActivityItem {
        at_ms: now_ms(),
        kind: "remote_change".into(),
        summary: format!("{by} {summary}"),
        detail: None,
    });
}

/// The name of `peer` if it may manage this device.
fn authorize(engine: &Engine, peer: DeviceId) -> Result<String, RemoteError> {
    engine
        .peers()
        .map_err(|err| RemoteError::new(RemoteErrorCode::Failed, err.to_string()))?
        .into_iter()
        .find(|p| p.id == peer && p.may_manage && !p.revoked)
        .map(|p| p.name)
        .ok_or_else(|| {
            RemoteError::new(
                RemoteErrorCode::Forbidden,
                "this device has not allowed yours to manage it",
            )
        })
}

/// One line for the activity log of a remote change.
fn describe(change: &ConfigChange) -> String {
    match change {
        ConfigChange::CreateSpace { space } => format!("created space {space}"),
        ConfigChange::DeleteSpace { space } => format!("deleted space {space}"),
        ConfigChange::JoinSpace { space, .. } => format!("joined space {space}"),
        ConfigChange::AddMount {
            space, mount, path, ..
        } => {
            format!("set up sync for {} ({space}/{mount})", path.display())
        }
        ConfigChange::RemoveMount { space, mount } => format!("stopped syncing {space}/{mount}"),
        ConfigChange::Share { space, .. } => format!("shared {space}"),
        ConfigChange::Unshare { space, .. } => format!("stopped sharing {space}"),
        ConfigChange::SetFolderMode {
            space,
            mount,
            path,
            mode,
        } => format!(
            "set {space}/{mount}/{path} to {}",
            mode.as_deref().unwrap_or("follow its parent")
        ),
        other => format!("changed {:?}", other.space()),
    }
}

/// Where browsing can start: home, then drives or volumes.
fn roots() -> Vec<RemoteRoot> {
    let mut roots = Vec::new();
    if let Some(home) = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        && let Some(path) = home.to_str()
    {
        roots.push(RemoteRoot {
            name: "Home".into(),
            path: path.to_owned(),
        });
    }
    roots.extend(volume_roots());
    roots
}

#[cfg(windows)]
fn volume_roots() -> Vec<RemoteRoot> {
    (b'A'..=b'Z')
        .map(|letter| format!("{}:\\", letter as char))
        .filter(|path| Path::new(path).exists())
        .map(|path| RemoteRoot {
            name: path.trim_end_matches('\\').to_owned(),
            path,
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn volume_roots() -> Vec<RemoteRoot> {
    let Ok(entries) = fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    let mut roots: Vec<RemoteRoot> = entries
        .flatten()
        .filter_map(|entry| {
            Some(RemoteRoot {
                name: entry.file_name().to_str()?.to_owned(),
                path: entry.path().to_str()?.to_owned(),
            })
        })
        .collect();
    roots.sort_by(|a, b| a.name.cmp(&b.name));
    roots
}

#[cfg(not(any(windows, target_os = "macos")))]
fn volume_roots() -> Vec<RemoteRoot> {
    vec![RemoteRoot {
        name: "Computer".into(),
        path: "/".into(),
    }]
}

fn spaces(engine: &Engine) -> Result<Vec<RemoteSpace>, RemoteError> {
    let failed =
        |err: relay_engine::EngineError| RemoteError::new(RemoteErrorCode::Failed, err.to_string());
    let mounts = engine.mounts(None).map_err(failed)?;
    Ok(engine
        .spaces()
        .map_err(failed)?
        .into_iter()
        .map(|space| RemoteSpace {
            mounts: mounts
                .iter()
                .filter(|(owner, _)| owner.id == space.id)
                .map(|(_, config)| RemoteMount {
                    name: config.mount.name.clone(),
                    path: config
                        .local_path
                        .as_ref()
                        .and_then(|p| p.to_str())
                        .map(str::to_owned),
                })
                .collect(),
            name: space.name,
        })
        .collect())
}

/// What a listing needs to know about this device: attached mounts and the
/// folders that must not be shown.
struct Context {
    mounts: Vec<(PathBuf, MountRef)>,
    relay_home: PathBuf,
    cloud_roots: Vec<PathBuf>,
}

impl Context {
    fn load(engine: &Engine, relay_home: &Path) -> Result<Self, RemoteError> {
        let mounts = engine
            .mounts(None)
            .map_err(|err| RemoteError::new(RemoteErrorCode::Failed, err.to_string()))?
            .into_iter()
            .filter_map(|(space, config)| {
                Some((
                    config.local_path?,
                    MountRef {
                        space: space.name,
                        mount: config.mount.name,
                    },
                ))
            })
            .collect();
        Ok(Self {
            mounts,
            relay_home: relay_home.to_path_buf(),
            cloud_roots: cloud_roots(),
        })
    }

    fn list(&self, path: &str, cursor: u32, limit: u32) -> Result<DirListing, RemoteError> {
        let dir = self.resolve(path)?;
        let read = fs::read_dir(&dir).map_err(|err| io_error(&err, &dir))?;
        let mut entries: Vec<DirEntry> = read
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path == self.relay_home
                    || relay_core::is_bookkeeping_component(entry.file_name().to_str()?)
                {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                self.describe(&path, &meta)
            })
            .collect();
        entries.sort_by(listing_order);
        let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let limit = match limit {
            0 => DEFAULT_LISTING,
            n => n.min(MAX_LISTING),
        };
        let start = (cursor as usize).min(entries.len());
        let end = (start + limit as usize).min(entries.len());
        let next_cursor = (end < entries.len()).then(|| u32::try_from(end).unwrap_or(u32::MAX));
        Ok(DirListing {
            path: utf8(&dir)?,
            parent: dir.parent().and_then(|p| p.to_str()).map(str::to_owned),
            entries: entries.drain(start..end).collect(),
            next_cursor,
            total,
            inside_mount: self
                .mounts
                .iter()
                .find(|(root, _)| dir.starts_with(root))
                .map(|(_, mount)| mount.clone()),
        })
    }

    fn stat(&self, path: &str) -> Result<DirEntry, RemoteError> {
        let path = self.resolve(path)?;
        let meta = fs::symlink_metadata(&path).map_err(|err| io_error(&err, &path))?;
        self.describe(&path, &meta).ok_or_else(|| {
            RemoteError::new(RemoteErrorCode::Invalid, "that path has no UTF-8 name")
        })
    }

    /// What making `path` a mount would mean. A missing path is not an
    /// error: the caller may create it.
    fn preview(&self, path: &str) -> Result<PathPreview, RemoteError> {
        let raw = Path::new(path);
        if !raw.is_absolute() {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!("{path} is not an absolute path"),
            ));
        }
        let canonical = match dunce::canonicalize(raw) {
            Ok(path) => path,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(PathPreview {
                    path: None,
                    exists: false,
                    is_dir: false,
                    files: 0,
                    bytes: 0,
                    truncated: false,
                    overlaps: None,
                    cloud_only: false,
                    writable: false,
                });
            }
            Err(err) => return Err(io_error(&err, raw)),
        };
        let path = self.resolve(path)?;
        let meta = fs::metadata(&path).map_err(|err| io_error(&err, &path))?;
        let entry = self.describe(&path, &meta);
        let (files, bytes, truncated) = if meta.is_dir() {
            count_files(&path)
        } else {
            (0, 0, false)
        };
        Ok(PathPreview {
            path: Some(utf8(&canonical)?),
            exists: true,
            is_dir: meta.is_dir(),
            files,
            bytes,
            truncated,
            overlaps: self
                .mounts
                .iter()
                .find(|(root, _)| root.starts_with(&path) || path.starts_with(root))
                .map(|(_, mount)| mount.clone()),
            cloud_only: entry.is_some_and(|e| e.cloud_only),
            writable: meta.is_dir() && writable(&path),
        })
    }

    /// Create folder `name` inside `parent`, or accept one already there.
    fn create_dir(&self, parent: &str, name: &str) -> Result<DirEntry, RemoteError> {
        let simple =
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']);
        if !simple {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!("{name:?} is not a folder name"),
            ));
        }
        let path = self.resolve(parent)?.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(err) => return Err(io_error(&err, &path)),
        }
        let meta = fs::metadata(&path).map_err(|err| io_error(&err, &path))?;
        self.describe(&path, &meta).ok_or_else(|| {
            RemoteError::new(RemoteErrorCode::Invalid, "that path has no UTF-8 name")
        })
    }

    /// An absolute path, canonicalized, outside Relay's own folder.
    fn resolve(&self, path: &str) -> Result<PathBuf, RemoteError> {
        let raw = Path::new(path);
        if !raw.is_absolute() {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!("{path} is not an absolute path"),
            ));
        }
        let canonical = dunce::canonicalize(raw).map_err(|err| io_error(&err, raw))?;
        if canonical.starts_with(&self.relay_home) {
            return Err(RemoteError::new(
                RemoteErrorCode::Denied,
                "Relay's own data folder cannot be browsed",
            ));
        }
        Ok(canonical)
    }

    fn describe(&self, path: &Path, meta: &fs::Metadata) -> Option<DirEntry> {
        let name = match path.file_name() {
            Some(name) => name.to_str()?.to_owned(),
            None => path.to_str()?.to_owned(),
        };
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            DirEntryKind::Symlink
        } else if file_type.is_dir() {
            DirEntryKind::Directory
        } else if file_type.is_file() {
            DirEntryKind::File
        } else {
            DirEntryKind::Other
        };
        Some(DirEntry {
            hidden: name.starts_with('.') || hidden_attribute(meta),
            cloud_only: cloud_attribute(meta)
                || self.cloud_roots.iter().any(|root| path.starts_with(root)),
            mount: self
                .mounts
                .iter()
                .find(|(root, _)| root == path)
                .map(|(_, mount)| mount.clone()),
            contains_mount: self
                .mounts
                .iter()
                .any(|(root, _)| root != path && root.starts_with(path)),
            size: (kind == DirEntryKind::File).then_some(meta.len()),
            modified_ms: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok()),
            path: path.to_str()?.to_owned(),
            name,
            kind,
        })
    }
}

/// Files counted before a preview stops and reports "at least".
const PREVIEW_MAX_FILES: u64 = 100_000;
const PREVIEW_MAX_TIME: Duration = Duration::from_secs(2);

/// Files and bytes under `root`, not following symlinks, within the bounds.
fn count_files(root: &Path) -> (u64, u64, bool) {
    let started = Instant::now();
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if files >= PREVIEW_MAX_FILES || started.elapsed() >= PREVIEW_MAX_TIME {
                return (files, bytes, true);
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                dirs.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_none_or(|name| !relay_core::is_bookkeeping_component(name))
            {
                files += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (files, bytes, false)
}

/// Whether this device can create files in `dir`: make and remove a probe
/// named like Relay's temp files, which scans already ignore.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(
        "{}probe-{}",
        relay_core::TEMP_PREFIX,
        std::process::id()
    ));
    match fs::File::create_new(&probe) {
        Ok(_) => fs::remove_file(&probe).is_ok(),
        Err(_) => false,
    }
}

/// Folders first, then by name ignoring case.
fn listing_order(a: &DirEntry, b: &DirEntry) -> Ordering {
    let is_dir = |e: &DirEntry| e.kind == DirEntryKind::Directory;
    is_dir(b)
        .cmp(&is_dir(a))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

fn utf8(path: &Path) -> Result<String, RemoteError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| RemoteError::new(RemoteErrorCode::Invalid, "that path has no UTF-8 name"))
}

fn io_error(err: &io::Error, path: &Path) -> RemoteError {
    let code = match err.kind() {
        io::ErrorKind::NotFound => RemoteErrorCode::NotFound,
        io::ErrorKind::PermissionDenied => RemoteErrorCode::Denied,
        io::ErrorKind::NotADirectory => RemoteErrorCode::Invalid,
        _ => RemoteErrorCode::Failed,
    };
    let message = match code {
        RemoteErrorCode::Denied => format!(
            "{} needs permission on this device (on a Mac: Full Disk Access for Relay)",
            path.display()
        ),
        _ => format!("{}: {err}", path.display()),
    };
    RemoteError::new(code, message)
}

/// Folders whose files are cloud placeholders on this platform.
fn cloud_roots() -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(home) = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    vec![
        home.join("Library/CloudStorage"),
        home.join("Library/Mobile Documents"),
    ]
}

#[cfg(windows)]
fn hidden_attribute(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(not(windows))]
fn hidden_attribute(_meta: &fs::Metadata) -> bool {
    false
}

/// OneDrive Files On-Demand and other cloud providers mark placeholders with
/// these attributes; opening one downloads it.
#[cfg(windows)]
fn cloud_attribute(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    meta.file_attributes()
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

#[cfg(not(windows))]
fn cloud_attribute(_meta: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(relay_home: &Path, mounts: Vec<(PathBuf, MountRef)>) -> Context {
        Context {
            mounts,
            relay_home: relay_home.to_path_buf(),
            cloud_roots: Vec::new(),
        }
    }

    #[test]
    fn lists_folders_first_pages_and_flags_mounts() {
        let root = tempfile::TempDir::new().unwrap();
        let root_path = dunce::canonicalize(root.path()).unwrap();
        for name in ["b.txt", "A.txt", "c.txt"] {
            fs::write(root_path.join(name), b"x").unwrap();
        }
        fs::create_dir_all(root_path.join("zdir/synced")).unwrap();
        fs::create_dir(root_path.join("relay-home")).unwrap();
        fs::write(root_path.join(".relay-mount"), b"").unwrap();
        let synced = root_path.join("zdir/synced");
        let ctx = context(
            &root_path.join("relay-home"),
            vec![(
                synced.clone(),
                MountRef {
                    space: "S".into(),
                    mount: "m".into(),
                },
            )],
        );

        let page = ctx.list(root_path.to_str().unwrap(), 0, 2).unwrap();
        assert_eq!(page.total, 4, "relay home and marker are hidden");
        let names: Vec<_> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["zdir", "A.txt"]);
        assert!(page.entries[0].contains_mount);
        assert_eq!(page.next_cursor, Some(2));

        let rest = ctx.list(root_path.to_str().unwrap(), 2, 2).unwrap();
        let names: Vec<_> = rest.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b.txt", "c.txt"]);
        assert_eq!(rest.next_cursor, None);

        let inside = ctx.list(synced.to_str().unwrap(), 0, 0).unwrap();
        assert_eq!(inside.inside_mount.unwrap().space, "S");
    }

    #[test]
    fn refuses_relative_paths_and_the_relay_home() {
        let root = tempfile::TempDir::new().unwrap();
        let home = dunce::canonicalize(root.path()).unwrap();
        let ctx = context(&home, Vec::new());
        let err = ctx.list("relative/path", 0, 0).unwrap_err();
        assert_eq!(err.code, RemoteErrorCode::Invalid);
        let err = ctx.list(home.to_str().unwrap(), 0, 0).unwrap_err();
        assert_eq!(err.code, RemoteErrorCode::Denied);
        let missing = home.join("nope");
        let err = ctx
            .list(
                &missing.parent().unwrap().join("x/y").to_string_lossy(),
                0,
                0,
            )
            .unwrap_err();
        assert!(matches!(
            err.code,
            RemoteErrorCode::NotFound | RemoteErrorCode::Denied
        ));
    }
}
