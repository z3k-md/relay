use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use relay_core::{CoreError, LogicalPath};
use unicode_normalization::UnicodeNormalization;

use crate::error::FsError;

/// Join `path` onto `root` one component at a time. Never string-concatenates
/// with `/`.
///
/// Each logical component must be exactly one [`Component::Normal`] on this
/// OS. Names that encode a drive prefix, `..`, or an OS path separator
/// (`C:\x`, `a\..\b`, `\\server\share`) are rejected so they cannot escape
/// the mount root.
///
/// This writes the NFC spelling from [`LogicalPath`]. Use it only to construct
/// destinations for **new** files. On-disk names may be a different
/// normalization (NFD is common for files copied from macOS); look up existing
/// entries with [`resolve_os_path`].
pub fn to_os_path(root: &Path, path: &LogicalPath) -> Result<PathBuf, FsError> {
    let mut out = root.to_path_buf();
    for component in path.components() {
        require_normal_component(path, component)?;
        out.push(component);
    }
    Ok(out)
}

/// True when `component` is exactly one [`Component::Normal`] whose OsStr
/// equals the logical name. Drive prefixes, `..`, and extra separators fail.
pub fn component_is_normal(component: &str) -> bool {
    inspect_normal_component(component).is_ok()
}

fn require_normal_component(path: &LogicalPath, component: &str) -> Result<(), FsError> {
    inspect_normal_component(component).map_err(|reason| FsError::Unrepresentable {
        path: path.as_str().to_owned(),
        reason,
    })
}

fn inspect_normal_component(component: &str) -> Result<(), String> {
    let as_path = Path::new(component);
    let mut parts = as_path.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(name)), None) if name.to_str() == Some(component) => Ok(()),
        (Some(Component::Normal(_)), None) => Err(format!(
            "component {component:?} is not a single ordinary name on this OS"
        )),
        (Some(other), rest) => Err(format!(
            "component {component:?} expands to {other:?}{} on this OS",
            if rest.is_some() {
                " plus further parts"
            } else {
                ""
            }
        )),
        (None, _) => Err(format!("component {component:?} is empty on this OS")),
    }
}

/// Walk `path` from `root`, returning the real on-disk path if it exists.
///
/// Each component is tried as a direct join first. If that name is missing,
/// the directory is scanned for a UTF-8 entry whose NFC form equals the
/// logical component. Several NFC-equivalent names are resolved by taking the
/// first in byte order of the on-disk name (the same survivor the scanner
/// keeps after a normalization collision).
///
/// Intermediate components that are not real directories (including
/// symlinks) yield `Ok(None)`. Permission and other IO errors are
/// [`FsError::Io`].
pub fn resolve_os_path(root: &Path, path: &LogicalPath) -> Result<Option<PathBuf>, FsError> {
    let mut current = root.to_path_buf();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        require_normal_component(path, component)?;
        let is_last = components.peek().is_none();
        let Some(next) = resolve_component(&current, component)? else {
            return Ok(None);
        };
        if !is_last {
            let meta = match fs::symlink_metadata(&next) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(FsError::io(&next, err)),
            };
            if !is_real_directory(&meta) {
                return Ok(None);
            }
        }
        current = next;
    }
    Ok(Some(current))
}

fn resolve_component(dir: &Path, component: &str) -> Result<Option<PathBuf>, FsError> {
    let direct = dir.join(component);
    match fs::symlink_metadata(&direct) {
        Ok(_) => return Ok(Some(direct)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(FsError::io(&direct, err)),
    }

    let mut matches = nfc_matching_entries(dir, component)?;
    if matches.is_empty() {
        return Ok(None);
    }
    matches.sort_by(|a, b| name_bytes(a).cmp(name_bytes(b)));
    Ok(matches.into_iter().next())
}

fn nfc_matching_entries(dir: &Path, component: &str) -> Result<Vec<PathBuf>, FsError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(FsError::io(dir, err)),
    };

    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| FsError::io(dir, err))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let nfc: String = name.nfc().collect();
        if nfc == component {
            matches.push(entry.path());
        }
    }
    Ok(matches)
}

/// Walk from `root` down to `dir`, creating missing directories one component
/// at a time. Every *existing* ancestor must be a real directory (not a
/// symlink, junction, or reparse point).
pub fn ensure_real_dir_chain(root: &Path, dir: &Path) -> Result<(), FsError> {
    walk_dir_chain(root, dir, true)
}

/// Same as [`ensure_real_dir_chain`] but never creates directories. Used
/// before deletes so a symlink ancestor cannot redirect the removal.
pub fn check_real_dir_chain(root: &Path, dir: &Path) -> Result<(), FsError> {
    walk_dir_chain(root, dir, false)
}

fn walk_dir_chain(root: &Path, dir: &Path, create_missing: bool) -> Result<(), FsError> {
    require_real_dir(root)?;
    if dir == root {
        return Ok(());
    }
    let rel = dir
        .strip_prefix(root)
        .map_err(|_| FsError::Unrepresentable {
            path: dir.display().to_string(),
            reason: "path is not under the mount root".into(),
        })?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(meta) if is_real_directory(&meta) => {}
                    Ok(_) => {
                        return Err(FsError::UnsafeAncestor {
                            path: current,
                            reason: "ancestor is not a real directory".into(),
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        if create_missing {
                            fs::create_dir(&current).map_err(|e| FsError::io(&current, e))?;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(err) => return Err(FsError::io(&current, err)),
                }
            }
            other => {
                return Err(FsError::Unrepresentable {
                    path: current.display().to_string(),
                    reason: format!("path contains {other:?}"),
                });
            }
        }
    }
    Ok(())
}

fn require_real_dir(path: &Path) -> Result<(), FsError> {
    let meta = fs::symlink_metadata(path).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            FsError::MountRootMissing(path.to_path_buf())
        } else {
            FsError::io(path, err)
        }
    })?;
    if is_real_directory(&meta) {
        Ok(())
    } else {
        Err(FsError::UnsafeAncestor {
            path: path.to_path_buf(),
            reason: "mount root is not a real directory".into(),
        })
    }
}

fn name_bytes(path: &Path) -> &[u8] {
    path.file_name()
        .map(|name| name.as_encoded_bytes())
        .unwrap_or(b"")
}

pub(crate) fn is_real_directory(meta: &fs::Metadata) -> bool {
    meta.is_dir() && !is_symlink_like(meta)
}

pub(crate) fn is_symlink_like(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

/// Strip `root` from `os_path` and rebuild a [`LogicalPath`].
///
/// Component names must be UTF-8. Names that fail [`LogicalPath::new`] become
/// [`FsError::InvalidName`].
pub fn to_logical_path(root: &Path, os_path: &Path) -> Result<LogicalPath, FsError> {
    let relative = os_path
        .strip_prefix(root)
        .map_err(|_| FsError::InvalidName {
            os_path: os_path.to_path_buf(),
            reason: "path is not under the mount root".to_owned(),
        })?;

    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| FsError::NonUtf8(os_path.to_path_buf()))?;
                parts.push(name);
            }
            _ => {
                return Err(FsError::InvalidName {
                    os_path: os_path.to_path_buf(),
                    reason: "path contains an illegal component".to_owned(),
                });
            }
        }
    }

    LogicalPath::from_components(parts).map_err(|err| match err {
        CoreError::InvalidPath { reason, .. } => FsError::InvalidName {
            os_path: os_path.to_path_buf(),
            reason: reason.to_owned(),
        },
        other => FsError::InvalidName {
            os_path: os_path.to_path_buf(),
            reason: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn round_trip_nested_path() {
        let root = PathBuf::from("/mnt/space");
        let logical = LogicalPath::new("game/inventory/Core.lua").unwrap();
        let os = to_os_path(&root, &logical).unwrap();
        assert_eq!(os, PathBuf::from("/mnt/space/game/inventory/Core.lua"));
        assert_eq!(to_logical_path(&root, &os).unwrap(), logical);
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = PathBuf::from("/mnt/space");
        let err = to_logical_path(&root, Path::new("/other/file")).unwrap_err();
        assert!(matches!(err, FsError::InvalidName { .. }));
    }

    #[test]
    fn resolve_os_path_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gone = LogicalPath::new("gone.txt").unwrap();
        assert_eq!(resolve_os_path(root, &gone).unwrap(), None);
        let nested = LogicalPath::new("missing/child.txt").unwrap();
        assert_eq!(resolve_os_path(root, &nested).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_os_path_intermediate_symlink_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let through_link = LogicalPath::new("link/file.txt").unwrap();
        assert_eq!(resolve_os_path(root, &through_link).unwrap(), None);
        let link_itself = LogicalPath::new("link").unwrap();
        assert_eq!(
            resolve_os_path(root, &link_itself).unwrap().as_deref(),
            Some(root.join("link").as_path())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_os_path_nfd_file_via_nfc_logical() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let nfd = root.join("cafe\u{301}.txt");
        std::fs::write(&nfd, b"nfd").unwrap();
        let logical = LogicalPath::new("caf\u{e9}.txt").unwrap();
        assert_eq!(
            to_os_path(root, &logical).unwrap(),
            root.join("caf\u{e9}.txt")
        );
        assert_eq!(
            resolve_os_path(root, &logical).unwrap().as_deref(),
            Some(nfd.as_path())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_os_path_nfd_directory_with_nfc_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let nfd_dir = root.join("cafe\u{301}");
        std::fs::create_dir(&nfd_dir).unwrap();
        let nfc_file = nfd_dir.join("caf\u{e9}.txt");
        std::fs::write(&nfc_file, b"inside").unwrap();
        let logical = LogicalPath::new("caf\u{e9}/caf\u{e9}.txt").unwrap();
        assert_eq!(
            resolve_os_path(root, &logical).unwrap().as_deref(),
            Some(nfc_file.as_path())
        );
    }

    #[test]
    fn rejects_component_containing_os_separator() {
        if std::path::MAIN_SEPARATOR == '/' {
            // `/` cannot appear in a LogicalPath component; nothing to test.
            return;
        }
        let name = format!("a{}b", std::path::MAIN_SEPARATOR);
        let logical = LogicalPath::new(&name).unwrap();
        let err = to_os_path(Path::new("/mnt"), &logical).unwrap_err();
        assert!(matches!(err, FsError::Unrepresentable { .. }), "{err:?}");
        let err = resolve_os_path(Path::new("/mnt"), &logical).unwrap_err();
        assert!(matches!(err, FsError::Unrepresentable { .. }), "{err:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_and_unc_components_are_unrepresentable() {
        let root = PathBuf::from(r"C:\relay\mount");
        for bad in [r"C:\x", r"a\..\b", r"\\server\share", "C:"] {
            let logical = LogicalPath::new(bad).unwrap();
            let err = to_os_path(&root, &logical).unwrap_err();
            assert!(
                matches!(err, FsError::Unrepresentable { .. }),
                "{bad:?} => {err:?}"
            );
        }
    }
}
