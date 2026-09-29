use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use relay_core::{ObjectId, StatHint, TEMP_PREFIX};
use uuid::Uuid;

use crate::error::FsError;

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
pub fn materialize_file(
    reader: &mut dyn Read,
    dest: &Path,
    expected: ObjectId,
    executable: bool,
    expected_existing: Option<&StatHint>,
) -> Result<StatHint, FsError> {
    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent).map_err(|e| FsError::io(parent, e))?;
    }
    let parent = parent.unwrap_or_else(|| Path::new("."));

    let tmp = parent.join(format!("{}{}", TEMP_PREFIX, Uuid::new_v4()));
    let mut guard = TempGuard(Some(tmp.clone()));

    write_hashed(&tmp, dest, reader, expected, executable)?;

    if !destination_still_matches(dest, expected_existing)? {
        return Err(FsError::DestinationChanged(dest.to_path_buf()));
    }

    fs::rename(&tmp, dest).map_err(|e| FsError::io(dest, e))?;
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
    executable: bool,
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

    set_executable(&file, tmp, executable)?;
    file.sync_all().map_err(|e| FsError::io(tmp, e))?;
    Ok(())
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

#[cfg(unix)]
fn set_executable(file: &File, path: &Path, executable: bool) -> Result<(), FsError> {
    if !executable {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file
        .metadata()
        .map_err(|e| FsError::io(path, e))?
        .permissions();
    perms.set_mode(perms.mode() | 0o111);
    file.set_permissions(perms)
        .map_err(|e| FsError::io(path, e))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_file: &File, _path: &Path, _executable: bool) -> Result<(), FsError> {
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
        let stat = write_via(&dest, b"hello", false, None).unwrap();
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
        write_via(&dest, b"newer!", false, Some(&before)).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"newer!");
    }

    #[test]
    fn destination_changed_when_dest_modified() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"old").unwrap();
        let before = StatHint::from_metadata(&fs::symlink_metadata(&dest).unwrap());
        fs::write(&dest, b"changed-size").unwrap();
        let err = write_via(&dest, b"incoming", false, Some(&before)).unwrap_err();
        assert!(matches!(err, FsError::DestinationChanged(_)));
        assert_eq!(fs::read(&dest).unwrap(), b"changed-size");
        assert!(list_temps(dir.path()).is_empty());
    }

    #[test]
    fn destination_changed_when_dest_unexpectedly_exists() {
        let dir = tempdir().unwrap();
        let dest = dir.path().join("file.txt");
        fs::write(&dest, b"already").unwrap();
        let err = write_via(&dest, b"incoming", false, None).unwrap_err();
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
        write_via(&dest, b"#!/bin/sh\n", true, None).unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0);
    }
}
