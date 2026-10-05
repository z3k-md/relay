//! Online-only files as native placeholders (D43).
//!
//! On Windows this wraps the Cloud Files API: a mount becomes a sync root,
//! online-only files are placeholders that hold the object id, and the system
//! asks a [`Provider`] for the bytes when an app reads one. Elsewhere every
//! operation reports [`FsError::CloudUnsupported`] and nothing is a
//! placeholder, so callers need no platform checks of their own.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use relay_core::ObjectId;

use crate::error::FsError;

#[cfg(windows)]
mod windows;

/// Provider name in every sync root id, `Relay!<user SID>!<mount id>`.
pub const PROVIDER: &str = "Relay";

/// Whether this placeholder's bytes are not all on disk. Reading such a file
/// asks its provider for them, so the engine never hashes one.
pub fn is_dehydrated(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        windows::is_dehydrated(meta)
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

/// [`is_dehydrated`] for a path. False when it cannot be read.
pub fn is_dehydrated_path(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| is_dehydrated(&meta))
}

/// Whether this system can host placeholders at all.
pub fn supported() -> bool {
    #[cfg(windows)]
    {
        windows::supported()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// What a folder is registered as.
#[derive(Clone, Debug)]
pub struct RootSpec<'a> {
    pub path: &'a Path,
    /// Unique per root. Relay uses the mount id.
    pub account: &'a str,
    /// Shown in Explorer's navigation pane.
    pub display_name: &'a str,
}

/// A sync root this user registered under [`PROVIDER`], from any run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredRoot {
    pub account: String,
    pub path: PathBuf,
}

/// Register `spec.path` as a sync root. Registering again updates it.
pub fn register(spec: &RootSpec<'_>) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::register(spec)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        Err(FsError::CloudUnsupported)
    }
}

/// Unregister the root with this account. Placeholders stay on disk; the
/// ones without data can no longer be opened.
pub fn unregister(account: &str) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::unregister(account)
    }
    #[cfg(not(windows))]
    {
        let _ = account;
        Err(FsError::CloudUnsupported)
    }
}

/// Every root registered under [`PROVIDER`] for this user.
pub fn registered() -> Vec<RegisteredRoot> {
    #[cfg(windows)]
    {
        windows::registered()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Where hydrated bytes go. Offsets and lengths must be multiples of 4 KiB,
/// except a write that ends at [`Hydration::len`].
pub trait Hydration {
    /// The placeholder's size.
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    /// Shown as a progress bar in Explorer.
    fn progress(&mut self, done: u64);
}

/// The daemon side of one sync root. Called on system threads; no method may
/// wait on anything that waits on the caller's own reads.
pub trait Provider: Send + Sync + 'static {
    /// Write the bytes of the placeholder at `path`, whose stored id is
    /// `object` (`None` when it is not one of Relay's).
    fn fetch(
        &self,
        path: &Path,
        object: Option<ObjectId>,
        out: &mut dyn Hydration,
    ) -> Result<(), String>;
    /// The system dropped the bytes of `path` ("Free up space").
    fn dehydrated(&self, path: &Path);
    /// A placeholder was deleted, or moved from `path` to `to`.
    fn moved(&self, path: &Path, to: Option<&Path>);
}

/// A live connection to a sync root. Callbacks stop when it is dropped.
pub struct Connection {
    #[cfg(windows)]
    _inner: windows::Connection,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

/// Connect `provider` to the registered root at `path`. Reads of placeholders
/// by this process never reach the provider: they fail instead, so the engine
/// cannot wait on itself.
pub fn connect(path: &Path, provider: Arc<dyn Provider>) -> Result<Connection, FsError> {
    #[cfg(windows)]
    {
        Ok(Connection {
            _inner: windows::connect(path, provider)?,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (path, provider);
        Err(FsError::CloudUnsupported)
    }
}

/// What is on disk at a path inside a sync root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Missing,
    /// A plain file, not a placeholder.
    File(relay_core::StatHint),
    Placeholder {
        stat: relay_core::StatHint,
        /// The object id Relay stored in it. `None` for anything else.
        object: Option<ObjectId>,
        dehydrated: bool,
        in_sync: bool,
    },
    /// A directory, symlink, or something unreadable.
    Other,
}

pub fn probe(path: &Path) -> io::Result<Probe> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Probe::Missing),
        Err(err) => return Err(err),
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Ok(Probe::Other);
    }
    let stat = relay_core::StatHint::from_metadata(&meta);
    #[cfg(windows)]
    {
        windows::probe(path, &meta, stat)
    }
    #[cfg(not(windows))]
    {
        Ok(Probe::File(stat))
    }
}

/// The object id Relay stored in a placeholder, if it is one of Relay's.
pub fn placeholder_object(path: &Path) -> Option<ObjectId> {
    match probe(path) {
        Ok(Probe::Placeholder { object, .. }) => object,
        _ => None,
    }
}

/// Create a placeholder without data at `path`, in sync, for `object`.
/// The parent directory must exist inside a connected root.
pub fn create(
    path: &Path,
    object: ObjectId,
    size: u64,
    modified_unix_ms: i64,
) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::create(path, object, size, modified_unix_ms)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, object, size, modified_unix_ms);
        Err(FsError::CloudUnsupported)
    }
}

/// Turn a plain file whose bytes are `object` into a placeholder in sync.
pub fn convert(path: &Path, object: ObjectId) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::convert(path, object)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, object);
        Err(FsError::CloudUnsupported)
    }
}

/// Point a placeholder at `object`, mark it in sync, and drop its bytes when
/// `dehydrate`.
pub fn update(
    path: &Path,
    object: ObjectId,
    size: u64,
    modified_unix_ms: i64,
    dehydrate: bool,
) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::update(path, object, size, modified_unix_ms, dehydrate)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, object, size, modified_unix_ms, dehydrate);
        Err(FsError::CloudUnsupported)
    }
}

/// Drop the bytes of a placeholder that is in sync. The file stays visible.
pub fn dehydrate(path: &Path) -> Result<(), FsError> {
    #[cfg(windows)]
    {
        windows::dehydrate(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(FsError::CloudUnsupported)
    }
}

/// Remove Relay's placeholders that hold no data under `root`, deepest
/// first, so nothing is left that cannot be opened once the root is gone.
/// Returns how many went.
pub fn remove_dehydrated(root: &Path) -> usize {
    let mut removed = 0;
    let walk = walkdir::WalkDir::new(root)
        .follow_links(false)
        .contents_first(true);
    for entry in walk.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if is_dehydrated(&meta)
            && placeholder_object(entry.path()).is_some()
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_files_are_never_placeholders_here() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        assert_eq!(probe(&path).unwrap(), Probe::Missing);
        fs::write(&path, b"hello").unwrap();
        assert!(matches!(probe(&path).unwrap(), Probe::File(stat) if stat.size == 5));
        assert!(!is_dehydrated_path(&path));
        assert_eq!(placeholder_object(&path), None);
        assert_eq!(probe(dir.path()).unwrap(), Probe::Other);
        assert_eq!(remove_dehydrated(dir.path()), 0);
        assert!(path.exists());
    }

    #[cfg(not(windows))]
    #[test]
    fn placeholder_operations_are_unsupported_here() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        assert!(!supported());
        assert!(registered().is_empty());
        let object = ObjectId::of(b"x");
        assert!(matches!(
            create(&path, object, 1, 0),
            Err(FsError::CloudUnsupported)
        ));
        assert!(matches!(dehydrate(&path), Err(FsError::CloudUnsupported)));
    }
}
