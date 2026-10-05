//! Cloud Files placeholders against the real API (D43). Windows only; skips
//! itself where the system has no Cloud Files support.
#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use relay_core::ObjectId;
use relay_fs::cloud::{self, Hydration, Probe, Provider, RootSpec};

#[derive(Default)]
struct MemoryProvider {
    objects: HashMap<ObjectId, Vec<u8>>,
    dehydrated: Mutex<Vec<PathBuf>>,
}

impl Provider for MemoryProvider {
    fn fetch(
        &self,
        _path: &Path,
        object: Option<ObjectId>,
        out: &mut dyn Hydration,
    ) -> Result<(), String> {
        let bytes = object
            .and_then(|id| self.objects.get(&id))
            .ok_or("unknown object")?;
        out.write(0, bytes).map_err(|err| err.to_string())?;
        out.progress(bytes.len() as u64);
        Ok(())
    }

    fn dehydrated(&self, path: &Path) {
        self.dehydrated.lock().unwrap().push(path.to_path_buf());
    }

    fn moved(&self, _path: &Path, _to: Option<&Path>) {}
}

/// Read a file from another process: this process's own reads of a
/// placeholder never reach the provider.
fn read_elsewhere(path: &Path) -> Vec<u8> {
    let out = Command::new("cmd")
        .arg("/C")
        .arg("type")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn placeholder_state(path: &Path) -> (Option<ObjectId>, bool, bool) {
    match cloud::probe(path).unwrap() {
        Probe::Placeholder {
            object,
            dehydrated,
            in_sync,
            ..
        } => (object, dehydrated, in_sync),
        other => panic!("not a placeholder: {other:?}"),
    }
}

#[test]
fn placeholders_download_on_open_and_free_up_space() {
    if !cloud::supported() {
        eprintln!("Cloud Files are not supported here; skipping");
        return;
    }
    // Sync roots need NTFS; CI's temp directory is on a ReFS dev drive.
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(base).unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let account = uuid::Uuid::new_v4().to_string();
    cloud::register(&RootSpec {
        path: &root,
        account: &account,
        display_name: "Relay test",
    })
    .unwrap();
    assert!(
        cloud::registered()
            .iter()
            .any(|r| r.account == account && r.path == root)
    );

    let content = b"hello from another device".to_vec();
    let object = ObjectId::of(&content);
    let mut provider = MemoryProvider::default();
    provider.objects.insert(object, content.clone());
    let provider = Arc::new(provider);
    let connection = cloud::connect(&root, provider.clone()).unwrap();

    // An online-only file: visible with its size, no bytes on disk.
    let online = root.join("online.txt");
    cloud::create(&online, object, content.len() as u64, 1_700_000_000_000).unwrap();
    assert_eq!(
        std::fs::metadata(&online).unwrap().len(),
        content.len() as u64
    );
    assert!(cloud::is_dehydrated_path(&online));
    assert_eq!(placeholder_state(&online), (Some(object), true, true));
    // This process cannot hydrate it by reading.
    assert!(std::fs::read(&online).is_err());

    // Another process opens it: the provider supplies the bytes.
    assert_eq!(read_elsewhere(&online), content);
    assert_eq!(placeholder_state(&online), (Some(object), false, true));
    assert_eq!(std::fs::read(&online).unwrap(), content);

    // Free up space: the file stays, its bytes go.
    cloud::dehydrate(&online).unwrap();
    assert!(cloud::is_dehydrated_path(&online));
    assert_eq!(read_elsewhere(&online), content);

    // A downloaded plain file becomes a placeholder in sync.
    let plain = root.join("plain.txt");
    std::fs::write(&plain, &content).unwrap();
    cloud::convert(&plain, object).unwrap();
    assert_eq!(placeholder_state(&plain), (Some(object), false, true));

    drop(connection);
    cloud::dehydrate(&online).ok();
    cloud::remove_dehydrated(&root);
    cloud::unregister(&account).unwrap();
    assert!(!cloud::registered().iter().any(|r| r.account == account));
}
