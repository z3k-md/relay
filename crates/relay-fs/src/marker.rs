use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use relay_core::{DeviceId, MOUNT_MARKER, MountId, SpaceId, TEMP_PREFIX};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::FsError;

/// TOML file named [`MOUNT_MARKER`] at the mount root.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MountMarker {
    pub space: SpaceId,
    pub mount: MountId,
    pub created_by: DeviceId,
}

impl MountMarker {
    /// Write this marker atomically (temp file + rename in `root`).
    pub fn write(&self, root: &Path) -> Result<(), FsError> {
        let dest = root.join(MOUNT_MARKER);
        let tmp = root.join(format!("{}{}", TEMP_PREFIX, Uuid::new_v4()));
        let encoded = toml::to_string_pretty(self).map_err(|err| FsError::MarkerInvalid {
            path: dest.clone(),
            message: err.to_string(),
        })?;

        let write = || -> Result<(), FsError> {
            let mut file = File::create(&tmp).map_err(|e| FsError::io(&tmp, e))?;
            file.write_all(encoded.as_bytes())
                .map_err(|e| FsError::io(&tmp, e))?;
            file.sync_all().map_err(|e| FsError::io(&tmp, e))?;
            drop(file);
            fs::rename(&tmp, &dest).map_err(|e| FsError::io(&dest, e))?;
            crate::sync_parent_dir(root)?;
            Ok(())
        };

        match write() {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                Err(err)
            }
        }
    }

    /// Read the marker at `root`.
    pub fn read(root: &Path) -> Result<MountMarker, FsError> {
        match fs::metadata(root) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::MountRootMissing(root.to_path_buf()));
            }
            Err(err) => return Err(FsError::io(root, err)),
            Ok(meta) if !meta.is_dir() => return Err(FsError::NotADirectory(root.to_path_buf())),
            Ok(_) => {}
        }

        let path = root.join(MOUNT_MARKER);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::MarkerMissing(path));
            }
            Err(err) => return Err(FsError::io(&path, err)),
        };

        toml::from_str(&text).map_err(|err| FsError::MarkerInvalid {
            path,
            message: err.to_string(),
        })
    }

    /// Refuse to scan unless `root` exists and holds a marker for `mount`.
    pub fn verify(root: &Path, mount: MountId) -> Result<MountMarker, FsError> {
        let marker = Self::read(root)?;
        if marker.mount != mount {
            return Err(FsError::MarkerMismatch {
                path: root.join(MOUNT_MARKER),
                expected_mount: mount,
                found_mount: marker.mount,
            });
        }
        Ok(marker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> MountMarker {
        MountMarker {
            space: SpaceId::new(),
            mount: MountId::new(),
            created_by: DeviceId::random(),
        }
    }

    #[test]
    fn write_read_verify_round_trip() {
        let dir = tempdir().unwrap();
        let marker = sample();
        marker.write(dir.path()).unwrap();
        assert_eq!(MountMarker::read(dir.path()).unwrap(), marker);
        assert_eq!(
            MountMarker::verify(dir.path(), marker.mount).unwrap(),
            marker
        );
    }

    #[test]
    fn verify_root_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        let err = MountMarker::verify(&missing, MountId::new()).unwrap_err();
        assert!(matches!(err, FsError::MountRootMissing(_)));
    }

    #[test]
    fn verify_not_a_directory() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let err = MountMarker::read(&file).unwrap_err();
        assert!(matches!(err, FsError::NotADirectory(_)));
    }

    #[test]
    fn verify_marker_missing() {
        let dir = tempdir().unwrap();
        let err = MountMarker::verify(dir.path(), MountId::new()).unwrap_err();
        assert!(matches!(err, FsError::MarkerMissing(_)));
    }

    #[test]
    fn verify_mismatched_mount() {
        let dir = tempdir().unwrap();
        let marker = sample();
        marker.write(dir.path()).unwrap();
        let err = MountMarker::verify(dir.path(), MountId::new()).unwrap_err();
        assert!(matches!(err, FsError::MarkerMismatch { .. }));
    }

    #[test]
    fn verify_garbage_marker() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(MOUNT_MARKER), "not toml {{").unwrap();
        let err = MountMarker::read(dir.path()).unwrap_err();
        assert!(matches!(err, FsError::MarkerInvalid { .. }));
    }
}
