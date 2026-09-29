use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use proptest::prelude::*;
use relay_core::{EntryContent, LogicalPath, MOUNT_MARKER, ObjectId, TEMP_PREFIX};
use relay_engine::{Engine, EngineError, ScanOptions};
use tempfile::TempDir;

fn new_home() -> TempDir {
    TempDir::new().unwrap()
}

fn ready(home: &Path, mount: &Path) -> Engine {
    let mut engine = Engine::init(home, "testdev").unwrap();
    engine.create_space("Personal").unwrap();
    engine
        .add_mount("Personal", "code", mount, &[], &[])
        .unwrap();
    engine
}

fn lp(path: &str) -> LogicalPath {
    LogicalPath::new(path).unwrap()
}

fn scan_paths(engine: &mut Engine, paths: &[&str]) -> relay_engine::ScanReport {
    let logical: Vec<LogicalPath> = paths.iter().map(|p| lp(p)).collect();
    engine
        .scan_paths("Personal", "code", &logical, ScanOptions::default())
        .unwrap()
}

fn live_paths(engine: &Engine) -> Vec<String> {
    let mut paths: Vec<String> = engine
        .entries("Personal", "code", false)
        .unwrap()
        .into_iter()
        .map(|e| e.key.path.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

fn entry_at(engine: &Engine, path: &str) -> Option<relay_engine::EntryRecord> {
    engine
        .entries("Personal", "code", true)
        .unwrap()
        .into_iter()
        .find(|e| e.key.path.as_str() == path)
}

#[test]
fn scan_paths_create_modify_delete_file() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());

    fs::write(mount.path().join("a.txt"), b"v1").unwrap();
    let report = scan_paths(&mut engine, &["a.txt"]);
    assert_eq!(report.created, 1, "{report:?}");
    assert_eq!(live_paths(&engine), ["a.txt"]);

    fs::write(mount.path().join("a.txt"), b"v2-longer").unwrap();
    let report = scan_paths(&mut engine, &["a.txt"]);
    assert_eq!(report.modified, 1, "{report:?}");
    assert_eq!(
        entry_at(&engine, "a.txt").unwrap().content.object(),
        Some(ObjectId::of(b"v2-longer"))
    );

    fs::remove_file(mount.path().join("a.txt")).unwrap();
    let report = scan_paths(&mut engine, &["a.txt"]);
    assert_eq!(report.deleted, 1, "{report:?}");
    assert!(entry_at(&engine, "a.txt").unwrap().is_deleted());
}

#[test]
fn scan_paths_delete_subtree_leaves_siblings() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());

    fs::create_dir_all(mount.path().join("a/b")).unwrap();
    fs::write(mount.path().join("a/b/c.txt"), b"c").unwrap();
    fs::write(mount.path().join("a-b"), b"dash").unwrap();
    fs::write(mount.path().join("ab"), b"ab").unwrap();
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();

    fs::remove_dir_all(mount.path().join("a/b")).unwrap();
    let report = scan_paths(&mut engine, &["a/b"]);
    assert!(report.deleted >= 2, "{report:?}");

    let live = live_paths(&engine);
    assert!(live.contains(&"a-b".to_owned()), "{live:?}");
    assert!(live.contains(&"ab".to_owned()), "{live:?}");
    assert!(!live.iter().any(|p| p == "a/b" || p.starts_with("a/b/")));
    assert!(entry_at(&engine, "a/b").unwrap().is_deleted());
    assert!(entry_at(&engine, "a/b/c.txt").unwrap().is_deleted());
    assert!(!entry_at(&engine, "a-b").unwrap().is_deleted());
    assert!(!entry_at(&engine, "ab").unwrap().is_deleted());
}

#[test]
fn scan_paths_nested_create_emits_ancestors() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());

    fs::create_dir_all(mount.path().join("a/b")).unwrap();
    fs::write(mount.path().join("a/b/c.txt"), b"c").unwrap();
    let report = scan_paths(&mut engine, &["a/b/c.txt"]);
    assert_eq!(report.created, 3, "{report:?}");
    assert_eq!(live_paths(&engine), ["a", "a/b", "a/b/c.txt"]);
    assert!(matches!(
        entry_at(&engine, "a").unwrap().content,
        EntryContent::Directory
    ));
}

#[test]
fn scan_paths_excluded_path_is_ignored() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::create_dir_all(mount.path().join("skip")).unwrap();
    fs::write(mount.path().join("skip/secret.txt"), b"no").unwrap();
    fs::write(mount.path().join(".relayignore"), "skip/**\n").unwrap();

    let report = scan_paths(&mut engine, &["skip/secret.txt"]);
    assert_eq!(report.created, 0, "{report:?}");
    assert!(!live_paths(&engine).iter().any(|p| p.starts_with("skip")));
}

#[cfg(unix)]
#[test]
fn scan_paths_unreadable_subdir_is_protected() {
    use std::os::unix::fs::PermissionsExt;

    if unreadable_dirs_still_readable() {
        eprintln!(
            "skipping scan_paths_unreadable_subdir_is_protected: running with permissions that bypass chmod"
        );
        return;
    }

    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    let secret = mount.path().join("secret");
    fs::create_dir_all(&secret).unwrap();
    fs::write(secret.join("hidden.txt"), b"nope").unwrap();
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();

    let mut perms = fs::metadata(&secret).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret, perms).unwrap();

    let result = engine.scan_paths("Personal", "code", &[lp("secret")], ScanOptions::default());
    let _ = fs::set_permissions(&secret, fs::Permissions::from_mode(0o755));
    let report = result.unwrap();
    assert_eq!(report.deleted, 0, "{report:?}");
    assert!(report.protected >= 1, "{report:?}");
    assert!(entry_at(&engine, "secret/hidden.txt").is_some_and(|e| !e.is_deleted()));
}

#[cfg(unix)]
fn unreadable_dirs_still_readable() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
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
fn scan_paths_marker_missing_is_refused() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    fs::write(mount.path().join("a.txt"), b"x").unwrap();
    scan_paths(&mut engine, &["a.txt"]);
    fs::remove_file(mount.path().join(MOUNT_MARKER)).unwrap();
    let err = engine
        .scan_paths("Personal", "code", &[lp("a.txt")], ScanOptions::default())
        .unwrap_err();
    assert!(matches!(err, EngineError::Fs(_)), "{err}");
    assert!(!entry_at(&engine, "a.txt").unwrap().is_deleted());
}

#[test]
fn scan_paths_mass_delete_uses_mount_wide_live() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    for i in 0..10 {
        fs::write(mount.path().join(format!("keep{i}.txt")), b"k").unwrap();
    }
    for i in 0..30 {
        fs::write(mount.path().join(format!("gone{i}.txt")), b"g").unwrap();
    }
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    let live = engine.entries("Personal", "code", false).unwrap().len();
    assert_eq!(live, 40);

    for i in 0..30 {
        fs::remove_file(mount.path().join(format!("gone{i}.txt"))).unwrap();
    }
    let paths: Vec<LogicalPath> = (0..30).map(|i| lp(&format!("gone{i}.txt"))).collect();
    let err = engine
        .scan_paths("Personal", "code", &paths, ScanOptions::default())
        .unwrap_err();
    match err {
        EngineError::MassDeleteRefused { deletions, live } => {
            assert_eq!(deletions, 30);
            assert_eq!(live, 40);
        }
        other => panic!("{other}"),
    }
    assert_eq!(
        engine
            .entries("Personal", "code", false)
            .unwrap()
            .iter()
            .filter(|e| e.content.object().is_some())
            .count(),
        40
    );
}

#[test]
fn scan_paths_empty_observation_does_not_use_full_scan_empty_rule() {
    let home = new_home();
    let mount = new_home();
    let mut engine = ready(home.path(), mount.path());
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(mount.path().join(name), b"x").unwrap();
    }
    engine
        .scan("Personal", "code", ScanOptions::default())
        .unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::remove_file(mount.path().join(name)).unwrap();
    }
    let report = scan_paths(&mut engine, &["a.txt", "b.txt", "c.txt"]);
    assert_eq!(report.deleted, 3, "{report:?}");
}

fn disk_live(root: &Path) -> BTreeMap<String, DiskEnt> {
    let mut out = BTreeMap::new();
    walk_disk(root, root, &mut out);
    out
}

#[derive(Debug, PartialEq, Eq)]
enum DiskEnt {
    File { size: u64, id: ObjectId },
    Dir,
    Link { target: String },
}

fn walk_disk(root: &Path, dir: &Path, out: &mut BTreeMap<String, DiskEnt>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries {
        let entry = entry.unwrap();
        let os_path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == MOUNT_MARKER || name.starts_with(TEMP_PREFIX) {
            continue;
        }
        let Ok(logical) = relay_fs::to_logical_path(root, &os_path) else {
            continue;
        };
        let meta = match fs::symlink_metadata(&os_path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&os_path)
                .ok()
                .and_then(|p| p.to_str().map(ToOwned::to_owned))
                .unwrap_or_default();
            out.insert(logical.to_string(), DiskEnt::Link { target });
        } else if meta.is_dir() {
            out.insert(logical.to_string(), DiskEnt::Dir);
            walk_disk(root, &os_path, out);
        } else if meta.is_file() {
            let bytes = fs::read(&os_path).unwrap_or_default();
            out.insert(
                logical.to_string(),
                DiskEnt::File {
                    size: bytes.len() as u64,
                    id: ObjectId::of(&bytes),
                },
            );
        }
    }
}

fn assert_index_matches_disk(engine: &Engine, root: &Path) {
    let disk = disk_live(root);
    let live = engine.entries("Personal", "code", false).unwrap();
    let index: BTreeMap<String, DiskEnt> = live
        .iter()
        .map(|e| {
            let ent = match &e.content {
                EntryContent::File { object, size, .. } => DiskEnt::File {
                    size: *size,
                    id: *object,
                },
                EntryContent::Directory => DiskEnt::Dir,
                EntryContent::Symlink { target } => DiskEnt::Link {
                    target: target.clone(),
                },
                EntryContent::Deleted => panic!("live entry is deleted"),
            };
            (e.key.path.to_string(), ent)
        })
        .collect();
    assert_eq!(index, disk, "index vs disk");
    for entry in &live {
        if let Some(id) = entry.content.object() {
            let store = relay_engine::ObjectStore::open(engine.home().join("store")).unwrap();
            store.verify(&id).unwrap();
        }
    }
    let verify = engine.verify_objects().unwrap();
    assert!(
        verify.missing.is_empty() && verify.corrupt.is_empty(),
        "{verify:?}"
    );
}

#[derive(Clone, Debug)]
struct RawOp {
    kind: u8,
    pick: u8,
    name: u8,
    payload: Vec<u8>,
}

fn arb_op() -> impl Strategy<Value = RawOp> {
    (
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        proptest::collection::vec(any::<u8>(), 1..8),
    )
        .prop_map(|(kind, pick, name, payload)| RawOp {
            kind,
            pick,
            name,
            payload,
        })
}

struct Model {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
    next: u32,
}

impl Model {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            dirs: BTreeSet::new(),
            next: 0,
        }
    }

    fn dir_list(&self) -> Vec<String> {
        let mut dirs: Vec<String> = self.dirs.iter().cloned().collect();
        dirs.insert(0, String::new());
        dirs
    }

    fn file_list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    fn apply(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        match op.kind % 8 {
            0 => self.create_file(root, op),
            1 => self.overwrite(root, op, true),
            2 => self.overwrite(root, op, false),
            3 => self.delete_file(root, op),
            4 => self.mkdir(root, op),
            5 => self.delete_dir(root, op),
            6 => self.rename_file(root, op),
            _ => self.rename_dir(root, op),
        }
    }

    fn create_file(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        if self.files.len() >= 12 {
            return Vec::new();
        }
        let parent = self.pick_dir(op.pick);
        if depth(&parent) >= 3 {
            return Vec::new();
        }
        let name = format!("f{}", self.next);
        self.next += 1;
        let path = join_rel(&parent, &name);
        let dest = os(root, &path);
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&dest, &op.payload).unwrap();
        self.files.insert(path.clone(), op.payload.clone());
        if !parent.is_empty() {
            self.dirs.insert(parent);
        }
        vec![lp(&path)]
    }

    fn overwrite(&mut self, root: &Path, op: &RawOp, same_size: bool) -> Vec<LogicalPath> {
        let files = self.file_list();
        if files.is_empty() {
            return Vec::new();
        }
        let path = files[op.pick as usize % files.len()].clone();
        let old = self.files.get(&path).cloned().unwrap();
        let new = if same_size {
            if old.is_empty() {
                vec![op.payload.first().copied().unwrap_or(1)]
            } else {
                let mut bytes = old.clone();
                bytes[0] = bytes[0].wrapping_add(1);
                bytes
            }
        } else {
            let mut bytes = op.payload.clone();
            bytes.extend_from_slice(&old);
            bytes.push(0xff);
            bytes
        };
        fs::write(os(root, &path), &new).unwrap();
        self.files.insert(path.clone(), new);
        vec![lp(&path)]
    }

    fn delete_file(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        let files = self.file_list();
        if files.is_empty() {
            return Vec::new();
        }
        let path = files[op.pick as usize % files.len()].clone();
        let _ = fs::remove_file(os(root, &path));
        self.files.remove(&path);
        vec![lp(&path)]
    }

    fn mkdir(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        let parent = self.pick_dir(op.pick);
        if depth(&parent) >= 2 {
            return Vec::new();
        }
        let name = format!("d{}", self.next);
        self.next += 1;
        let path = join_rel(&parent, &name);
        fs::create_dir_all(os(root, &path)).unwrap();
        self.dirs.insert(path.clone());
        vec![lp(&path)]
    }

    fn delete_dir(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        let dirs: Vec<String> = self.dirs.iter().cloned().collect();
        if dirs.is_empty() {
            return Vec::new();
        }
        let path = dirs[op.pick as usize % dirs.len()].clone();
        let _ = fs::remove_dir_all(os(root, &path));
        self.files.retain(|p, _| !under(p, &path));
        self.dirs.retain(|p| p != &path && !under(p, &path));
        vec![lp(&path)]
    }

    fn rename_file(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        let files = self.file_list();
        if files.is_empty() {
            return Vec::new();
        }
        let old = files[op.pick as usize % files.len()].clone();
        let parent = parent_of(&old);
        let name = format!("r{}", self.next);
        self.next += 1;
        let new = join_rel(&parent, &name);
        fs::rename(os(root, &old), os(root, &new)).unwrap();
        let content = self.files.remove(&old).unwrap();
        self.files.insert(new.clone(), content);
        vec![lp(&old), lp(&new)]
    }

    fn rename_dir(&mut self, root: &Path, op: &RawOp) -> Vec<LogicalPath> {
        let dirs: Vec<String> = self.dirs.iter().cloned().collect();
        if dirs.is_empty() {
            return Vec::new();
        }
        let old = dirs[op.pick as usize % dirs.len()].clone();
        if self.files.keys().any(|p| under(p, &old)) && self.files.len() > 12 {
            return Vec::new();
        }
        let parent = parent_of(&old);
        let name = format!("m{}", self.next);
        self.next += 1;
        let new = join_rel(&parent, &name);
        if fs::rename(os(root, &old), os(root, &new)).is_err() {
            return Vec::new();
        }
        let files: Vec<(String, Vec<u8>)> = self
            .files
            .iter()
            .filter(|(p, _)| p == &&old || under(p, &old))
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect();
        for (p, c) in files {
            self.files.remove(&p);
            self.files.insert(reparent(&p, &old, &new), c);
        }
        let nested: Vec<String> = self
            .dirs
            .iter()
            .filter(|p| *p == &old || under(p, &old))
            .cloned()
            .collect();
        for p in nested {
            self.dirs.remove(&p);
            self.dirs.insert(reparent(&p, &old, &new));
        }
        vec![lp(&old), lp(&new)]
    }

    fn pick_dir(&self, pick: u8) -> String {
        let dirs = self.dir_list();
        dirs[pick as usize % dirs.len()].clone()
    }
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_of(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(p, _)| p.to_owned())
        .unwrap_or_default()
}

fn os(root: &Path, path: &str) -> std::path::PathBuf {
    let mut out = root.to_path_buf();
    for part in path.split('/') {
        out.push(part);
    }
    out
}

fn depth(path: &str) -> usize {
    if path.is_empty() {
        0
    } else {
        path.split('/').count()
    }
}

fn under(path: &str, prefix: &str) -> bool {
    path.len() > prefix.len() && path.starts_with(prefix) && path.as_bytes()[prefix.len()] == b'/'
}

fn reparent(path: &str, old: &str, new: &str) -> String {
    if path == old {
        new.to_owned()
    } else {
        format!("{new}{}", &path[old.len()..])
    }
}

#[cfg(unix)]
fn maybe_chmod(root: &Path, model: &mut Model, op: &RawOp) -> Vec<LogicalPath> {
    use std::os::unix::fs::PermissionsExt;
    let files = model.file_list();
    if files.is_empty() {
        return Vec::new();
    }
    let path = files[op.name as usize % files.len()].clone();
    let dest = os(root, &path);
    let mut perms = fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(&dest, perms).unwrap();
    vec![lp(&path)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn scan_paths_random_ops_converge(
        ops in proptest::collection::vec(arb_op(), 1..16),
        modes in proptest::collection::vec(any::<bool>(), 1..8),
        subset_bits in proptest::collection::vec(any::<u32>(), 1..8),
    ) {
        let home = TempDir::new().unwrap();
        let mount = TempDir::new().unwrap();
        let mut engine = ready(home.path(), mount.path());
        engine.scan("Personal", "code", ScanOptions::default()).unwrap();
        let mut model = Model::new();
        let mut batches: Vec<Vec<RawOp>> = Vec::new();
        for (i, op) in ops.into_iter().enumerate() {
            if i % 3 == 0 {
                batches.push(Vec::new());
            }
            batches.last_mut().unwrap().push(op);
        }
        for (i, batch) in batches.into_iter().enumerate() {
            let mut touched = Vec::new();
            for op in &batch {
                let mut paths = model.apply(mount.path(), op);
                #[cfg(unix)]
                if op.kind % 11 == 10 {
                    paths.extend(maybe_chmod(mount.path(), &mut model, op));
                }
                touched.extend(paths);
            }
            touched.sort();
            touched.dedup();
            if touched.is_empty() {
                continue;
            }
            let complete = modes[i % modes.len()];
            if complete {
                engine
                    .scan_paths("Personal", "code", &touched, ScanOptions::default())
                    .unwrap();
                let dry = engine
                    .scan(
                        "Personal",
                        "code",
                        ScanOptions {
                            dry_run: true,
                            ..ScanOptions::default()
                        },
                    )
                    .unwrap();
                prop_assert_eq!(dry.created, 0, "created after complete incremental: {:?}", dry);
                prop_assert_eq!(dry.modified, 0, "modified after complete incremental: {:?}", dry);
                prop_assert_eq!(dry.deleted, 0, "deleted after complete incremental: {:?}", dry);
            } else {
                let bits = subset_bits[i % subset_bits.len()];
                let subset: Vec<LogicalPath> = touched
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| (bits >> (j % 32)) & 1 == 1)
                    .map(|(_, p)| p.clone())
                    .collect();
                engine
                    .scan_paths("Personal", "code", &subset, ScanOptions::default())
                    .unwrap();
                engine
                    .scan(
                        "Personal",
                        "code",
                        ScanOptions {
                            allow_mass_delete: true,
                            ..ScanOptions::default()
                        },
                    )
                    .unwrap();
            }
        }
        engine
            .scan(
                "Personal",
                "code",
                ScanOptions {
                    allow_mass_delete: true,
                    ..ScanOptions::default()
                },
            )
            .unwrap();
        assert_index_matches_disk(&engine, mount.path());
    }
}
