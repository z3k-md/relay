use std::path::{Component, Path, PathBuf};

use relay_core::{CoreError, LogicalPath};

use crate::error::FsError;

/// Join `path` onto `root` one component at a time. Never string-concatenates
/// with `/`.
pub fn to_os_path(root: &Path, path: &LogicalPath) -> PathBuf {
    let mut out = root.to_path_buf();
    for component in path.components() {
        out.push(component);
    }
    out
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
        let os = to_os_path(&root, &logical);
        assert_eq!(os, PathBuf::from("/mnt/space/game/inventory/Core.lua"));
        assert_eq!(to_logical_path(&root, &os).unwrap(), logical);
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = PathBuf::from("/mnt/space");
        let err = to_logical_path(&root, Path::new("/other/file")).unwrap_err();
        assert!(matches!(err, FsError::InvalidName { .. }));
    }
}
