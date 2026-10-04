//! Files view: folder listings, per-folder modes, and freeing space.

use std::fs;
use std::path::Path;

use relay_engine::{
    ConfigChange, CopyState, Engine, EntryKind, FolderView, MaterializationMode, ScanOptions,
};
use tempfile::TempDir;

fn ready(home: &Path, folder: &Path) -> Engine {
    for (path, bytes) in [("a.txt", "a"), ("dir/b.txt", "b"), ("dir/sub/c.txt", "c")] {
        let path = folder.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let mut engine = Engine::init(home, "dev").unwrap();
    engine.create_space("S").unwrap();
    engine.add_mount("S", "m", folder, &[], &[]).unwrap();
    engine.scan("S", "m", ScanOptions::default()).unwrap();
    engine
}

fn list(engine: &Engine, path: &str) -> FolderView {
    engine.list_folder("S", "m", path).unwrap()
}

fn names(view: &FolderView) -> Vec<&str> {
    view.entries.iter().map(|e| e.name.as_str()).collect()
}

fn set_mode(engine: &mut Engine, path: &str, mode: Option<&str>) {
    engine
        .apply_config(&ConfigChange::SetFolderMode {
            space: "S".into(),
            mount: "m".into(),
            path: path.into(),
            mode: mode.map(str::to_owned),
        })
        .unwrap();
}

#[test]
fn lists_one_level_folders_first() {
    let (home, folder) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let engine = ready(home.path(), folder.path());

    let root = list(&engine, "");
    assert_eq!(names(&root), ["dir", "a.txt"]);
    assert_eq!(root.entries[0].kind, EntryKind::Directory);
    assert_eq!(root.entries[1].size, Some(1));
    assert!(root.entries.iter().all(|e| e.state == CopyState::Local));
    assert_eq!(root.mode, MaterializationMode::Full);

    let dir = list(&engine, "dir");
    assert_eq!(names(&dir), ["sub", "b.txt"]);
    assert_eq!(dir.entries[1].path, "dir/b.txt");
}

#[test]
fn a_parent_choice_replaces_choices_inside_it() {
    let (home, folder) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut engine = ready(home.path(), folder.path());
    engine
        .materialize_add("S", "media", "metadata", &["m/media/**".into()])
        .unwrap();

    set_mode(&mut engine, "dir", Some("demand"));
    assert_eq!(list(&engine, "").mode, MaterializationMode::Full);
    assert_eq!(list(&engine, "dir").mode, MaterializationMode::Demand);
    assert_eq!(
        list(&engine, "dir").chosen_here,
        Some(MaterializationMode::Demand)
    );

    set_mode(&mut engine, "dir/sub", Some("full"));
    assert_eq!(list(&engine, "dir/sub").mode, MaterializationMode::Full);
    assert_eq!(list(&engine, "dir").mode, MaterializationMode::Demand);

    // Choosing for `dir` again covers `dir/sub` too.
    set_mode(&mut engine, "dir", Some("demand"));
    assert_eq!(list(&engine, "dir/sub").mode, MaterializationMode::Demand);
    assert_eq!(list(&engine, "dir/sub").chosen_here, None);

    set_mode(&mut engine, "dir", Some("exclude"));
    let root = list(&engine, "");
    let dir = root.entries.iter().find(|e| e.name == "dir").unwrap();
    assert_eq!(
        dir.mode,
        MaterializationMode::Exclude,
        "the folder itself too"
    );

    set_mode(&mut engine, "dir", None);
    assert_eq!(list(&engine, "dir").mode, MaterializationMode::Full);
    let rules = engine.materialization_rules(Some("S")).unwrap();
    assert_eq!(rules.len(), 1, "only the hand-written rule is left");
    assert_eq!(rules[0].name, "media");
}

#[test]
fn freeing_a_folder_keeps_changed_files_and_the_index() {
    let (home, folder) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut engine = ready(home.path(), folder.path());
    set_mode(&mut engine, "dir", Some("demand"));
    fs::write(folder.path().join("dir/sub/c.txt"), "edited").unwrap();

    assert_eq!(
        engine.evict("S", "m", "dir").unwrap(),
        1,
        "the edited file stays"
    );
    assert!(!folder.path().join("dir/b.txt").exists());
    assert!(folder.path().join("dir/sub/c.txt").exists());
    assert!(folder.path().join("a.txt").exists(), "outside the folder");
    let dir = list(&engine, "dir");
    let b = dir.entries.iter().find(|e| e.name == "b.txt").unwrap();
    assert_eq!(b.state, CopyState::OnlineOnly);

    let path = engine.local_file_path("S", "m", "a.txt").unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "a");
}
