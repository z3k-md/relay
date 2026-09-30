//! Filesystem-only conflict resolution (D21).
//!
//! Resolution edits files inside the mount so a running watcher records the
//! change. The index is scanned immediately only when no sync loop holds the
//! exclusive lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use relay_core::conflict::{is_conflict_copy, original_path};
use relay_core::{EntryContent, EntryKey, EntryRecord, LogicalPath, TEMP_PREFIX, git_dir_of};
use relay_fs::{check_real_dir_chain, ensure_real_dir_chain, resolve_os_path, to_os_path};
use serde::Serialize;

use crate::error::EngineError;
use crate::peers::ConflictClass;
use crate::{Engine, ScanOptions};

/// What to do with a single conflict copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    KeepCurrent,
    UseCopy,
}

/// Outcome of resolving one file conflict copy.
#[derive(Clone, Debug, Serialize)]
pub struct ResolveReport {
    pub space: String,
    pub mount: String,
    pub copy: LogicalPath,
    pub original: LogicalPath,
    pub resolution: Resolution,
    pub scanned: bool,
}

/// Outcome of cleaning Git metadata conflict copies under one repository.
#[derive(Clone, Debug, Serialize)]
pub struct GitResolveReport {
    pub space: String,
    pub mount: String,
    pub git_dir: LogicalPath,
    pub deleted: Vec<LogicalPath>,
    pub kept: Vec<LogicalPath>,
    pub scanned: bool,
}

/// Delete the copy (`KeepCurrent`) or replace the original with the copy's
/// current on-disk bytes (`UseCopy`). Old versions stay in history.
pub fn resolve_conflict(
    home: &Path,
    space: &str,
    mount: &str,
    copy_path: &LogicalPath,
    resolution: Resolution,
) -> Result<ResolveReport, EngineError> {
    let (root, original, copy_record) = {
        let engine = Engine::open_read_only(home)?;
        let (root, record) = lookup_live_copy(&engine, space, mount, copy_path)?;
        let original = original_path(copy_path)
            .ok_or_else(|| EngineError::NotAConflictCopy(copy_path.clone()))?;
        (root, original, record)
    };

    match resolution {
        Resolution::KeepCurrent => delete_entry_file(&root, copy_path)?,
        Resolution::UseCopy => {
            replace_original_with_copy(&root, &original, copy_path, &copy_record)?;
            delete_entry_file(&root, copy_path)?;
        }
    }

    let scanned = scan_if_possible(home, space, mount, &[original.clone(), copy_path.clone()])?;
    Ok(ResolveReport {
        space: space.to_owned(),
        mount: mount.to_owned(),
        copy: copy_path.clone(),
        original,
        resolution,
        scanned,
    })
}

/// Delete conflict copies under `git_dir`. Ref copies (`<git_dir>/refs/**`)
/// stay unless `include_branches` is set so the user can merge in Git.
pub fn resolve_git_conflicts(
    home: &Path,
    space: &str,
    mount: &str,
    git_dir: &LogicalPath,
    include_branches: bool,
) -> Result<GitResolveReport, EngineError> {
    let git_dir = normalize_git_dir(git_dir)?;
    let (to_delete, kept) = {
        let engine = Engine::open_read_only(home)?;
        let (_, config) = engine.lookup_mount(space, mount)?;
        if config.local_path.is_none() {
            return Err(EngineError::MountNotLocal);
        }
        let infos = engine.conflict_infos(Some(space))?;
        let mut to_delete = Vec::new();
        let mut kept = Vec::new();
        for info in infos {
            if info.mount != mount {
                continue;
            }
            let ConflictClass::Git {
                git_dir: entry_dir,
                is_ref,
            } = &info.class
            else {
                continue;
            };
            if entry_dir != &git_dir {
                continue;
            }
            if *is_ref && !include_branches {
                kept.push(info.record.key.path);
            } else {
                to_delete.push(info.record.key.path);
            }
        }
        (to_delete, kept)
    };

    {
        let engine = Engine::open_read_only(home)?;
        let (_, config) = engine.lookup_mount(space, mount)?;
        let root = config
            .local_path
            .clone()
            .ok_or(EngineError::MountNotLocal)?;
        drop(engine);
        for path in &to_delete {
            delete_entry_file(&root, path)?;
        }
    }

    let scanned = scan_if_possible(home, space, mount, &to_delete)?;
    Ok(GitResolveReport {
        space: space.to_owned(),
        mount: mount.to_owned(),
        git_dir,
        deleted: to_delete,
        kept,
        scanned,
    })
}

fn normalize_git_dir(path: &LogicalPath) -> Result<LogicalPath, EngineError> {
    if path.file_name() == ".git" {
        return Ok(path.clone());
    }
    if let Some(dir) = git_dir_of(path) {
        return Ok(dir);
    }
    path.join(".git").map_err(EngineError::InvalidName)
}

fn lookup_live_copy(
    engine: &Engine,
    space: &str,
    mount: &str,
    copy_path: &LogicalPath,
) -> Result<(PathBuf, EntryRecord), EngineError> {
    if !is_conflict_copy(copy_path) {
        return Err(EngineError::NotAConflictCopy(copy_path.clone()));
    }
    let (space_rec, config) = engine.lookup_mount(space, mount)?;
    let root = config
        .local_path
        .clone()
        .ok_or(EngineError::MountNotLocal)?;
    let key = EntryKey {
        space: space_rec.id,
        mount: config.mount.id,
        path: copy_path.clone(),
    };
    let record = engine
        .db
        .repo()
        .entry(&key)?
        .filter(|e| !e.is_deleted())
        .ok_or_else(|| EngineError::NotAConflictCopy(copy_path.clone()))?;
    match &record.content {
        EntryContent::File { .. } | EntryContent::Symlink { .. } => {}
        EntryContent::Directory => {
            return Err(EngineError::DirectoryConflict(copy_path.clone()));
        }
        EntryContent::Deleted => {
            return Err(EngineError::NotAConflictCopy(copy_path.clone()));
        }
    }
    Ok((root, record))
}

fn delete_entry_file(root: &Path, path: &LogicalPath) -> Result<(), EngineError> {
    let dest = match resolve_os_path(root, path)? {
        Some(p) => p,
        None => return Ok(()),
    };
    if let Some(parent) = dest.parent() {
        check_real_dir_chain(root, parent)?;
    }
    let meta = match fs::symlink_metadata(&dest) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(EngineError::Io(err)),
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        return Err(EngineError::DirectoryConflict(path.clone()));
    }
    fs::remove_file(&dest).map_err(EngineError::Io)
}

fn replace_original_with_copy(
    root: &Path,
    original: &LogicalPath,
    copy_path: &LogicalPath,
    copy_record: &EntryRecord,
) -> Result<(), EngineError> {
    let copy_os = resolve_os_path(root, copy_path)?
        .ok_or_else(|| EngineError::NotAConflictCopy(copy_path.clone()))?;
    if let Some(parent) = copy_os.parent() {
        check_real_dir_chain(root, parent)?;
    }
    let copy_meta = fs::symlink_metadata(&copy_os).map_err(EngineError::Io)?;
    if copy_meta.is_dir() && !copy_meta.file_type().is_symlink() {
        return Err(EngineError::DirectoryConflict(copy_path.clone()));
    }

    let dest = match resolve_os_path(root, original)? {
        Some(existing) => existing,
        None => to_os_path(root, original)?,
    };
    let dest_parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(root);
    ensure_real_dir_chain(root, dest_parent)?;

    if copy_meta.file_type().is_symlink()
        || matches!(copy_record.content, EntryContent::Symlink { .. })
    {
        let target = fs::read_link(&copy_os).map_err(EngineError::Io)?;
        if dest.exists() {
            let dest_meta = fs::symlink_metadata(&dest).map_err(EngineError::Io)?;
            if dest_meta.is_dir() && !dest_meta.file_type().is_symlink() {
                return Err(EngineError::DirectoryConflict(original.clone()));
            }
            fs::remove_file(&dest).map_err(EngineError::Io)?;
        }
        create_symlink(&target, &dest)?;
        return Ok(());
    }

    let bytes = read_regular_file(&copy_os)?;
    let executable = file_is_executable(&copy_meta);
    write_atomic(root, original, &dest, dest_parent, &bytes, executable)
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, EngineError> {
    let meta = fs::symlink_metadata(path).map_err(EngineError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(EngineError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to follow a symlink conflict copy",
        )));
    }
    fs::read(path).map_err(EngineError::Io)
}

fn write_atomic(
    root: &Path,
    original: &LogicalPath,
    dest: &Path,
    dest_parent: &Path,
    bytes: &[u8],
    executable: bool,
) -> Result<(), EngineError> {
    ensure_real_dir_chain(root, dest_parent)?;
    let tmp = temp_path(dest_parent);
    let mut guard = TempGuard(Some(tmp.clone()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(EngineError::Io)?;
        file.write_all(bytes).map_err(EngineError::Io)?;
        set_executable(&file, executable)?;
        file.sync_all().map_err(EngineError::Io)?;
    }
    if dest.exists() {
        let dest_meta = fs::symlink_metadata(dest).map_err(EngineError::Io)?;
        if dest_meta.is_dir() && !dest_meta.file_type().is_symlink() {
            return Err(EngineError::DirectoryConflict(original.clone()));
        }
        if dest_meta.file_type().is_symlink() {
            fs::remove_file(dest).map_err(EngineError::Io)?;
        }
    }
    fs::rename(&tmp, dest).map_err(EngineError::Io)?;
    guard.defuse();
    sync_dir(dest_parent)
}

fn temp_path(parent: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(
        "{}{:x}-{:x}",
        TEMP_PREFIX,
        std::process::id(),
        nanos
    ))
}

struct TempGuard(Option<PathBuf>);

impl TempGuard {
    fn defuse(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn file_is_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

fn set_executable(file: &File, executable: bool) -> Result<(), EngineError> {
    #[cfg(unix)]
    {
        if !executable {
            return Ok(());
        }
        use std::os::unix::fs::PermissionsExt;
        let mut perms = file.metadata().map_err(EngineError::Io)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        file.set_permissions(perms).map_err(EngineError::Io)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, executable);
        Ok(())
    }
}

fn create_symlink(target: &Path, dest: &Path) -> Result<(), EngineError> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dest).map_err(EngineError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, dest);
        Err(EngineError::RestoreUnsupported(
            "symlinks are not supported on this OS".into(),
        ))
    }
}

fn sync_dir(dir: &Path) -> Result<(), EngineError> {
    #[cfg(unix)]
    {
        let file = File::open(dir).map_err(EngineError::Io)?;
        file.sync_all().map_err(EngineError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn scan_if_possible(
    home: &Path,
    space: &str,
    mount: &str,
    paths: &[LogicalPath],
) -> Result<bool, EngineError> {
    if paths.is_empty() {
        return Ok(false);
    }
    match Engine::open(home) {
        Ok(mut engine) => {
            engine.scan_paths(space, mount, paths, ScanOptions::default())?;
            Ok(true)
        }
        Err(EngineError::Running { .. }) | Err(EngineError::Busy { .. }) => Ok(false),
        Err(err) => Err(err),
    }
}
