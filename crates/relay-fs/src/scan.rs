use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, FileType};
use std::path::{Path, PathBuf};

use relay_core::{EntryKind, LogicalPath, MOUNT_MARKER, StatHint, TEMP_PREFIX};
use relay_policy::{MountRules, PolicyError, parse_relayignore};
use walkdir::{DirEntry, WalkDir};

use crate::error::FsError;
use crate::paths::to_logical_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedEntry {
    pub path: LogicalPath,
    pub os_path: PathBuf,
    pub kind: EntryKind,
    pub stat: StatHint,
    pub executable: bool,
    pub symlink_target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanWarning {
    NonUtf8Name(PathBuf),
    InvalidName { os_path: PathBuf, reason: String },
    CaseCollision { a: LogicalPath, b: LogicalPath },
    NotPortable { path: LogicalPath, issue: String },
    Unreadable { os_path: PathBuf, error: String },
    SpecialFile(PathBuf),
    NestedMount(PathBuf),
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<ScannedEntry>,
    pub warnings: Vec<ScanWarning>,
    /// Rules actually applied (user rules plus a root `.relayignore`, if any).
    pub rules: Option<MountRules>,
}

/// User `rules` plus a root `.relayignore` when one is present and readable.
///
/// Unreadable `.relayignore` is recorded as a warning and the user rules are
/// returned unchanged, matching [`scan_mount`].
pub fn effective_rules(
    root: &Path,
    rules: &MountRules,
    warnings: &mut Vec<ScanWarning>,
) -> Result<MountRules, FsError> {
    match load_root_relayignore(root, warnings) {
        None => Ok(rules.clone()),
        Some(extra) => rules.with_extra_excludes(&extra).map_err(map_policy),
    }
}

/// Walk `root` applying `rules` (plus a root `.relayignore` if present).
///
/// The scanner does not check the mount marker; the caller should
/// [`crate::MountMarker::verify`] first. The marker file itself is skipped.
/// An error reading the root is fatal; per-entry IO errors become warnings.
pub fn scan_mount(root: &Path, rules: &MountRules) -> Result<ScanResult, FsError> {
    ensure_root(root)?;
    let mut result = ScanResult::default();
    let rules = effective_rules(root, rules, &mut result.warnings)?;

    let early = RefCell::new(WalkEarly::default());
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| filter_entry(root, entry, &rules, &early));

    for item in walker {
        match item {
            Err(err) => {
                let os_path = err.path().unwrap_or(root).to_path_buf();
                if err.depth() == 0 || os_path == root {
                    return Err(FsError::io(root, std::io::Error::other(err.to_string())));
                }
                result.warnings.push(ScanWarning::Unreadable {
                    os_path,
                    error: err.to_string(),
                });
            }
            Ok(entry) => {
                if entry.depth() == 0 {
                    continue;
                }
                if let Some(scanned) = collect_entry(root, &entry, &rules, &mut result.warnings)? {
                    result.entries.push(scanned);
                }
            }
        }
    }

    let WalkEarly {
        mut warnings,
        extra_symlinks,
    } = early.into_inner();
    result.warnings.append(&mut warnings);
    for os_path in extra_symlinks {
        if let Some(scanned) = collect_symlink_path(root, &os_path, &rules, &mut result.warnings) {
            result.entries.push(scanned);
        }
    }

    add_portability_warnings(&result.entries, &mut result.warnings);
    add_case_collision_warnings(&result.entries, &mut result.warnings);
    result.entries.sort_by(|a, b| a.path.cmp(&b.path));
    result.rules = Some(rules);
    Ok(result)
}

fn ensure_root(root: &Path) -> Result<(), FsError> {
    match fs::metadata(root) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(FsError::MountRootMissing(root.to_path_buf()))
        }
        Err(err) => Err(FsError::io(root, err)),
        Ok(meta) if !meta.is_dir() => Err(FsError::NotADirectory(root.to_path_buf())),
        Ok(_) => Ok(()),
    }
}

fn load_root_relayignore(root: &Path, warnings: &mut Vec<ScanWarning>) -> Option<Vec<String>> {
    let path = root.join(".relayignore");
    match fs::read_to_string(&path) {
        Ok(text) => Some(parse_relayignore(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            warnings.push(ScanWarning::Unreadable {
                os_path: path,
                error: err.to_string(),
            });
            None
        }
    }
}

#[derive(Default)]
struct WalkEarly {
    warnings: Vec<ScanWarning>,
    extra_symlinks: Vec<PathBuf>,
}

fn map_policy(err: PolicyError) -> FsError {
    match err {
        PolicyError::InvalidPattern { pattern, message } => {
            FsError::InvalidPattern { pattern, message }
        }
    }
}

fn filter_entry(
    root: &Path,
    entry: &DirEntry,
    rules: &MountRules,
    early: &RefCell<WalkEarly>,
) -> bool {
    if entry.depth() == 0 {
        return true;
    }

    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        early
            .borrow_mut()
            .warnings
            .push(ScanWarning::NonUtf8Name(entry.path().to_path_buf()));
        return false;
    };

    if name.starts_with(TEMP_PREFIX) {
        return false;
    }

    if entry.depth() == 1 && name == MOUNT_MARKER {
        return false;
    }

    if is_windows_reparse_dir(entry) {
        early
            .borrow_mut()
            .extra_symlinks
            .push(entry.path().to_path_buf());
        return false;
    }

    if entry.file_type().is_dir() {
        if entry.path().join(MOUNT_MARKER).is_file() {
            early
                .borrow_mut()
                .warnings
                .push(ScanWarning::NestedMount(entry.path().to_path_buf()));
            return false;
        }
        match to_logical_path(root, entry.path()) {
            Ok(dir) => rules.should_descend(&dir),
            Err(FsError::NonUtf8(path)) => {
                early
                    .borrow_mut()
                    .warnings
                    .push(ScanWarning::NonUtf8Name(path));
                false
            }
            Err(FsError::InvalidName { os_path, reason }) => {
                early
                    .borrow_mut()
                    .warnings
                    .push(ScanWarning::InvalidName { os_path, reason });
                false
            }
            Err(_) => false,
        }
    } else {
        true
    }
}

fn collect_entry(
    root: &Path,
    entry: &DirEntry,
    rules: &MountRules,
    warnings: &mut Vec<ScanWarning>,
) -> Result<Option<ScannedEntry>, FsError> {
    let os_path = entry.path().to_path_buf();
    let path = match to_logical_path(root, &os_path) {
        Ok(path) => path,
        Err(FsError::NonUtf8(path)) => {
            warnings.push(ScanWarning::NonUtf8Name(path));
            return Ok(None);
        }
        Err(FsError::InvalidName { os_path, reason }) => {
            warnings.push(ScanWarning::InvalidName { os_path, reason });
            return Ok(None);
        }
        Err(err) => return Err(err),
    };

    let file_type = entry.file_type();
    if is_special_file(&file_type) {
        warnings.push(ScanWarning::SpecialFile(os_path));
        return Ok(None);
    }

    let meta = match entry.metadata() {
        Ok(meta) => meta,
        Err(err) => {
            warnings.push(ScanWarning::Unreadable {
                os_path,
                error: err.to_string(),
            });
            return Ok(None);
        }
    };

    let (kind, symlink_target) = if file_type.is_symlink() {
        match read_symlink_target(&os_path) {
            Ok(target) => (EntryKind::Symlink, Some(target)),
            Err(warning) => {
                warnings.push(warning);
                return Ok(None);
            }
        }
    } else if file_type.is_dir() {
        (EntryKind::Directory, None)
    } else if file_type.is_file() {
        (EntryKind::File, None)
    } else {
        warnings.push(ScanWarning::SpecialFile(os_path));
        return Ok(None);
    };

    if !rules.is_selected(&path, kind) {
        return Ok(None);
    }

    let mut stat = StatHint::from_metadata(&meta);
    if kind == EntryKind::Directory {
        stat.size = 0;
    }

    let executable = kind == EntryKind::File && is_executable(&meta);

    Ok(Some(ScannedEntry {
        path,
        os_path,
        kind,
        stat,
        executable,
        symlink_target,
    }))
}

fn collect_symlink_path(
    root: &Path,
    os_path: &Path,
    rules: &MountRules,
    warnings: &mut Vec<ScanWarning>,
) -> Option<ScannedEntry> {
    let path = match to_logical_path(root, os_path) {
        Ok(path) => path,
        Err(FsError::NonUtf8(path)) => {
            warnings.push(ScanWarning::NonUtf8Name(path));
            return None;
        }
        Err(FsError::InvalidName { os_path, reason }) => {
            warnings.push(ScanWarning::InvalidName { os_path, reason });
            return None;
        }
        Err(_) => return None,
    };
    let target = match read_symlink_target(os_path) {
        Ok(target) => Some(target),
        Err(warning) => {
            warnings.push(warning);
            return None;
        }
    };
    if !rules.is_selected(&path, EntryKind::Symlink) {
        return None;
    }
    let meta = match fs::symlink_metadata(os_path) {
        Ok(meta) => meta,
        Err(err) => {
            warnings.push(ScanWarning::Unreadable {
                os_path: os_path.to_path_buf(),
                error: err.to_string(),
            });
            return None;
        }
    };
    Some(ScannedEntry {
        path,
        os_path: os_path.to_path_buf(),
        kind: EntryKind::Symlink,
        stat: StatHint::from_metadata(&meta),
        executable: false,
        symlink_target: target,
    })
}

fn read_symlink_target(os_path: &Path) -> Result<String, ScanWarning> {
    match fs::read_link(os_path) {
        Ok(target) => match target.to_str() {
            Some(s) => Ok(s.to_owned()),
            None => Err(ScanWarning::Unreadable {
                os_path: os_path.to_path_buf(),
                error: "symlink target is not valid UTF-8".to_owned(),
            }),
        },
        Err(err) => Err(ScanWarning::Unreadable {
            os_path: os_path.to_path_buf(),
            error: err.to_string(),
        }),
    }
}

fn add_portability_warnings(entries: &[ScannedEntry], warnings: &mut Vec<ScanWarning>) {
    for entry in entries {
        for issue in entry.path.portability_issues() {
            warnings.push(ScanWarning::NotPortable {
                path: entry.path.clone(),
                issue: issue.to_string(),
            });
        }
    }
}

fn add_case_collision_warnings(entries: &[ScannedEntry], warnings: &mut Vec<ScanWarning>) {
    let mut groups: BTreeMap<String, Vec<LogicalPath>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry(entry.path.case_fold_key())
            .or_default()
            .push(entry.path.clone());
    }
    for mut members in groups.into_values() {
        if members.len() < 2 {
            continue;
        }
        members.sort();
        let first = members[0].clone();
        for extra in members.into_iter().skip(1) {
            warnings.push(ScanWarning::CaseCollision {
                a: first.clone(),
                b: extra,
            });
        }
    }
}

fn is_special_file(file_type: &FileType) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        file_type.is_fifo()
            || file_type.is_socket()
            || file_type.is_block_device()
            || file_type.is_char_device()
    }
    #[cfg(not(unix))]
    {
        let _ = file_type;
        false
    }
}

fn is_executable(meta: &fs::Metadata) -> bool {
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

fn is_windows_reparse_dir(entry: &DirEntry) -> bool {
    #[cfg(windows)]
    {
        if !entry.file_type().is_dir() || entry.file_type().is_symlink() {
            return false;
        }
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        entry.metadata().ok().is_some_and(|meta| {
            use std::os::windows::fs::MetadataExt;
            meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        })
    }
    #[cfg(not(windows))]
    {
        let _ = entry;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use relay_core::{DeviceId, MountId, SpaceId};
    use tempfile::tempdir;

    use crate::marker::MountMarker;

    fn default_rules() -> MountRules {
        MountRules::new(&[], &[]).unwrap()
    }

    fn write_marker(root: &Path) {
        MountMarker {
            space: SpaceId::new(),
            mount: MountId::new(),
            created_by: DeviceId::random(),
        }
        .write(root)
        .unwrap();
    }

    fn paths(result: &ScanResult) -> Vec<String> {
        result
            .entries
            .iter()
            .map(|e| e.path.as_str().to_owned())
            .collect()
    }

    #[test]
    fn scan_errors_when_root_missing_or_not_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("gone");
        let err = scan_mount(&missing, &default_rules()).unwrap_err();
        assert!(matches!(err, FsError::MountRootMissing(_)));

        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let err = scan_mount(&file, &default_rules()).unwrap_err();
        assert!(matches!(err, FsError::NotADirectory(_)));
    }

    #[test]
    fn scan_fixture_tree() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);

        fs::create_dir_all(root.join("nested/dir")).unwrap();
        fs::write(root.join("nested/dir/file.txt"), b"hi").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join(".hidden"), b"dot").unwrap();

        fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), b"ref").unwrap();
        fs::write(root.join(".git/objects/ab/cd"), b"obj").unwrap();
        fs::write(root.join(".git/refs/heads/main"), b"abc").unwrap();
        fs::write(root.join(".git/index.lock"), b"lock").unwrap();
        fs::write(root.join(".git/refs/heads/main.lock"), b"lock").unwrap();

        fs::create_dir_all(root.join("node_modules/deep/secret")).unwrap();
        fs::write(root.join("node_modules/deep/secret/x"), b"nope").unwrap();
        let secret = root.join("node_modules/deep/secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&secret).unwrap().permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&secret, perms).unwrap();
        }

        fs::write(root.join(".relayignore"), "ignored.txt\nskip-me/**\n").unwrap();
        fs::write(root.join("ignored.txt"), b"no").unwrap();
        fs::create_dir_all(root.join("skip-me")).unwrap();
        fs::write(root.join("skip-me/a.txt"), b"no").unwrap();
        fs::write(root.join("keep.txt"), b"yes").unwrap();

        fs::write(root.join(format!("{TEMP_PREFIX}inflight")), b"tmp").unwrap();

        let nested = root.join("other-mount");
        fs::create_dir_all(nested.join("inside")).unwrap();
        fs::write(nested.join(MOUNT_MARKER), "nested").unwrap();
        fs::write(nested.join("inside/secret.txt"), b"no").unwrap();

        let rules = MountRules::new(&[], &["**/node_modules/**".to_owned()]).unwrap();
        let result = match scan_mount(root, &rules) {
            Ok(r) => r,
            Err(err) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
                }
                panic!("{err}");
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
        }

        let listed = paths(&result);
        assert!(listed.contains(&"nested/dir/file.txt".to_owned()));
        assert!(listed.contains(&"nested/dir".to_owned()));
        assert!(listed.contains(&"empty".to_owned()));
        assert!(listed.contains(&".hidden".to_owned()));
        assert!(listed.contains(&".git/HEAD".to_owned()));
        assert!(listed.contains(&".relayignore".to_owned()));
        assert!(listed.contains(&"keep.txt".to_owned()));
        assert!(!listed.iter().any(|p| p == MOUNT_MARKER));
        assert!(!listed.iter().any(|p| p.starts_with(".relay-tmp-")));
        assert!(
            !listed
                .iter()
                .any(|p| p.contains("index.lock") || p.ends_with(".lock"))
        );
        assert!(!listed.iter().any(|p| p.starts_with("node_modules")));
        assert!(!listed.contains(&"ignored.txt".to_owned()));
        assert!(!listed.iter().any(|p| p.starts_with("skip-me")));
        assert!(!listed.iter().any(|p| p.starts_with("other-mount")));
        assert!(!result.warnings.iter().any(
            |w| matches!(w, ScanWarning::Unreadable { os_path, .. } if os_path.starts_with(&secret))
        ));
        assert!(
            result
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::NestedMount(p) if p == &nested))
        );
        assert!(result.entries.windows(2).all(|w| w[0].path <= w[1].path));
        let effective = result.rules.expect("scan should report effective rules");
        assert!(!effective.is_selected(&LogicalPath::new("ignored.txt").unwrap(), EntryKind::File));
        assert!(!effective.is_selected(
            &LogicalPath::new("node_modules/deep/secret/x").unwrap(),
            EntryKind::File
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlink_executable_and_fifo() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);

        let outside = dir.path().join("outside-dir");
        fs::create_dir_all(outside.join("hidden")).unwrap();
        fs::write(outside.join("hidden/x"), b"no").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link-out")).unwrap();
        std::os::unix::fs::symlink("target.txt", root.join("rel-link")).unwrap();

        let bin = root.join("tool.sh");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();

        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());

        let result = scan_mount(root, &default_rules()).unwrap();
        let listed = paths(&result);
        assert!(listed.contains(&"link-out".to_owned()));
        assert!(listed.contains(&"rel-link".to_owned()));
        assert!(!listed.iter().any(|p| p.starts_with("link-out/")));

        let link = result
            .entries
            .iter()
            .find(|e| e.path.as_str() == "rel-link")
            .unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.symlink_target.as_deref(), Some("target.txt"));
        assert!(!link.executable);

        let tool = result
            .entries
            .iter()
            .find(|e| e.path.as_str() == "tool.sh")
            .unwrap();
        assert!(tool.executable);
        assert_eq!(tool.kind, EntryKind::File);

        assert!(
            result
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::SpecialFile(p) if p == &fifo))
        );
        assert!(!listed.contains(&"pipe".to_owned()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_case_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::write(root.join("Foo.lua"), b"A").unwrap();
        fs::write(root.join("foo.lua"), b"b").unwrap();
        let result = scan_mount(root, &default_rules()).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::CaseCollision { .. }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn not_portable_names() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::write(root.join("aux.txt"), b"x").unwrap();
        fs::write(root.join("what?.md"), b"y").unwrap();
        let result = scan_mount(root, &default_rules()).unwrap();
        let issues: Vec<_> = result
            .warnings
            .iter()
            .filter_map(|w| match w {
                ScanWarning::NotPortable { path, .. } => Some(path.as_str().to_owned()),
                _ => None,
            })
            .collect();
        assert!(issues.iter().any(|p| p == "aux.txt"));
        assert!(issues.iter().any(|p| p == "what?.md"));
        assert!(paths(&result).contains(&"aux.txt".to_owned()));
        assert!(paths(&result).contains(&"what?.md".to_owned()));
    }
}
