//! Sync roots for online-only files (D43).
//!
//! The engine decides which mounts want placeholders; this registers and
//! connects them, and answers the system's callbacks. Opening a placeholder
//! asks the sync loop to get the object into the local store, then streams
//! it from there. Nothing here waits on the loop while the loop could be
//! waiting on this process's own reads: those are refused by the connection.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use relay_core::{MountId, ObjectId, SpaceId};
use relay_engine::{ObjectStore, PlaceholderHost, PlaceholderRoot, SyncInput};
use relay_fs::cloud::{self, Hydration, Provider, RootSpec};

use crate::host::Host;

/// Bytes per write into a placeholder. A multiple of 4 KiB, as required.
const CHUNK: usize = 1 << 20;
/// How long an open may wait for another device to send the bytes.
const FETCH_CAP: Duration = Duration::from_secs(30 * 60);
/// The system cancels a request that reports nothing for 60 seconds.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// The roots this host has connected. Lives as long as the host, so a
/// reload of the engine does not disconnect them.
pub(crate) struct Roots {
    host: Weak<Host>,
    enabled: bool,
    store_root: Mutex<Option<PathBuf>>,
    live: Mutex<HashMap<MountId, Live>>,
}

struct Live {
    path: PathBuf,
    _connection: cloud::Connection,
}

impl Roots {
    /// `enabled` false still unregisters roots left by an earlier run.
    pub fn new(host: &Arc<Host>, enabled: bool) -> Arc<Self> {
        let enabled = enabled
            && std::env::var("RELAY_PLACEHOLDERS").map_or(true, |value| value != "0")
            && cloud::supported();
        Arc::new(Self {
            host: Arc::downgrade(host),
            enabled,
            store_root: Mutex::new(None),
            live: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_store_root(&self, root: &Path) {
        if let Ok(mut slot) = self.store_root.lock() {
            *slot = Some(root.to_path_buf());
        }
    }
}

impl PlaceholderHost for Roots {
    fn registered(&self) -> Vec<(MountId, PathBuf)> {
        cloud::registered()
            .into_iter()
            .filter_map(|root| Some((root.account.parse().ok()?, root.path)))
            .collect()
    }

    fn attach(&self, root: &PlaceholderRoot) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(store_root) = self.store_root.lock().ok().and_then(|g| g.clone()) else {
            return false;
        };
        let Ok(mut live) = self.live.lock() else {
            return false;
        };
        if live.get(&root.mount).is_some_and(|l| l.path == root.path) {
            return true;
        }
        live.remove(&root.mount);
        let account = root.mount.to_string();
        let display_name = format!("Relay · {}/{}", root.space_name, root.mount_name);
        let spec = RootSpec {
            path: &root.path,
            account: &account,
            display_name: &display_name,
        };
        if let Err(err) = cloud::register(&spec) {
            tracing::warn!(path = %root.path.display(), error = %err, "online-only files stay in Relay: could not register the folder");
            return false;
        }
        let provider = Arc::new(MountProvider {
            host: self.host.clone(),
            space: root.space,
            space_name: root.space_name.clone(),
            mount_name: root.mount_name.clone(),
            root: root.path.clone(),
            store_root,
        });
        match cloud::connect(&root.path, provider) {
            Ok(connection) => {
                tracing::info!(path = %root.path.display(), "online-only files show in the file manager");
                live.insert(
                    root.mount,
                    Live {
                        path: root.path.clone(),
                        _connection: connection,
                    },
                );
                true
            }
            Err(err) => {
                tracing::warn!(path = %root.path.display(), error = %err, "online-only files stay in Relay: could not connect the folder");
                let _ = cloud::unregister(&account);
                false
            }
        }
    }

    fn detach(&self, mount: MountId, path: &Path) {
        if let Ok(mut live) = self.live.lock() {
            live.remove(&mount);
        }
        if let Err(err) = cloud::unregister(&mount.to_string()) {
            tracing::warn!(path = %path.display(), error = %err, "could not unregister an online-only folder");
        }
    }
}

/// Answers callbacks for one mount's root.
struct MountProvider {
    host: Weak<Host>,
    space: SpaceId,
    space_name: String,
    mount_name: String,
    root: PathBuf,
    store_root: PathBuf,
}

impl MountProvider {
    /// Ask the loop for `object`, keeping the system's request alive.
    fn fetch_object(&self, object: ObjectId, out: &mut dyn Hydration) -> Result<(), String> {
        let host = self.host.upgrade().ok_or("Relay is stopping")?;
        let (reply_tx, reply_rx) = mpsc::channel();
        let input = SyncInput::FetchObject {
            space: self.space,
            object,
            reply: reply_tx,
        };
        if !host.send_to_loop(input) {
            return Err("Relay is not syncing right now".into());
        }
        drop(host);
        let started = Instant::now();
        loop {
            match reply_rx.recv_timeout(HEARTBEAT) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(rejected)) => return Err(rejected.message),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("the sync loop stopped".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() >= FETCH_CAP => {
                    return Err("no device sent the file in time".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => out.progress(0),
            }
        }
    }

    fn touched(&self, paths: Vec<PathBuf>) {
        if let Some(host) = self.host.upgrade() {
            host.send_to_loop(SyncInput::Touched {
                root: self.root.clone(),
                paths,
            });
        }
    }
}

impl Provider for MountProvider {
    fn fetch(
        &self,
        path: &Path,
        object: Option<ObjectId>,
        out: &mut dyn Hydration,
    ) -> Result<(), String> {
        let object = object.ok_or("not one of Relay's placeholders")?;
        let store = ObjectStore::open(&self.store_root).map_err(|err| err.to_string())?;
        if !store.contains(&object) {
            self.fetch_object(object, out)?;
        }
        let len = out.len();
        let size = store.size_of(&object).map_err(|err| err.to_string())?;
        if size != len {
            return Err(format!(
                "stored object is {size} bytes, placeholder is {len}"
            ));
        }
        let mut file = store.open_object(&object).map_err(|err| err.to_string())?;
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        while offset < len {
            let want = usize::try_from(len - offset).map_or(CHUNK, |left| left.min(CHUNK));
            file.read_exact(&mut buf[..want])
                .map_err(|err| err.to_string())?;
            if let Err(err) = out.write(offset, &buf[..want]) {
                // Usually a cancelled open. Reporting it back can abort the
                // process inside cloud-filter, so let the request lapse.
                tracing::debug!(path = %path.display(), error = %err, "placeholder download stopped");
                return Ok(());
            }
            offset += want as u64;
            out.progress(offset);
        }
        // Mark the row downloaded; the file already holds its bytes.
        if let (Some(host), Ok(logical)) = (
            self.host.upgrade(),
            relay_fs::to_logical_path(&self.root, path),
        ) {
            let (reply, _) = mpsc::channel();
            host.send_to_loop(SyncInput::Fetch {
                space: self.space_name.clone(),
                mount: self.mount_name.clone(),
                path: logical.as_str().to_owned(),
                reply,
            });
        }
        Ok(())
    }

    fn dehydrated(&self, path: &Path) {
        self.touched(vec![path.to_path_buf()]);
    }

    fn moved(&self, path: &Path, to: Option<&Path>) {
        let mut paths = vec![path.to_path_buf()];
        paths.extend(to.map(Path::to_path_buf));
        self.touched(paths);
    }
}
