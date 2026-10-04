//! Read-only copies of files on a managed device (D41).
//!
//! A quick look that sets nothing up: the file is copied once into
//! `<home>/read-only/<n>/<name>`, marked read-only, and opened. Edits stay
//! here. The folder is emptied each time the host starts.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use relay_core::remote::{READ_COPY_MAX, RemoteError};
use relay_engine::PeerInfo;
use relay_ipc::RpcErrorBody;
use relay_net::NetCommand;

use crate::host::Host;

const DIR: &str = "read-only";

/// Copy `path` from `peer` and return where the copy is.
pub(crate) fn copy(host: &Host, peer: &PeerInfo, path: &str) -> Result<PathBuf, RpcErrorBody> {
    let name = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| RpcErrorBody::new("invalid", format!("{path} does not name a file")))?;
    let folder = new_slot(&host.home.join(DIR)).map_err(|err| failed(&err))?;
    let dest = folder.join(name);
    let net = host.net_sender()?;
    let (reply, rx) = mpsc::channel();
    net.send(NetCommand::ReadFile {
        peer: peer.id,
        path: path.to_owned(),
        max_bytes: READ_COPY_MAX,
        dest: dest.clone(),
        reply,
    });
    // The network gives up on a stalled copy itself, so no deadline here.
    let copied = rx
        .recv()
        .map_err(|_| RpcErrorBody::new("unavailable", "the network stopped"))
        .and_then(|result| result.map_err(rpc));
    if let Err(err) = copied {
        let _ = fs::remove_dir_all(&folder);
        return Err(err);
    }
    let mut perms = fs::metadata(&dest)
        .map_err(|err| failed(&err))?
        .permissions();
    perms.set_readonly(true);
    fs::set_permissions(&dest, perms).map_err(|err| failed(&err))?;
    Ok(dest)
}

/// Remove earlier copies. Called when the host starts.
pub(crate) fn clear(home: &Path) {
    let dir = home.join(DIR);
    if !dir.exists() {
        return;
    }
    // Windows refuses to delete read-only files.
    make_writable(&dir);
    if let Err(err) = fs::remove_dir_all(&dir) {
        tracing::warn!(path = %dir.display(), error = %err, "could not clear read-only copies");
    }
}

fn make_writable(path: &Path) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.is_file()
    {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
}

/// A fresh numbered folder under `root`, so copies of same-named files from
/// different places never collide and each keeps its own name.
fn new_slot(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    for n in 1u32.. {
        let slot = root.join(n.to_string());
        match fs::create_dir(&slot) {
            Ok(()) => return Ok(slot),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    unreachable!("u32 slots run out")
}

fn failed(err: &io::Error) -> RpcErrorBody {
    RpcErrorBody::new("failed", err.to_string())
}

fn rpc(err: RemoteError) -> RpcErrorBody {
    RpcErrorBody::new(err.code.as_str(), err.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_numbered_and_clear_removes_read_only_files() {
        let home = tempfile::TempDir::new().unwrap();
        let root = home.path().join(DIR);
        let one = new_slot(&root).unwrap();
        let two = new_slot(&root).unwrap();
        assert_ne!(one, two);
        let file = one.join("report.docx");
        fs::write(&file, b"x").unwrap();
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file, perms).unwrap();
        clear(home.path());
        assert!(!root.exists());
    }
}
