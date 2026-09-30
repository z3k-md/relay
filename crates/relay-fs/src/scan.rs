use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, FileType};
use std::path::{Path, PathBuf};

use relay_core::{
    EntryKind, LogicalPath, MOUNT_MARKER, StatHint, is_bookkeeping_component, is_bookkeeping_path,
};
use relay_policy::{MountRules, PolicyError, parse_relayignore};
use walkdir::{DirEntry, WalkDir};

use crate::error::FsError;
use crate::paths::{resolve_os_path, to_logical_path};

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
    InvalidName {
        os_path: PathBuf,
        reason: String,
    },
    CaseCollision {
        a: LogicalPath,
        b: LogicalPath,
    },
    NotPortable {
        path: LogicalPath,
        issue: String,
    },
    Unreadable {
        os_path: PathBuf,
        error: String,
    },
    SpecialFile(PathBuf),
    NestedMount(PathBuf),
    NormalizationCollision {
        path: LogicalPath,
        os_paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<ScannedEntry>,
    pub warnings: Vec<ScanWarning>,
    /// Rules actually applied (user rules plus a root `.relayignore`, if any).
    pub rules: Option<MountRules>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Exact,
    Subtree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanScope {
    pub path: LogicalPath,
    pub kind: ScopeKind,
}

#[derive(Debug)]
pub struct PartialScan {
    pub scopes: Vec<ScanScope>,
    pub entries: Vec<ScannedEntry>,
    pub warnings: Vec<ScanWarning>,
    pub rules: MountRules,
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
    scan_mount_with(root, rules, &mut || true)
}

/// Like [`scan_mount`], calling `on_entry` once per collected entry during the walk.
///
/// `on_entry` returns `false` to stop the walk early. The returned entries are
/// then only what was visited; callers must not treat that as a complete mount.
pub fn scan_mount_with(
    root: &Path,
    rules: &MountRules,
    on_entry: &mut dyn FnMut() -> bool,
) -> Result<ScanResult, FsError> {
    ensure_root(root)?;
    let mut result = ScanResult::default();
    let rules = effective_rules(root, rules, &mut result.warnings)?;
    walk_tree(
        root,
        root,
        &rules,
        false,
        &mut result.entries,
        &mut result.warnings,
        on_entry,
    )?;
    finalize_scan(&mut result.entries, &mut result.warnings);
    result.rules = Some(rules);
    Ok(result)
}

/// Observe only `paths` (mount-relative). The caller treats previously indexed
/// entries inside a scope that are absent from `entries` as deleted, so scopes
/// must be exact about what was actually examined.
pub fn scan_paths(
    root: &Path,
    rules: &MountRules,
    paths: &[LogicalPath],
) -> Result<PartialScan, FsError> {
    ensure_root(root)?;
    let mut warnings = Vec::new();
    let rules = effective_rules(root, rules, &mut warnings)?;
    let requested = coalesce_requests(root, paths);

    let mut scopes = Vec::new();
    let mut entries = Vec::new();
    for path in &requested {
        examine_requested(
            root,
            path,
            &rules,
            &mut PartialSink {
                scopes: &mut scopes,
                entries: &mut entries,
                warnings: &mut warnings,
            },
        )?;
    }
    emit_ancestor_directories(root, &rules, &mut scopes, &mut entries);

    finalize_scan(&mut entries, &mut warnings);
    finalize_scopes(&mut scopes);
    Ok(PartialScan {
        scopes,
        entries,
        warnings,
        rules,
    })
}

fn finalize_scan(entries: &mut Vec<ScannedEntry>, warnings: &mut Vec<ScanWarning>) {
    resolve_normalization_collisions(entries, warnings);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    add_portability_warnings(entries, warnings);
    add_case_collision_warnings(entries, warnings);
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

fn walk_tree(
    root: &Path,
    start: &Path,
    rules: &MountRules,
    include_start: bool,
    entries: &mut Vec<ScannedEntry>,
    warnings: &mut Vec<ScanWarning>,
    on_entry: &mut dyn FnMut() -> bool,
) -> Result<(), FsError> {
    let early = RefCell::new(WalkEarly::default());
    let walker = WalkDir::new(start)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| filter_entry(root, entry, rules, &early));

    for item in walker {
        match item {
            Err(err) => {
                let os_path = err.path().unwrap_or(start).to_path_buf();
                if os_path == root || (err.depth() == 0 && start == root) {
                    return Err(FsError::io(root, std::io::Error::other(err.to_string())));
                }
                warnings.push(ScanWarning::Unreadable {
                    os_path,
                    error: err.to_string(),
                });
            }
            Ok(entry) => {
                if entry.depth() == 0 && !include_start {
                    continue;
                }
                if let Some(scanned) = collect_entry(root, &entry, rules, warnings)? {
                    entries.push(scanned);
                    if !on_entry() {
                        return Ok(());
                    }
                }
            }
        }
    }

    let WalkEarly {
        warnings: mut early_warnings,
        extra_symlinks,
    } = early.into_inner();
    warnings.append(&mut early_warnings);
    for os_path in extra_symlinks {
        if let Some(scanned) = collect_symlink_path(root, &os_path, rules, warnings) {
            entries.push(scanned);
            if !on_entry() {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn coalesce_requests(root: &Path, paths: &[LogicalPath]) -> Vec<LogicalPath> {
    let mut requested: Vec<LogicalPath> = paths.to_vec();
    requested.sort();
    requested.dedup();
    let candidates = requested.clone();
    requested.retain(|path| {
        !candidates
            .iter()
            .any(|other| other != path && path.starts_with(other) && covers_as_subtree(root, other))
    });
    requested
}

fn covers_as_subtree(root: &Path, path: &LogicalPath) -> bool {
    match resolve_os_path(root, path) {
        Ok(None) => true,
        Ok(Some(os_path)) => match fs::symlink_metadata(&os_path) {
            Ok(meta) => is_real_directory(&meta),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            _ => false,
        },
        Err(_) => false,
    }
}

struct PartialSink<'a> {
    scopes: &'a mut Vec<ScanScope>,
    entries: &'a mut Vec<ScannedEntry>,
    warnings: &'a mut Vec<ScanWarning>,
}

fn examine_requested(
    root: &Path,
    path: &LogicalPath,
    rules: &MountRules,
    sink: &mut PartialSink<'_>,
) -> Result<(), FsError> {
    if skip_requested_path(root, path, rules) {
        return Ok(());
    }

    let os_path = match resolve_os_path(root, path) {
        Ok(None) => {
            sink.scopes.push(ScanScope {
                path: path.clone(),
                kind: ScopeKind::Subtree,
            });
            return Ok(());
        }
        Ok(Some(os_path)) => os_path,
        Err(FsError::Io {
            path: os_path,
            source,
        }) => {
            sink.warnings.push(ScanWarning::Unreadable {
                os_path,
                error: source.to_string(),
            });
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    match fs::symlink_metadata(&os_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            sink.scopes.push(ScanScope {
                path: path.clone(),
                kind: ScopeKind::Subtree,
            });
        }
        Err(err) => {
            sink.warnings.push(ScanWarning::Unreadable {
                os_path,
                error: err.to_string(),
            });
        }
        Ok(meta) => examine_existing(root, path, &os_path, &meta, rules, sink)?,
    }
    Ok(())
}

fn skip_requested_path(root: &Path, path: &LogicalPath, rules: &MountRules) -> bool {
    if is_bookkeeping_component(path.file_name()) {
        return true;
    }
    ancestor_blocks(root, path, rules)
}

fn ancestor_blocks(root: &Path, path: &LogicalPath, rules: &MountRules) -> bool {
    let mut current = path.parent();
    while let Some(ancestor) = current {
        if !rules.should_descend(&ancestor) {
            return true;
        }
        match resolve_os_path(root, &ancestor) {
            Ok(None) => {}
            Err(_) => return true,
            Ok(Some(os_path)) => match fs::symlink_metadata(&os_path) {
                Err(_) => return true,
                Ok(meta) => {
                    if !is_real_directory(&meta) {
                        return true;
                    }
                    if contains_mount_marker(&os_path) {
                        return true;
                    }
                }
            },
        }
        current = ancestor.parent();
    }
    false
}

fn examine_existing(
    root: &Path,
    path: &LogicalPath,
    os_path: &Path,
    meta: &fs::Metadata,
    rules: &MountRules,
    sink: &mut PartialSink<'_>,
) -> Result<(), FsError> {
    let file_type = meta.file_type();
    if is_special_file(&file_type) {
        sink.warnings
            .push(ScanWarning::SpecialFile(os_path.to_path_buf()));
        return Ok(());
    }

    if is_real_directory(meta) {
        if contains_mount_marker(os_path) {
            sink.warnings
                .push(ScanWarning::NestedMount(os_path.to_path_buf()));
            return Ok(());
        }
        if !rules.should_descend(path) {
            return Ok(());
        }
        sink.scopes.push(ScanScope {
            path: path.clone(),
            kind: ScopeKind::Subtree,
        });
        walk_tree(
            root,
            os_path,
            rules,
            true,
            sink.entries,
            sink.warnings,
            &mut || true,
        )?;
        return Ok(());
    }

    if let Some(scanned) = collect_from_metadata(root, os_path, meta, rules, sink.warnings)? {
        sink.entries.push(scanned);
        sink.scopes.push(ScanScope {
            path: path.clone(),
            kind: ScopeKind::Exact,
        });
    }
    Ok(())
}

fn emit_ancestor_directories(
    root: &Path,
    rules: &MountRules,
    scopes: &mut Vec<ScanScope>,
    entries: &mut Vec<ScannedEntry>,
) {
    let existing: Vec<LogicalPath> = scopes
        .iter()
        .filter(|scope| resolve_os_path(root, &scope.path).ok().flatten().is_some())
        .map(|scope| scope.path.clone())
        .collect();

    for path in existing {
        let mut current = path.parent();
        while let Some(ancestor) = current {
            maybe_emit_ancestor_dir(root, &ancestor, rules, scopes, entries);
            current = ancestor.parent();
        }
    }
}

fn maybe_emit_ancestor_dir(
    root: &Path,
    ancestor: &LogicalPath,
    rules: &MountRules,
    scopes: &mut Vec<ScanScope>,
    entries: &mut Vec<ScannedEntry>,
) {
    if scopes.iter().any(|scope| scope.path == *ancestor) {
        return;
    }
    let Ok(Some(os_path)) = resolve_os_path(root, ancestor) else {
        return;
    };
    let Ok(meta) = fs::symlink_metadata(&os_path) else {
        return;
    };
    if !is_real_directory(&meta) || !rules.is_selected(ancestor, EntryKind::Directory) {
        return;
    }
    if !entries.iter().any(|entry| entry.path == *ancestor) {
        let mut stat = StatHint::from_metadata(&meta);
        stat.size = 0;
        entries.push(ScannedEntry {
            path: ancestor.clone(),
            os_path,
            kind: EntryKind::Directory,
            stat,
            executable: false,
            symlink_target: None,
        });
    }
    scopes.push(ScanScope {
        path: ancestor.clone(),
        kind: ScopeKind::Exact,
    });
}

fn finalize_scopes(scopes: &mut Vec<ScanScope>) {
    scopes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| scope_kind_rank(a.kind).cmp(&scope_kind_rank(b.kind)))
    });
    scopes.dedup();
}

fn scope_kind_rank(kind: ScopeKind) -> u8 {
    match kind {
        ScopeKind::Exact => 0,
        ScopeKind::Subtree => 1,
    }
}

fn contains_mount_marker(dir: &Path) -> bool {
    dir.join(MOUNT_MARKER).is_file()
}

fn is_real_directory(meta: &fs::Metadata) -> bool {
    meta.is_dir() && !is_symlink_or_junction(meta)
}

fn is_symlink_or_junction(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    is_windows_reparse_meta(meta)
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

    if is_bookkeeping_component(name) {
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
        if contains_mount_marker(entry.path()) {
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

    finish_entry(path, os_path, &file_type, &meta, rules, warnings)
}

fn collect_from_metadata(
    root: &Path,
    os_path: &Path,
    meta: &fs::Metadata,
    rules: &MountRules,
    warnings: &mut Vec<ScanWarning>,
) -> Result<Option<ScannedEntry>, FsError> {
    let path = match to_logical_path(root, os_path) {
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
    let file_type = meta.file_type();
    if is_special_file(&file_type) {
        warnings.push(ScanWarning::SpecialFile(os_path.to_path_buf()));
        return Ok(None);
    }
    finish_entry(
        path,
        os_path.to_path_buf(),
        &file_type,
        meta,
        rules,
        warnings,
    )
}

fn finish_entry(
    path: LogicalPath,
    os_path: PathBuf,
    file_type: &FileType,
    meta: &fs::Metadata,
    rules: &MountRules,
    warnings: &mut Vec<ScanWarning>,
) -> Result<Option<ScannedEntry>, FsError> {
    let (kind, symlink_target) = if file_type.is_symlink() || is_windows_reparse_meta(meta) {
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

    if is_bookkeeping_path(&path) {
        return Ok(None);
    }

    if !rules.is_selected(&path, kind) {
        return Ok(None);
    }

    let mut stat = StatHint::from_metadata(meta);
    if kind == EntryKind::Directory {
        stat.size = 0;
    }

    let executable = kind == EntryKind::File && is_executable(meta);

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

fn resolve_normalization_collisions(
    entries: &mut Vec<ScannedEntry>,
    warnings: &mut Vec<ScanWarning>,
) {
    let mut groups: BTreeMap<LogicalPath, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        groups.entry(entry.path.clone()).or_default().push(index);
    }

    let mut drop_at = Vec::new();
    for (path, indices) in groups {
        if indices.len() < 2 {
            continue;
        }
        let mut ordered = indices;
        ordered.sort_by(|&a, &b| {
            os_path_key(&entries[a].os_path).cmp(&os_path_key(&entries[b].os_path))
        });
        let os_paths = ordered
            .iter()
            .map(|&i| entries[i].os_path.clone())
            .collect();
        drop_at.extend(ordered.into_iter().skip(1));
        warnings.push(ScanWarning::NormalizationCollision { path, os_paths });
    }

    if drop_at.is_empty() {
        return;
    }
    drop_at.sort_unstable();
    drop_at.dedup();
    let mut drop_iter = drop_at.into_iter().peekable();
    let mut keep = 0;
    for index in 0..entries.len() {
        if drop_iter.peek() == Some(&index) {
            drop_iter.next();
            continue;
        }
        if keep != index {
            entries.swap(keep, index);
        }
        keep += 1;
    }
    entries.truncate(keep);
}

fn os_path_key(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
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
        entry
            .metadata()
            .ok()
            .is_some_and(|meta| is_windows_reparse_meta(&meta))
    }
    #[cfg(not(windows))]
    {
        let _ = entry;
        false
    }
}

fn is_windows_reparse_meta(meta: &fs::Metadata) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use relay_core::{DeviceId, MountId, SpaceId, TEMP_PREFIX};
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

    #[cfg(unix)]
    fn unreadable_dirs_still_readable() -> bool {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let secret = dir.path().join("s");
        fs::create_dir(&secret).unwrap();
        let mut perms = fs::metadata(&secret).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&secret, perms).unwrap();
        let readable = fs::read_dir(&secret).is_ok();
        let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
        readable
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
    fn linux_normalization_collision() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        let nfc = root.join("caf\u{e9}.txt");
        let nfd = root.join("cafe\u{301}.txt");
        fs::write(&nfc, b"nfc").unwrap();
        fs::write(&nfd, b"nfd").unwrap();
        let result = scan_mount(root, &default_rules()).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path.as_str(), "caf\u{e9}.txt");
        let mut expected_os = [nfc, nfd];
        expected_os.sort_by_key(|a| os_path_key(a));
        assert_eq!(result.entries[0].os_path, expected_os[0]);
        let collision = result
            .warnings
            .iter()
            .find_map(|w| match w {
                ScanWarning::NormalizationCollision { path, os_paths } => {
                    Some((path.clone(), os_paths.clone()))
                }
                _ => None,
            })
            .expect("normalization collision warning");
        assert_eq!(collision.0.as_str(), "caf\u{e9}.txt");
        assert_eq!(collision.1.len(), 2);
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

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).unwrap()
    }

    fn has_scope(scan: &PartialScan, path: &str, kind: ScopeKind) -> bool {
        scan.scopes
            .iter()
            .any(|scope| scope.path.as_str() == path && scope.kind == kind)
    }

    fn assert_entries_match(got: &[ScannedEntry], expected: &[ScannedEntry]) {
        // Directory stats are unused by the engine, and on Windows a directory
        // listed from its parent reports NTFS's lazily updated cached mtime,
        // which differs from reading the directory itself.
        let summarize = |entries: &[ScannedEntry]| {
            entries
                .iter()
                .map(|e| {
                    (
                        e.path.as_str().to_owned(),
                        e.kind,
                        (e.kind != EntryKind::Directory).then_some(e.stat),
                        e.symlink_target.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(summarize(got), summarize(expected));
    }

    fn fixture_tree(root: &Path) -> MountRules {
        write_marker(root);
        fs::create_dir_all(root.join("nested/dir")).unwrap();
        fs::write(root.join("nested/dir/file.txt"), b"hi").unwrap();
        fs::write(root.join("nested/dir/other.txt"), b"ho").unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join(".hidden"), b"dot").unwrap();
        fs::create_dir_all(root.join(".git/objects/ab")).unwrap();
        fs::write(root.join(".git/HEAD"), b"ref").unwrap();
        fs::write(root.join(".git/objects/ab/cd"), b"obj").unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/x"), b"nope").unwrap();
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
        fs::write(root.join("alpha.txt"), b"a").unwrap();
        fs::write(root.join("zeta.txt"), b"z").unwrap();
        MountRules::new(&[], &["**/node_modules/**".to_owned()]).unwrap()
    }

    fn top_level_logical_paths(root: &Path) -> Vec<LogicalPath> {
        let mut out = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if let Ok(path) = to_logical_path(root, &entry.path()) {
                out.push(path);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn scan_paths_errors_when_root_missing_or_not_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("gone");
        let err = scan_paths(&missing, &default_rules(), &[]).unwrap_err();
        assert!(matches!(err, FsError::MountRootMissing(_)));

        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        let err = scan_paths(&file, &default_rules(), &[]).unwrap_err();
        assert!(matches!(err, FsError::NotADirectory(_)));
    }

    #[test]
    fn scan_paths_top_level_children_match_full_scan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let rules = fixture_tree(root);
        let full = scan_mount(root, &rules).unwrap();
        let children = top_level_logical_paths(root);
        assert!(!children.is_empty());
        let partial = scan_paths(root, &rules, &children).unwrap();
        assert_entries_match(&partial.entries, &full.entries);
        assert!(partial.scopes.windows(2).all(|w| w[0].path <= w[1].path));
        assert!(partial.entries.windows(2).all(|w| w[0].path <= w[1].path));
    }

    #[test]
    fn scan_paths_subdir_matches_full_scan_subset_plus_ancestors() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let rules = fixture_tree(root);
        let full = scan_mount(root, &rules).unwrap();
        let scope = lp("nested/dir");
        let partial = scan_paths(root, &rules, std::slice::from_ref(&scope)).unwrap();
        let expected: Vec<ScannedEntry> = full
            .entries
            .iter()
            .filter(|entry| entry.path.starts_with(&scope) || scope.starts_with(&entry.path))
            .cloned()
            .collect();
        assert_entries_match(&partial.entries, &expected);
        assert!(has_scope(&partial, "nested/dir", ScopeKind::Subtree));
        assert!(has_scope(&partial, "nested", ScopeKind::Exact));
    }

    #[test]
    fn scan_paths_dedups_and_drops_paths_under_dir_or_missing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.txt"), b"x").unwrap();
        fs::write(root.join("keep.txt"), b"y").unwrap();

        let partial = scan_paths(
            root,
            &default_rules(),
            &[lp("a"), lp("a"), lp("a/b"), lp("a/b/c.txt"), lp("keep.txt")],
        )
        .unwrap();
        assert!(has_scope(&partial, "a", ScopeKind::Subtree));
        assert!(!has_scope(&partial, "a/b", ScopeKind::Subtree));
        assert!(!has_scope(&partial, "a/b/c.txt", ScopeKind::Exact));
        assert!(has_scope(&partial, "keep.txt", ScopeKind::Exact));
        assert!(
            partial
                .entries
                .iter()
                .any(|e| e.path.as_str() == "a/b/c.txt")
        );

        let missing = scan_paths(root, &default_rules(), &[lp("gone"), lp("gone/child")]).unwrap();
        assert_eq!(missing.scopes.len(), 1);
        assert!(has_scope(&missing, "gone", ScopeKind::Subtree));
        assert!(missing.entries.is_empty());
    }

    #[test]
    fn scan_paths_skips_when_ancestor_excluded_or_nested_mount() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let rules = fixture_tree(root);

        let excluded = scan_paths(root, &rules, &[lp("skip-me/a.txt")]).unwrap();
        assert!(excluded.scopes.is_empty());
        assert!(excluded.entries.is_empty());

        let pruned = scan_paths(root, &rules, &[lp("skip-me")]).unwrap();
        assert!(pruned.scopes.is_empty());

        let nested = scan_paths(root, &rules, &[lp("other-mount/inside/secret.txt")]).unwrap();
        assert!(nested.scopes.is_empty());
        assert!(nested.entries.is_empty());

        let nested_dir = scan_paths(root, &rules, &[lp("other-mount")]).unwrap();
        assert!(nested_dir.scopes.is_empty());
        assert!(
            nested_dir
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::NestedMount(_)))
        );
    }

    #[test]
    fn scan_paths_skips_temp_prefix_and_root_marker() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::write(root.join(format!("{TEMP_PREFIX}inflight")), b"tmp").unwrap();
        let partial = scan_paths(
            root,
            &default_rules(),
            &[lp(MOUNT_MARKER), lp(&format!("{TEMP_PREFIX}inflight"))],
        )
        .unwrap();
        assert!(partial.scopes.is_empty());
        assert!(partial.entries.is_empty());
    }

    #[test]
    fn scan_paths_missing_is_subtree_even_if_parent_missing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        let partial = scan_paths(root, &default_rules(), &[lp("gone/child")]).unwrap();
        assert_eq!(partial.scopes.len(), 1);
        assert!(has_scope(&partial, "gone/child", ScopeKind::Subtree));
        assert!(partial.entries.is_empty());
    }

    #[test]
    fn scan_paths_file_exact_and_not_selected() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::write(root.join(".relayignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("keep.txt"), b"yes").unwrap();
        fs::write(root.join("ignored.txt"), b"no").unwrap();

        let selected = scan_paths(root, &default_rules(), &[lp("keep.txt")]).unwrap();
        assert!(has_scope(&selected, "keep.txt", ScopeKind::Exact));
        assert_eq!(selected.entries.len(), 1);
        assert_eq!(selected.entries[0].path.as_str(), "keep.txt");
        assert_eq!(selected.entries[0].kind, EntryKind::File);

        let ignored = scan_paths(root, &default_rules(), &[lp("ignored.txt")]).unwrap();
        assert!(ignored.scopes.is_empty());
        assert!(ignored.entries.is_empty());
    }

    #[test]
    fn scan_paths_directory_walk_includes_self_when_selected() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::create_dir_all(root.join("nested/dir")).unwrap();
        fs::write(root.join("nested/dir/file.txt"), b"hi").unwrap();
        let partial = scan_paths(root, &default_rules(), &[lp("nested")]).unwrap();
        assert!(has_scope(&partial, "nested", ScopeKind::Subtree));
        let listed: Vec<_> = partial.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(listed.contains(&"nested"));
        assert!(listed.contains(&"nested/dir"));
        assert!(listed.contains(&"nested/dir/file.txt"));
    }

    #[test]
    fn scan_paths_emits_selected_ancestors_for_existing_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.txt"), b"x").unwrap();
        let partial = scan_paths(root, &default_rules(), &[lp("a/b/c.txt")]).unwrap();
        assert!(has_scope(&partial, "a/b/c.txt", ScopeKind::Exact));
        assert!(has_scope(&partial, "a", ScopeKind::Exact));
        assert!(has_scope(&partial, "a/b", ScopeKind::Exact));
        let listed: Vec<_> = partial.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(listed, ["a", "a/b", "a/b/c.txt"]);
    }

    #[test]
    fn scan_paths_skips_when_ancestor_is_not_a_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::write(root.join("plain.txt"), b"x").unwrap();
        let partial = scan_paths(root, &default_rules(), &[lp("plain.txt/child")]).unwrap();
        assert!(partial.scopes.is_empty());
        assert!(partial.entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_skips_when_ancestor_is_symlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::create_dir_all(root.join("real")).unwrap();
        fs::write(root.join("real/file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let through_link = scan_paths(root, &default_rules(), &[lp("link/file.txt")]).unwrap();
        assert!(through_link.scopes.is_empty());
        assert!(through_link.entries.is_empty());

        let link_itself = scan_paths(root, &default_rules(), &[lp("link")]).unwrap();
        assert!(has_scope(&link_itself, "link", ScopeKind::Exact));
        assert_eq!(link_itself.entries.len(), 1);
        assert_eq!(link_itself.entries[0].kind, EntryKind::Symlink);
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_special_file_warns_without_scope() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let partial = scan_paths(root, &default_rules(), &[lp("pipe")]).unwrap();
        assert!(partial.scopes.is_empty());
        assert!(partial.entries.is_empty());
        assert!(
            partial
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::SpecialFile(p) if p == &fifo))
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_paths_unreadable_path_and_subdir() {
        use std::os::unix::fs::PermissionsExt;

        if unreadable_dirs_still_readable() {
            eprintln!(
                "skipping scan_paths_unreadable_path_and_subdir: running with permissions that bypass chmod"
            );
            return;
        }

        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("locked/hidden.txt"), b"x").unwrap();
        let locked = root.join("locked");
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms).unwrap();

        let child = scan_paths(root, &default_rules(), &[lp("locked/hidden.txt")]);
        let subtree = scan_paths(root, &default_rules(), &[lp("locked")]);

        let restore = fs::Permissions::from_mode(0o755);
        let _ = fs::set_permissions(&locked, restore);

        let child = child.unwrap();
        assert!(child.scopes.is_empty());
        assert!(
            child
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::Unreadable { .. }))
        );

        let subtree = subtree.unwrap();
        assert!(has_scope(&subtree, "locked", ScopeKind::Subtree));
        assert!(
            subtree
                .warnings
                .iter()
                .any(|w| matches!(w, ScanWarning::Unreadable { .. }))
        );
    }

    #[test]
    fn scan_paths_deleted_file_is_empty_subtree() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        let partial = scan_paths(root, &default_rules(), &[lp("deleted.txt")]).unwrap();
        assert_eq!(partial.scopes.len(), 1);
        assert!(has_scope(&partial, "deleted.txt", ScopeKind::Subtree));
        assert!(partial.entries.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_paths_nfd_file_via_nfc_logical_is_exact() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_marker(root);
        let nfd = root.join("cafe\u{301}.txt");
        fs::write(&nfd, b"nfd").unwrap();
        let logical = lp("caf\u{e9}.txt");
        let partial = scan_paths(root, &default_rules(), std::slice::from_ref(&logical)).unwrap();
        assert!(has_scope(&partial, "caf\u{e9}.txt", ScopeKind::Exact));
        assert!(!has_scope(&partial, "caf\u{e9}.txt", ScopeKind::Subtree));
        assert_eq!(partial.entries.len(), 1);
        assert_eq!(partial.entries[0].path.as_str(), "caf\u{e9}.txt");
        assert_eq!(partial.entries[0].os_path, nfd);
        assert_eq!(partial.entries[0].kind, EntryKind::File);
    }
}
