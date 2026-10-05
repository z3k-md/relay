//! Cloud Files API through the `cloud-filter` crate, which holds the unsafe
//! FFI so this workspace stays `unsafe_code = "forbid"`.
//!
//! The crate's callback shims panic (and so abort the process) when reporting
//! a failure back to the system fails, for example after the request was
//! cancelled. Callbacks here therefore report success for anything except a
//! fetch that could not get its bytes, and never panic.

use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{Request, SyncFilter, info, ticket};
use cloud_filter::metadata::{Metadata, MetadataExt as _};
use cloud_filter::placeholder::{ConvertOptions, OpenOptions, Placeholder, UpdateOptions};
use cloud_filter::placeholder_file::PlaceholderFile;
use cloud_filter::root::{
    HydrationType, PopulationType, SecurityId, Session, SyncRootIdBuilder, SyncRootInfo,
};
use cloud_filter::utility::WriteAt;
use relay_core::{ObjectId, StatHint};

use super::{Hydration, Probe, Provider, RegisteredRoot, RootSpec};
use crate::error::FsError;

const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x4_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// Placeholder identity: this prefix, then the 32-byte object id.
const BLOB_MAGIC: &[u8; 4] = b"RLY1";
const BLOB_LEN: usize = 36;

/// FILETIME of the Unix epoch, in 100 ns ticks since 1601.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

pub(super) type Connection = cloud_filter::root::Connection<Filter>;

pub(super) fn is_dehydrated(meta: &fs::Metadata) -> bool {
    meta.file_attributes()
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

pub(super) fn supported() -> bool {
    catch_unwind(|| cloud_filter::root::is_supported().unwrap_or(false)).unwrap_or(false)
}

fn cloud_err(path: &Path, err: impl std::fmt::Display) -> FsError {
    FsError::Cloud {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Run a `cloud-filter` call that may panic on odd input (its getters
/// `unwrap` and `unreachable!` on values it does not expect).
fn guarded<T>(path: &Path, f: impl FnOnce() -> Result<T, FsError>) -> Result<T, FsError> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(cloud_err(path, "the Cloud Files call failed")))
}

fn root_id(account: &str) -> Result<cloud_filter::root::SyncRootId, String> {
    let sid = SecurityId::current_user().map_err(|err| err.to_string())?;
    Ok(SyncRootIdBuilder::new(super::PROVIDER)
        .user_security_id(sid)
        .account_name(account)
        .build())
}

pub(super) fn register(spec: &RootSpec<'_>) -> Result<(), FsError> {
    guarded(spec.path, || {
        let id = root_id(spec.account).map_err(|err| cloud_err(spec.path, err))?;
        let icon = std::env::current_exe()
            .map(|exe| format!("{},0", exe.display()))
            .unwrap_or_else(|_| "%SystemRoot%\\system32\\imageres.dll,-1043".to_owned());
        let info = SyncRootInfo::default()
            .with_display_name(spec.display_name)
            .with_icon(icon)
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_hydration_type(HydrationType::Full)
            .with_population_type(PopulationType::AlwaysFull)
            .with_allow_pinning(true)
            .with_show_siblings_as_group(false)
            .with_path(spec.path)
            .map_err(|err| cloud_err(spec.path, err))?;
        id.register(info).map_err(|err| cloud_err(spec.path, err))
    })
}

pub(super) fn unregister(account: &str) -> Result<(), FsError> {
    let path = Path::new(account);
    guarded(path, || {
        let id = root_id(account).map_err(|err| cloud_err(path, err))?;
        match id.is_registered() {
            Ok(false) => Ok(()),
            _ => id.unregister().map_err(|err| cloud_err(path, err)),
        }
    })
}

/// Where the system keeps every registered sync root, one key per id.
const SYNC_ROOT_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";

pub(super) fn registered() -> Vec<RegisteredRoot> {
    // `Relay!<SID>!`: every id this user registered starts with it.
    let Ok(prefix) = root_id("").map(|id| id.to_os_string()) else {
        return Vec::new();
    };
    let prefix = prefix.to_string_lossy().into_owned();
    let mut found = registered_in_registry(&prefix);
    for root in registered_by_shell(&prefix) {
        if !found.iter().any(|r| r.account == root.account) {
            found.push(root);
        }
    }
    found
}

fn account_of<'a>(id: &'a str, prefix: &str) -> Option<&'a str> {
    let account = id.strip_prefix(prefix)?;
    (!account.is_empty() && !account.contains('!')).then_some(account)
}

/// The registry is the system's own record and needs no shell; the shell's
/// list can come back empty where none runs (services, CI).
fn registered_in_registry(prefix: &str) -> Vec<RegisteredRoot> {
    let sid = prefix
        .trim_end_matches('!')
        .rsplit('!')
        .next()
        .unwrap_or("");
    let Ok(roots) = windows_registry::LOCAL_MACHINE.open(SYNC_ROOT_KEY) else {
        return Vec::new();
    };
    let Ok(ids) = roots.keys() else {
        return Vec::new();
    };
    ids.filter_map(|id| {
        let account = account_of(&id, prefix)?.to_owned();
        let path = roots
            .open(format!(r"{id}\UserSyncRoots"))
            .and_then(|key| key.get_string(sid))
            .ok()?;
        Some(RegisteredRoot {
            account,
            path: path.into(),
        })
    })
    .collect()
}

fn registered_by_shell(prefix: &str) -> Vec<RegisteredRoot> {
    let roots = catch_unwind(cloud_filter::root::active_roots)
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    // Other providers' roots can be malformed enough to panic in
    // `cloud-filter`'s getters, so each one is read on its own.
    roots
        .into_iter()
        .filter_map(|info| {
            catch_unwind(AssertUnwindSafe(|| {
                let id = info.id().to_os_string();
                let account = account_of(id.to_str()?, prefix)?.to_owned();
                Some(RegisteredRoot {
                    account,
                    path: info.path(),
                })
            }))
            .ok()
            .flatten()
        })
        .collect()
}

pub(super) fn connect(path: &Path, provider: Arc<dyn Provider>) -> Result<Connection, FsError> {
    guarded(path, || {
        Session::new()
            .block_implicit_hydration()
            .connect(path, Filter { provider })
            .map_err(|err| cloud_err(path, err))
    })
}

fn blob(object: ObjectId) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOB_LEN);
    out.extend_from_slice(BLOB_MAGIC);
    out.extend_from_slice(object.as_bytes());
    out
}

/// The object id in a placeholder identity as the callbacks see it: the
/// whole blob.
fn object_from_blob(bytes: &[u8]) -> Option<ObjectId> {
    if bytes.len() != BLOB_LEN || &bytes[..4] != BLOB_MAGIC {
        return None;
    }
    let id: [u8; 32] = bytes[4..].try_into().ok()?;
    Some(ObjectId::from_bytes(id))
}

/// The object id from `Placeholder::fixed_size_info(BLOB_LEN).blob()`.
///
/// cloud-filter 0.0.6 starts that slice at the end of the info struct rather
/// than at its identity field, 4 bytes earlier, so it holds the identity
/// minus its first 4 bytes plus 4 bytes past it. Both layouts are accepted
/// so a fixed release keeps working.
fn object_from_info_blob(bytes: &[u8]) -> Option<ObjectId> {
    if bytes.len() < 32 {
        return None;
    }
    if bytes.len() >= BLOB_LEN && &bytes[..4] == BLOB_MAGIC {
        return object_from_blob(&bytes[..BLOB_LEN]);
    }
    let id: [u8; 32] = bytes[..32].try_into().ok()?;
    Some(ObjectId::from_bytes(id))
}

pub(super) fn probe(path: &Path, meta: &fs::Metadata, stat: StatHint) -> io::Result<Probe> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return Ok(Probe::File(stat));
    }
    let dehydrated = is_dehydrated(meta);
    let read = catch_unwind(|| {
        let placeholder = Placeholder::open(path).ok()?;
        placeholder.fixed_size_info(BLOB_LEN).ok().flatten()
    });
    match read {
        Ok(Some(info)) => Ok(Probe::Placeholder {
            stat,
            object: object_from_info_blob(info.blob()),
            dehydrated,
            in_sync: info.is_in_sync(),
        }),
        // Not a placeholder, or one whose identity is not Relay's size.
        Ok(None) | Err(_) if dehydrated => Ok(Probe::Placeholder {
            stat,
            object: None,
            dehydrated,
            in_sync: false,
        }),
        Ok(None) | Err(_) => Ok(Probe::File(stat)),
    }
}

fn metadata(size: u64, modified_unix_ms: i64) -> Metadata {
    let filetime = modified_unix_ms
        .saturating_mul(10_000)
        .saturating_add(UNIX_EPOCH_FILETIME)
        .max(0);
    Metadata::file()
        .size(size)
        .creation_time(filetime)
        .last_write_time(filetime)
        .change_time(filetime)
        .last_access_time(filetime)
}

pub(super) fn create(
    path: &Path,
    object: ObjectId,
    size: u64,
    modified_unix_ms: i64,
) -> Result<(), FsError> {
    guarded(path, || {
        let parent = path
            .parent()
            .ok_or_else(|| cloud_err(path, "no parent directory"))?;
        let name = path
            .file_name()
            .ok_or_else(|| cloud_err(path, "no file name"))?;
        PlaceholderFile::new(name)
            .metadata(metadata(size, modified_unix_ms))
            .blob(blob(object))
            .mark_in_sync()
            .create::<&Path>(parent)
            .map(drop)
            .map_err(|err| cloud_err(path, err))
    })
}

fn open_for_write(path: &Path) -> Result<Placeholder, FsError> {
    OpenOptions::new()
        .write_access()
        .exclusive()
        .open(path)
        .map_err(|err| cloud_err(path, err))
}

pub(super) fn convert(path: &Path, object: ObjectId) -> Result<(), FsError> {
    guarded(path, || {
        let mut placeholder = open_for_write(path)?;
        placeholder
            .convert_to_placeholder(
                ConvertOptions::default().mark_in_sync().blob(blob(object)),
                None,
            )
            .map(drop)
            .map_err(|err| cloud_err(path, err))
    })
}

pub(super) fn update(
    path: &Path,
    object: ObjectId,
    size: u64,
    modified_unix_ms: i64,
    dehydrate: bool,
) -> Result<(), FsError> {
    guarded(path, || {
        let mut placeholder = open_for_write(path)?;
        let identity = blob(object);
        let mut options = UpdateOptions::default().blob(&identity).mark_in_sync();
        if dehydrate {
            options = options
                .metadata(metadata(size, modified_unix_ms))
                .dehydrate();
        }
        placeholder
            .update(options, None)
            .map(drop)
            .map_err(|err| cloud_err(path, err))
    })
}

pub(super) fn dehydrate(path: &Path) -> Result<(), FsError> {
    guarded(path, || {
        let mut placeholder = open_for_write(path)?;
        placeholder
            .update(
                UpdateOptions::default().update_if_in_sync().dehydrate(),
                None,
            )
            .map(drop)
            .map_err(|err| cloud_err(path, err))
    })
}

pub(super) struct Filter {
    provider: Arc<dyn Provider>,
}

struct Ticket<'a> {
    ticket: &'a ticket::FetchData,
    len: u64,
}

impl Hydration for Ticket<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.ticket
            .write_at(bytes, offset)
            .map_err(|err| io::Error::other(err.to_string()))
    }

    fn progress(&mut self, done: u64) {
        let _ = self.ticket.report_progress(self.len, done);
    }
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        _info: info::FetchData,
    ) -> CResult<()> {
        // This process's own reads are blocked by the session flag; refuse
        // them here too rather than wait on the loop that is reading.
        if request.process().id() == std::process::id() {
            return Err(CloudErrorKind::AccessDenied);
        }
        let path = request.path();
        let object = object_from_blob(request.file_blob());
        let mut out = Ticket {
            ticket: &ticket,
            len: request.file_size(),
        };
        let provider = &self.provider;
        match catch_unwind(AssertUnwindSafe(|| provider.fetch(&path, object, &mut out))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => {
                tracing::info!(path = %path.display(), %reason, "could not download a placeholder");
                Err(CloudErrorKind::NetworkUnavailable)
            }
            Err(_) => Err(CloudErrorKind::Unsuccessful),
        }
    }

    fn fetch_placeholders(
        &self,
        _request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        // Folders are always fully populated; there is nothing to add.
        let _ = ticket.pass_with_placeholder(&mut []);
        Ok(())
    }

    fn dehydrate(
        &self,
        _request: Request,
        ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        let _ = ticket.pass();
        Ok(())
    }

    fn dehydrated(&self, request: Request, _info: info::Dehydrated) {
        let path = request.path();
        let provider = &self.provider;
        let _ = catch_unwind(AssertUnwindSafe(|| provider.dehydrated(&path)));
    }

    fn delete(
        &self,
        _request: Request,
        ticket: ticket::Delete,
        _info: info::Delete,
    ) -> CResult<()> {
        let _ = ticket.pass();
        Ok(())
    }

    fn deleted(&self, request: Request, _info: info::Deleted) {
        let path = request.path();
        let provider = &self.provider;
        let _ = catch_unwind(AssertUnwindSafe(|| provider.moved(&path, None)));
    }

    fn rename(
        &self,
        _request: Request,
        ticket: ticket::Rename,
        _info: info::Rename,
    ) -> CResult<()> {
        let _ = ticket.pass();
        Ok(())
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        let to = request.path();
        let from = info.source_path();
        let provider = &self.provider;
        let _ = catch_unwind(AssertUnwindSafe(|| provider.moved(&from, Some(&to))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_round_trips_in_both_layouts() {
        let object = ObjectId::of(b"hello");
        let identity = blob(object);
        assert_eq!(object_from_blob(&identity), Some(object));
        assert_eq!(object_from_info_blob(&identity), Some(object));
        let mut shifted = identity[4..].to_vec();
        shifted.extend_from_slice(&[0; 4]);
        assert_eq!(object_from_info_blob(&shifted), Some(object));
        assert_eq!(object_from_blob(&identity[..20]), None);
    }
}
