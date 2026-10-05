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
    DEFAULT_LISTING, DirEntry, DirEntryKind, DirListing, FolderSize, FolderSizes, MAX_LISTING,
    MountRef, RemoteCall, RemoteError, RemoteErrorCode, RemoteMount, RemoteReply, RemoteResult,
    RemoteRoot, RemoteSpace,
};
use std::sync::Arc;

use relay_core::ConfigChange;
use relay_core::remote::{Located, MountedPath, PathPreview};
use relay_engine::Engine;
use relay_ipc::ActivityItem;
use relay_net::ControlHandler;

use crate::host::{Host, now_ms};
use crate::sizes::{self, Sizer};

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

    fn open_file(
        &self,
        peer: DeviceId,
        path: &str,
        max_bytes: u64,
    ) -> Result<fs::File, RemoteError> {
        let engine = Engine::open_read_only(&self.home)
            .map_err(|err| RemoteError::new(RemoteErrorCode::Busy, err.to_string()))?;
        let name = authorize(&engine, peer)?;
        let (file, path) = Context::load(&engine, &self.home)?.open_for_copy(path, max_bytes)?;
        log(&self.host, &name, &format!("copied {}", path.display()));
        Ok(file)
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
        RemoteCall::Locate { path } => Ok(RemoteReply::Located {
            located: Context::load(&engine()?, home)?.locate(&path)?,
        }),
        RemoteCall::FolderSizes { path } => Ok(RemoteReply::FolderSizes {
            sizes: Context::load(&engine()?, home)?.folder_sizes(&host.sizer, &path)?,
        }),
        RemoteCall::ScanFirst { space, mount, path } => {
            host.scan_first(&space, &mount, &path)
                .map_err(|err| RemoteError::new(RemoteErrorCode::parse(&err.code), err.message))?;
            Ok(RemoteReply::Done)
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
        // The directory read already gives names and types, which is all the
        // order needs; only the page returned is stat'ed, so a large folder
        // costs one stat per entry shown rather than per entry per page.
        let mut found: Vec<Found> = read
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                let path = entry.path();
                if path == self.relay_home || relay_core::is_bookkeeping_component(&name) {
                    return None;
                }
                let is_dir = entry.file_type().ok()?.is_dir();
                Some(Found { is_dir, name, path })
            })
            .collect();
        found.sort_by(listing_order);
        let total = u32::try_from(found.len()).unwrap_or(u32::MAX);
        let limit = match limit {
            0 => DEFAULT_LISTING,
            n => n.min(MAX_LISTING),
        };
        let start = (cursor as usize).min(found.len());
        let end = (start + limit as usize).min(found.len());
        let next_cursor = (end < found.len()).then(|| u32::try_from(end).unwrap_or(u32::MAX));
        let entries = found[start..end]
            .iter()
            .filter_map(|entry| {
                let meta = fs::symlink_metadata(&entry.path).ok()?;
                self.describe(&entry.path, &meta)
            })
            .collect();
        Ok(DirListing {
            path: utf8(&dir)?,
            parent: dir.parent().and_then(|p| p.to_str()).map(str::to_owned),
            entries,
            next_cursor,
            total,
            inside_mount: self
                .mounts
                .iter()
                .find(|(root, _)| dir.starts_with(root))
                .map(|(_, mount)| mount.clone()),
            ancestors: ancestors(&dir),
        })
    }

    /// Sizes of the folders directly inside `path`, as far as counted.
    fn folder_sizes(&self, sizer: &Sizer, path: &str) -> Result<FolderSizes, RemoteError> {
        let dir = self.resolve(path)?;
        let read = fs::read_dir(&dir).map_err(|err| io_error(&err, &dir))?;
        let folders: Vec<PathBuf> = read
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .map(|entry| entry.path())
            .filter(|path| *path != self.relay_home && path.to_str().is_some())
            .collect();
        let folders: Vec<FolderSize> = sizer
            .sizes(&folders)
            .into_iter()
            .zip(&folders)
            .filter_map(|(progress, path)| {
                Some(FolderSize {
                    path: path.to_str()?.to_owned(),
                    bytes: progress.bytes,
                    files: progress.files,
                    done: progress.done,
                })
            })
            .collect();
        Ok(FolderSizes {
            path: utf8(&dir)?,
            done: folders.iter().all(|f| f.done),
            folders,
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

    /// Open one file for a read-only copy, with its canonical path.
    fn open_for_copy(
        &self,
        path: &str,
        max_bytes: u64,
    ) -> Result<(fs::File, PathBuf), RemoteError> {
        let path = self.resolve(path)?;
        let meta = fs::metadata(&path).map_err(|err| io_error(&err, &path))?;
        if !meta.is_file() {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!("{} is not a file", path.display()),
            ));
        }
        if meta.len() > max_bytes {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!(
                    "{} is {} MB, more than a read-only copy takes ({} MB); open it to sync its folder instead",
                    path.display(),
                    meta.len().div_ceil(1024 * 1024),
                    max_bytes / (1024 * 1024)
                ),
            ));
        }
        let file = fs::File::open(&path).map_err(|err| io_error(&err, &path))?;
        Ok((file, path))
    }

    /// The folder, name, and mount of one file.
    fn locate(&self, path: &str) -> Result<Located, RemoteError> {
        let file = self.resolve(path)?;
        let meta = fs::metadata(&file).map_err(|err| io_error(&err, &file))?;
        if !meta.is_file() {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                format!("{} is not a file", file.display()),
            ));
        }
        let folder = file.parent().unwrap_or(&file);
        let name = file.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            RemoteError::new(RemoteErrorCode::Invalid, "that file has no UTF-8 name")
        })?;
        let mount = self
            .mounts
            .iter()
            .find(|(root, _)| file.starts_with(root))
            .and_then(|(root, mount)| {
                let inside: Vec<&str> = file
                    .strip_prefix(root)
                    .ok()?
                    .components()
                    .map(|c| c.as_os_str().to_str())
                    .collect::<Option<_>>()?;
                let logical = relay_core::LogicalPath::new(&inside.join("/")).ok()?;
                Some(MountedPath {
                    space: mount.space.clone(),
                    mount: mount.mount.clone(),
                    path: logical.as_str().to_owned(),
                })
            });
        Ok(Located {
            folder: utf8(folder)?,
            name: name.to_owned(),
            size: meta.len(),
            mount,
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
            disk_size: (kind == DirEntryKind::File).then(|| sizes::allocated(path, meta)),
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

/// `dir` and the folders above it, outermost first. Empty if any of them has
/// no UTF-8 name, since the caller could not open it anyway.
fn ancestors(dir: &Path) -> Vec<RemoteRoot> {
    let mut folders: Vec<RemoteRoot> = dir
        .ancestors()
        .map(|path| {
            let path_str = path.to_str()?;
            let name = path
                .file_name()
                .map_or(Some(path_str), |name| name.to_str())?;
            Some(RemoteRoot {
                name: name.to_owned(),
                path: path_str.to_owned(),
            })
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    folders.reverse();
    folders
}

/// A directory entry before it is stat'ed: what the listing order needs.
/// A symlink to a folder is not a folder here, as in [`DirEntryKind`].
struct Found {
    is_dir: bool,
    name: String,
    path: PathBuf,
}

/// Folders first, then by name ignoring case.
fn listing_order(a: &Found, b: &Found) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
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
        let crumbs: Vec<_> = inside.ancestors.iter().map(|a| a.name.as_str()).collect();
        assert!(crumbs.ends_with(&["zdir", "synced"]), "{crumbs:?}");
        assert_eq!(inside.ancestors.last().unwrap().path, inside.path);
        assert_eq!(
            inside.ancestors[inside.ancestors.len() - 2].path,
            inside.parent.unwrap()
        );
        assert_eq!(page.ancestors.len() + 2, inside.ancestors.len());
    }

    /// Folder sizes come back keyed by the same paths a listing shows, and
    /// files list their space on disk next to their length.
    #[test]
    fn folder_sizes_match_listing_paths() {
        let root = tempfile::TempDir::new().unwrap();
        let root_path = dunce::canonicalize(root.path()).unwrap();
        fs::create_dir_all(root_path.join("Projects/app")).unwrap();
        fs::write(root_path.join("Projects/app/main.rs"), vec![b'x'; 10_000]).unwrap();
        fs::write(root_path.join("notes.txt"), b"hi").unwrap();
        fs::create_dir(root_path.join("relay-home")).unwrap();
        let ctx = context(&root_path.join("relay-home"), Vec::new());
        let dir = root_path.to_str().unwrap();

        let listing = ctx.list(dir, 0, 0).unwrap();
        let notes = listing
            .entries
            .iter()
            .find(|e| e.name == "notes.txt")
            .unwrap();
        assert_eq!(notes.size, Some(2));
        assert!(notes.disk_size.is_some());
        let projects = listing
            .entries
            .iter()
            .find(|e| e.name == "Projects")
            .unwrap();
        assert_eq!(projects.disk_size, None);

        let sizer = Sizer::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        let sizes = loop {
            let sizes = ctx.folder_sizes(&sizer, dir).unwrap();
            if sizes.done {
                break sizes;
            }
            assert!(Instant::now() < deadline, "sizes never finished");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(sizes.path, listing.path);
        assert_eq!(sizes.folders.len(), 1, "relay home and files are left out");
        assert_eq!(sizes.folders[0].path, projects.path);
        assert_eq!(sizes.folders[0].files, 1);
    }

    #[test]
    fn copies_only_small_files_outside_the_relay_home() {
        let root = tempfile::TempDir::new().unwrap();
        let root_path = dunce::canonicalize(root.path()).unwrap();
        let home = root_path.join("relay-home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("device.key"), b"secret").unwrap();
        fs::write(root_path.join("big.bin"), vec![0u8; 2048]).unwrap();
        let ctx = context(&home, Vec::new());

        let (_, path) = ctx
            .open_for_copy(root_path.join("big.bin").to_str().unwrap(), 4096)
            .unwrap();
        assert_eq!(path, root_path.join("big.bin"));
        let err = ctx
            .open_for_copy(root_path.join("big.bin").to_str().unwrap(), 1024)
            .unwrap_err();
        assert_eq!(err.code, RemoteErrorCode::Invalid);
        let err = ctx
            .open_for_copy(home.join("device.key").to_str().unwrap(), 4096)
            .unwrap_err();
        assert_eq!(err.code, RemoteErrorCode::Denied);
        let err = ctx
            .open_for_copy(root_path.to_str().unwrap(), 4096)
            .unwrap_err();
        assert_eq!(err.code, RemoteErrorCode::Invalid, "a folder is not a file");
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
