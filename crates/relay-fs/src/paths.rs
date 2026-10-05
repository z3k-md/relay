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
/// On macOS and Windows a direct join also succeeds for a name that differs
/// only in case, so the on-disk spelling is confirmed; a path that exists
/// only as `Foo` when `foo` was asked for is `Ok(None)` (see [`Resolved`]).
/// The spelling is confirmed once for the whole path from its canonical
/// form, not per component: the engine's apply path resolves every entry
/// here and the scanner resolves every ancestor of every requested path, so
/// a call costs at most one `canonicalize` of the path (plus one of the
/// root).
///
/// Intermediate components that are not real directories (including
/// symlinks) yield `Ok(None)`. Permission and other IO errors are
/// [`FsError::Io`].
pub fn resolve_os_path(root: &Path, path: &LogicalPath) -> Result<Option<PathBuf>, FsError> {
    Ok(match resolve_os_path_detailed(root, path)? {
        Resolved::Found(os_path) => Some(os_path),
        Resolved::Missing | Resolved::CaseMismatch(_) => None,
    })
}

/// Outcome of [`resolve_os_path_detailed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The path exists under this on-disk spelling.
    Found(PathBuf),
    /// Nothing on disk matches.
    Missing,
    /// On a case-insensitive filesystem the path exists only under a spelling
    /// that differs from the logical one in case (`Foo` on disk, `foo`
    /// asked). The logical path itself counts as missing; the payload is the
    /// on-disk path so the caller can index the real name.
    CaseMismatch(PathBuf),
}

/// [`resolve_os_path`] that also reports a case-only spelling difference.
pub(crate) fn resolve_os_path_detailed(
    root: &Path,
    path: &LogicalPath,
) -> Result<Resolved, FsError> {
    if !CASE_INSENSITIVE_PLATFORM {
        return walk_components(root, path, false);
    }
    // Accept every direct join as it comes, then confirm the spelling of
    // the whole path with one `canonicalize`. When that cannot decide (a
    // link as the last component, a root that does not canonicalize) each
    // component is confirmed on its own.
    match walk_components(root, path, false)? {
        Resolved::Missing => Ok(Resolved::Missing),
        Resolved::Found(current) | Resolved::CaseMismatch(current) => {
            match confirm_whole_path(root, &current, path) {
                Some(resolved) => Ok(resolved),
                None => walk_components(root, path, true),
            }
        }
    }
}

/// One component at a time from `root`. With `confirm_each`, a direct join
/// is only accepted once [`spelling_confirmed`] or the directory listing
/// proves the on-disk spelling; without it the caller confirms the whole
/// path afterwards (or, on a case-sensitive platform, needs no confirmation).
fn walk_components(
    root: &Path,
    path: &LogicalPath,
    confirm_each: bool,
) -> Result<Resolved, FsError> {
    let mut current = root.to_path_buf();
    let mut case_mismatch = false;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        require_normal_component(path, component)?;
        let is_last = components.peek().is_none();
        let next = match resolve_component(&current, component, confirm_each)? {
            ComponentMatch::Found(next) => next,
            ComponentMatch::Missing => return Ok(Resolved::Missing),
            ComponentMatch::CaseVariant(next) => {
                case_mismatch = true;
                next
            }
        };
        if !is_last {
            let meta = match fs::symlink_metadata(&next) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Resolved::Missing),
                Err(err) => return Err(FsError::io(&next, err)),
            };
            if !is_real_directory(&meta) {
                return Ok(Resolved::Missing);
            }
        }
        current = next;
    }
    Ok(if case_mismatch {
        Resolved::CaseMismatch(current)
    } else {
        Resolved::Found(current)
    })
}

enum ComponentMatch {
    Found(PathBuf),
    Missing,
    /// Exists only under a spelling that differs in case.
    CaseVariant(PathBuf),
}

fn resolve_component(
    dir: &Path,
    component: &str,
    confirm: bool,
) -> Result<ComponentMatch, FsError> {
    let direct = dir.join(component);
    match fs::symlink_metadata(&direct) {
        Ok(meta) => {
            if !confirm || spelling_confirmed(&direct, &meta, component) {
                return Ok(ComponentMatch::Found(direct));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(FsError::io(&direct, err)),
    }

    let (matches, case_variants) = matching_entries(dir, component)?;
    if let Some(first) = first_in_byte_order(matches) {
        return Ok(ComponentMatch::Found(first));
    }
    Ok(match first_in_byte_order(case_variants) {
        Some(variant) => ComponentMatch::CaseVariant(variant),
        None => ComponentMatch::Missing,
    })
}

/// Confirm the spelling of the whole resolved path at once: its canonical
/// form carries every on-disk name. `None` when that form cannot be trusted
/// (a link as the last component canonicalizes to its target; canonicalizing
/// or the root prefix strip failed), so the caller confirms per component.
fn confirm_whole_path(root: &Path, current: &Path, path: &LogicalPath) -> Option<Resolved> {
    let meta = fs::symlink_metadata(current).ok()?;
    if is_symlink_like(&meta) {
        return None;
    }
    let canonical = fs::canonicalize(current).ok()?;
    let canonical_root = fs::canonicalize(root).ok()?;
    let on_disk = canonical.strip_prefix(&canonical_root).ok()?;
    Some(match compare_spelling(on_disk, path)? {
        Spelling::Same => Resolved::Found(current.to_path_buf()),
        Spelling::Differs(on_disk) => Resolved::CaseMismatch(root.join(on_disk)),
    })
}

/// How the on-disk components of a canonical path (relative to the canonical
/// root) compare with the logical ones.
#[derive(Debug, PartialEq, Eq)]
enum Spelling {
    /// Every on-disk name equals its logical component after NFC.
    Same,
    /// At least one differs (in case, on a case-insensitive filesystem); the
    /// payload is the relative path as spelled on disk.
    Differs(PathBuf),
}

/// `None` when `on_disk` is not one ordinary UTF-8 name per logical
/// component, which the per-component walk sorts out.
fn compare_spelling(on_disk: &Path, path: &LogicalPath) -> Option<Spelling> {
    let mut names = on_disk.components();
    let mut spelled = PathBuf::new();
    let mut same = true;
    for component in path.components() {
        let Component::Normal(name) = names.next()? else {
            return None;
        };
        let name = name.to_str()?;
        same &= name.nfc().eq(component.chars());
        spelled.push(name);
    }
    if names.next().is_some() {
        return None;
    }
    Some(if same {
        Spelling::Same
    } else {
        Spelling::Differs(spelled)
    })
}

/// macOS and Windows default filesystems match names case-insensitively, so
/// a direct join there does not prove the spelling.
const CASE_INSENSITIVE_PLATFORM: bool = cfg!(any(windows, target_os = "macos"));

/// True when the entry `direct` hit is spelled `component` on disk. The
/// canonical path carries the on-disk name without listing the directory;
/// links cannot be canonicalized (that names their target), so they are
/// confirmed by the directory listing instead.
fn spelling_confirmed(direct: &Path, meta: &fs::Metadata, component: &str) -> bool {
    if is_symlink_like(meta) {
        return false;
    }
    let Ok(canonical) = fs::canonicalize(direct) else {
        return false;
    };
    canonical
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.nfc().eq(component.chars()))
}

/// Entries of `dir` whose NFC form equals `component`, and (on
/// case-insensitive platforms) those that equal it only after case folding.
fn matching_entries(dir: &Path, component: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>), FsError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(err) => return Err(FsError::io(dir, err)),
    };

    let folded = CASE_INSENSITIVE_PLATFORM.then(|| component.to_lowercase());
    let mut matches = Vec::new();
    let mut case_variants = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| FsError::io(dir, err))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let nfc: String = name.nfc().collect();
        if nfc == component {
            matches.push(entry.path());
        } else if let Some(folded) = &folded
            && nfc.to_lowercase() == *folded
        {
            case_variants.push(entry.path());
        }
    }
    Ok((matches, case_variants))
}

fn first_in_byte_order(mut paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.sort_by(|a, b| name_bytes(a).cmp(name_bytes(b)));
    paths.into_iter().next()
}

/// Walk from `root` down to `dir`, creating missing directories one component
/// at a time. Every *existing* ancestor must be a real directory (not a
/// symlink or junction).
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

/// Symlinks and junctions (name-surrogate reparse points, which std reports
/// as symlinks). Cloud Files placeholders and other reparse points are not
/// links: a sync root and its folders are real directories.
pub(crate) fn is_symlink_like(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
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

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_os_path_is_case_sensitive_on_linux() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("Dir")).unwrap();
        std::fs::write(root.join("Dir/File.txt"), b"x").unwrap();
        let exact = LogicalPath::new("Dir/File.txt").unwrap();
        assert_eq!(
            resolve_os_path_detailed(root, &exact).unwrap(),
            Resolved::Found(root.join("Dir/File.txt"))
        );
        for other in ["dir/File.txt", "Dir/file.txt", "DIR"] {
            let logical = LogicalPath::new(other).unwrap();
            assert_eq!(
                resolve_os_path_detailed(root, &logical).unwrap(),
                Resolved::Missing,
                "{other}"
            );
            assert_eq!(resolve_os_path(root, &logical).unwrap(), None, "{other}");
        }
    }

    /// The direct join finds `Foo` when `foo` is asked for; the on-disk
    /// spelling is reported and the logical path counts as missing.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn resolve_os_path_reports_case_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("case-probe"), b"").unwrap();
        if !root.join("CASE-PROBE").exists() {
            eprintln!("skipping: the temp volume is case-sensitive");
            return;
        }
        std::fs::create_dir(root.join("Dir")).unwrap();
        std::fs::write(root.join("Dir/File.txt"), b"x").unwrap();

        let exact = LogicalPath::new("Dir/File.txt").unwrap();
        assert_eq!(
            resolve_os_path_detailed(root, &exact).unwrap(),
            Resolved::Found(root.join("Dir/File.txt"))
        );
        for (other, on_disk) in [
            ("dir/File.txt", "Dir/File.txt"),
            ("Dir/file.txt", "Dir/File.txt"),
            ("DIR", "Dir"),
        ] {
            let logical = LogicalPath::new(other).unwrap();
            assert_eq!(
                resolve_os_path_detailed(root, &logical).unwrap(),
                Resolved::CaseMismatch(root.join(on_disk)),
                "{other}"
            );
            assert_eq!(resolve_os_path(root, &logical).unwrap(), None, "{other}");
        }
        let gone = LogicalPath::new("dir/gone.txt").unwrap();
        assert_eq!(
            resolve_os_path_detailed(root, &gone).unwrap(),
            Resolved::Missing
        );
    }

    /// The canonical path confirms the whole spelling at once on macOS and
    /// Windows; the comparison itself runs anywhere.
    #[test]
    fn compare_spelling_reports_the_component_that_differs() {
        let logical = LogicalPath::new("caf\u{e9}/Dir/File.txt").unwrap();
        // NFD on disk is the same name.
        assert_eq!(
            compare_spelling(Path::new("cafe\u{301}/Dir/File.txt"), &logical),
            Some(Spelling::Same)
        );
        assert_eq!(
            compare_spelling(Path::new("cafe\u{301}/dir/File.txt"), &logical),
            Some(Spelling::Differs(PathBuf::from("cafe\u{301}/dir/File.txt")))
        );
        assert_eq!(
            compare_spelling(Path::new("caf\u{e9}/Dir/FILE.TXT"), &logical),
            Some(Spelling::Differs(PathBuf::from("caf\u{e9}/Dir/FILE.TXT")))
        );
        // A different depth is left to the per-component walk.
        assert_eq!(compare_spelling(Path::new("caf\u{e9}/Dir"), &logical), None);
        assert_eq!(
            compare_spelling(Path::new("caf\u{e9}/Dir/File.txt/more"), &logical),
            None
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
