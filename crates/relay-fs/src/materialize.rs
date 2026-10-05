use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use relay_core::faults::{self, FaultPoint};
use relay_core::{ObjectId, StatHint, TEMP_PREFIX};
use uuid::Uuid;

use crate::error::FsError;
use crate::paths::ensure_real_dir_chain;

/// Options for a safe materialize under a mount root.
#[derive(Clone, Copy, Debug)]
pub struct MaterializeOptions<'a> {
    /// Mount root. Every existing ancestor from here to `dest`'s parent must
    /// be a real directory (not a symlink or reparse point).
    pub mount_root: &'a Path,
    /// Set the destination mtime to this Unix-nanosecond timestamp before the
    /// final rename, so a peer's Git stat checks keep matching.
    pub mtime_ns: Option<i64>,
}

/// Write `reader`'s bytes to `dest` atomically.
///
/// The temp file is named [`TEMP_PREFIX`] plus a random suffix and lives in
/// `dest`'s parent (same volume). Bytes are hashed while writing; a mismatch
/// deletes the temp and returns [`FsError::HashMismatch`].
///
/// Immediately before rename the destination is re-checked: if
/// `expected_existing` is `None` it must not exist; if `Some(stat)` the
/// current [`StatHint::from_metadata`] of `symlink_metadata` must equal it.
/// Otherwise the temp is deleted and [`FsError::DestinationChanged`] is
/// returned so the caller can treat the edit as a local change.
///
/// Parents are created one component at a time; an existing ancestor that is
/// not a real directory is refused so a symlink cannot redirect the write
/// outside the mount.
pub fn materialize_file(
    reader: &mut dyn Read,
    dest: &Path,
    expected: ObjectId,
    executable: bool,
    expected_existing: Option<&StatHint>,
    options: MaterializeOptions<'_>,
) -> Result<StatHint, FsError> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_real_dir_chain(options.mount_root, parent)?;

    // A replaced file keeps its read/write bits (a private `0600` file stays
    // private); a new file gets the umask default.
    let base_mode = expected_existing.and_then(|_| existing_rw_mode(dest));

    let tmp = parent.join(format!("{}{}", TEMP_PREFIX, Uuid::new_v4()));
    let mut guard = TempGuard(Some(tmp.clone()));

    faults::check(FaultPoint::MaterializeWrite, dest).map_err(|e| FsError::io(&tmp, e))?;
    write_hashed(
        &tmp,
        dest,
        reader,
        expected,
        base_mode,
        executable,
        options.mtime_ns,
    )?;

    if !destination_still_matches(dest, expected_existing)? {
        return Err(FsError::DestinationChanged(dest.to_path_buf()));
    }

    faults::check(FaultPoint::MaterializeRename, dest).map_err(|e| FsError::io(dest, e))?;
    rename_with_retry(&tmp, dest)?;
    guard.defuse();
    crate::sync_parent_dir(parent)?;

    let meta = fs::symlink_metadata(dest).map_err(|e| FsError::io(dest, e))?;
    Ok(StatHint::from_metadata(&meta))
}

fn write_hashed(
    tmp: &Path,
    dest: &Path,
    reader: &mut dyn Read,
    expected: ObjectId,
    base_mode: Option<u32>,
    executable: bool,
    mtime_ns: Option<i64>,
) -> Result<(), FsError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
        .map_err(|e| FsError::io(tmp, e))?;

    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).map_err(|e| FsError::io(tmp, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| FsError::io(tmp, e))?;
    }

    let actual = ObjectId::from(hasher.finalize());
    if actual != expected {
        return Err(FsError::HashMismatch {
            path: dest.to_path_buf(),
            expected,
            actual,
        });
    }

    set_mode(&file, tmp, base_mode, executable)?;
    if let Some(mtime_ns) = mtime_ns {
        set_mtime(&file, tmp, mtime_ns)?;
    }
    file.sync_all().map_err(|e| FsError::io(tmp, e))?;
    Ok(())
}

fn set_mtime(file: &File, path: &Path, mtime_ns: i64) -> Result<(), FsError> {
    let time = unix_ns_to_system_time(mtime_ns);
    let times = fs::FileTimes::new().set_modified(time);
    file.set_times(times).map_err(|e| FsError::io(path, e))
}

fn unix_ns_to_system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

const RENAME_ATTEMPTS: u32 = 5;

fn rename_with_retry(tmp: &Path, dest: &Path) -> Result<(), FsError> {
    let mut last = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(err) if retryable_rename(&err) && attempt + 1 < RENAME_ATTEMPTS => {
                last = Some(err);
                std::thread::sleep(Duration::from_millis(10 * u64::from(attempt + 1)));
            }
            Err(err) => return Err(FsError::io(dest, err)),
        }
    }
    Err(FsError::io(dest, last.expect("retry loop stored an error")))
}

fn retryable_rename(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION
        err.raw_os_error() == Some(32)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn destination_still_matches(
    dest: &Path,
    expected_existing: Option<&StatHint>,
) -> Result<bool, FsError> {
    match (expected_existing, fs::symlink_metadata(dest)) {
        (None, Err(err)) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        (None, Ok(_)) => Ok(false),
        (None, Err(err)) => Err(FsError::io(dest, err)),
        (Some(_), Err(err)) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        (Some(expected), Ok(meta)) => Ok(&StatHint::from_metadata(&meta) == expected),
        (Some(_), Err(err)) => Err(FsError::io(dest, err)),
    }
}

/// Read/write bits of the regular file at `dest`, if there is one. Execute
/// and special bits are left out: the record's `executable` flag decides
/// those.
#[cfg(unix)]
fn existing_rw_mode(dest: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::symlink_metadata(dest).ok()?;
    meta.is_file().then(|| meta.permissions().mode() & 0o666)
}

#[cfg(not(unix))]
fn existing_rw_mode(_dest: &Path) -> Option<u32> {
    None
}

/// Apply `base_mode` (else keep the umask default) and, for an executable,
/// add execute where read is granted.
#[cfg(unix)]
fn set_mode(
    file: &File,
    path: &Path,
    base_mode: Option<u32>,
    executable: bool,
) -> Result<(), FsError> {
    if base_mode.is_none() && !executable {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file
        .metadata()
        .map_err(|e| FsError::io(path, e))?
        .permissions();
    let mut mode = base_mode.unwrap_or_else(|| perms.mode());
    if executable {
        mode |= (mode & 0o444) >> 2;
    }
    perms.set_mode(mode);
    file.set_permissions(perms)
        .map_err(|e| FsError::io(path, e))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(
    _file: &File,
    _path: &Path,
    _base_mode: Option<u32>,
    _executable: bool,
) -> Result<(), FsError> {
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;

    use tempfile::tempdir;

    fn write_via(
        root: &Path,
        dest: &Path,
        bytes: &[u8],
        executable: bool,
        expected_existing: Option<&StatHint>,
    ) -> Result<StatHint, FsError> {
        materialize_file(
            &mut Cursor::new(bytes),
            dest,
            ObjectId::of(bytes),
            executable,
            expected_existing,
            MaterializeOptions {
                mount_root: root,
                mtime_ns: None,
            },
        )
    }

    fn list_temps(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(TEMP_PREFIX))
            })
            .collect()
    }

    #[test]
    fn materializes_new_file_and_parents() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("a/b/hello.txt");
        let stat = write_via(dir.path(), &dest, b"hello", false, None).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        assert_eq!(stat.size, 5);
        assert!(list_temps(dest.parent().unwrap()).is_empty());
    }

    #[test]
    fn replaces_when_expected_existing_matches() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"old").unwrap();
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        write_via(dir.path(), &dest, b"newer!", false, Some(&before)).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"newer!");
    }

    #[test]
    fn destination_changed_when_dest_modified() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"old").unwrap();
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        fs::write(&dest, b"changed-size").unwrap();
        let err = write_via(dir.path(), &dest, b"incoming", false, Some(&before)).unwrap_err();
        assert!(matches!(err, FsError::DestinationChanged(_)));
        assert_eq!(fs::read(&dest).unwrap(), b"changed-size");
        assert!(list_temps(dir.path()).is_empty());
    }

    #[test]
    fn destination_changed_when_dest_unexpectedly_exists() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"already").unwrap();
        let err = write_via(dir.path(), &dest, b"incoming", false, None).unwrap_err();
        assert!(matches!(err, FsError::DestinationChanged(_)));
        assert_eq!(fs::read(&dest).unwrap(), b"already");
    }

    #[test]
    fn hash_mismatch_leaves_dest_and_no_temps() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"keep-me").unwrap();
        let err = materialize_file(
            &mut Cursor::new(b"wrong-bytes"),
            &dest,
            ObjectId::of(b"expected-bytes"),
            false,
            None,
            MaterializeOptions {
                mount_root: dir.path(),
                mtime_ns: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, FsError::HashMismatch { .. }));
        assert_eq!(fs::read(&dest).unwrap(), b"keep-me");
        assert!(list_temps(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sets_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let dest = dir.path().join("tool");
        write_via(dir.path(), &dest, b"#!/bin/sh\n", true, None).unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_file_keeps_its_read_write_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let dest = dir.path().join("secret.env");
        fs::write(&dest, b"old").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        write_via(dir.path(), &dest, b"newer", false, Some(&before)).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"newer");
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Execute follows the record, not the old file: granted only where
        // read is, and dropped when the record says not executable.
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        write_via(dir.path(), &dest, b"#!/bin/sh\n", true, Some(&before)).unwrap();
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        write_via(dir.path(), &dest, b"plain", false, Some(&before)).unwrap();
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_write_through_a_symlink_escape() {
        let mount = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), mount.path().join("evil")).unwrap();
        let dest = mount.path().join("evil/x");
        let err = write_via(mount.path(), &dest, b"pwned", false, None).unwrap_err();
        assert!(matches!(err, FsError::UnsafeAncestor { .. }), "{err:?}");
        assert!(!outside.path().join("x").exists());
        assert!(!dest.exists());
    }

    #[test]
    fn sets_mtime_when_provided() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("timed.txt");
        let mtime_ns = 1_700_000_000_000_000_000i64;
        materialize_file(
            &mut Cursor::new(b"hi"),
            &dest,
            ObjectId::of(b"hi"),
            false,
            None,
            MaterializeOptions {
                mount_root: dir.path(),
                mtime_ns: Some(mtime_ns),
            },
        )
        .unwrap();
        let observed = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        let delta = (observed.mtime_ns - mtime_ns).abs();
        assert!(
            delta < 2_000_000_000,
            "mtime {observed:?} not near {mtime_ns}"
        );
    }
}
