//! Pushing one file to a connected device (D50).
//!
//! Like a connection test (D48), a drop rides its own bidirectional stream.
//! The sender writes an `ObjectRequest` with `drop_offer` set and the
//! receiver answers with an `ObjectHeader`. After acceptance the sender
//! writes exactly `size` bytes and finishes. The receiver writes them to a
//! hidden partial file in its inbox, checks the size and BLAKE3 hash, gives
//! the file its final name without replacing anything, and ends with a
//! `DropDone`. The sender reports success only after that.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quinn::{RecvStream, SendStream};
use relay_core::DeviceId;
use relay_core::drops::{DROP_MAX_BYTES, DropSent, clean_drop_name, mime_for, numbered_name};
use relay_core::remote::{RemoteError, RemoteErrorCode};
use relay_proto::{
    DropDone, DropOffer, FEATURE_DROP, ObjectHeader, ObjectRequest, encode_frame, error_from_wire,
    error_to_wire,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::control::{COPY_IDLE_TIMEOUT, on_handler};
use crate::io::read_message_max;
use crate::session::{Inner, OBJECT_HEADER_MAX};

/// Drops this device takes at once, from all peers together.
pub(crate) const MAX_DROPS_IN: usize = 4;
/// How long the sender waits for the receiver to accept.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the receiver may take to verify and save after the last byte.
const DONE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the receiver's handler may take to name the inbox.
const INBOX_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK: usize = 64 * 1024;
/// Numbered names tried before a drop is refused.
const MAX_NUMBER: u32 = 10_000;

/// Push the file at `path` to `peer`, offered as `name` (its own file name
/// when `None`).
pub(crate) async fn send(
    inner: Arc<Inner>,
    peer: DeviceId,
    path: PathBuf,
    name: Option<String>,
) -> Result<DropSent, RemoteError> {
    let Some(conn) = inner.control_connection(peer) else {
        return Err(RemoteError::new(
            RemoteErrorCode::Offline,
            "that device is not connected",
        ));
    };
    if !inner.peer_has_feature(peer, FEATURE_DROP) {
        return Err(RemoteError::new(
            RemoteErrorCode::Unsupported,
            "that device runs a Relay without drops; update it",
        ));
    }
    let name = match name {
        Some(name) => name,
        None => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| RemoteError::new(RemoteErrorCode::Invalid, "that is not a file"))?,
    };
    let hashed = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || hash_file(&path))
            .await
            .map_err(failed)??
    };
    let offer = DropOffer {
        mime: mime_for(&name).to_owned(),
        name,
        size: hashed.size,
        blake3: hashed.hash.as_bytes().to_vec(),
        taken_ms: hashed.modified_ms,
    };

    let start = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.map_err(failed)?;
    let request = encode_frame(&ObjectRequest {
        drop_offer: Some(offer),
        ..Default::default()
    })
    .map_err(failed)?;
    send.write_all(&request).await.map_err(failed)?;
    let header: ObjectHeader = tokio::time::timeout(
        ACCEPT_TIMEOUT,
        read_message_max(&mut recv, OBJECT_HEADER_MAX),
    )
    .await
    .map_err(|_| {
        RemoteError::new(
            RemoteErrorCode::Timeout,
            "that device did not answer in time",
        )
    })?
    .map_err(failed)?;
    if let Some(err) = header.error {
        return Err(error_from_wire(err));
    }
    if !header.found {
        return Err(RemoteError::new(
            RemoteErrorCode::Failed,
            "that device refused the file",
        ));
    }

    let file = tokio::fs::File::open(&path).await.map_err(failed)?;
    let mut body = file.take(hashed.size);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = body.read(&mut buf).await.map_err(failed)?;
        if n == 0 {
            break;
        }
        tokio::time::timeout(COPY_IDLE_TIMEOUT, send.write_all(&buf[..n]))
            .await
            .map_err(|_| RemoteError::new(RemoteErrorCode::Timeout, "the send stalled"))?
            .map_err(failed)?;
    }
    send.finish().map_err(failed)?;

    let done: DropDone =
        tokio::time::timeout(DONE_TIMEOUT, read_message_max(&mut recv, OBJECT_HEADER_MAX))
            .await
            .map_err(|_| {
                RemoteError::new(
                    RemoteErrorCode::Timeout,
                    "that device did not confirm the file",
                )
            })?
            .map_err(failed)?;
    if let Some(err) = done.error {
        return Err(error_from_wire(err));
    }
    Ok(DropSent {
        saved_name: done.saved_name,
        size: hashed.size,
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

struct Hashed {
    size: u64,
    hash: blake3::Hash,
    modified_ms: Option<i64>,
}

/// Size, hash and modified time of the file to send. The size is read once,
/// and exactly that many bytes are hashed and later sent; a file that
/// changes in between fails the receiver's check.
fn hash_file(path: &Path) -> Result<Hashed, RemoteError> {
    // Windows cannot open a folder as a file, so check before opening.
    if !std::fs::metadata(path).map_err(failed)?.is_file() {
        return Err(RemoteError::new(
            RemoteErrorCode::Invalid,
            "only a single file can be sent",
        ));
    }
    let file = std::fs::File::open(path).map_err(failed)?;
    let meta = file.metadata().map_err(failed)?;
    let size = meta.len();
    if size > DROP_MAX_BYTES {
        return Err(RemoteError::new(
            RemoteErrorCode::Invalid,
            format!(
                "that file is larger than the {} MiB a drop carries",
                DROP_MAX_BYTES / (1024 * 1024)
            ),
        ));
    }
    let mut hasher = blake3::Hasher::new();
    let mut body = file.take(size);
    let mut buf = vec![0u8; CHUNK];
    let mut have = 0u64;
    loop {
        let n = body.read(&mut buf).map_err(failed)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        have += n as u64;
    }
    if have != size {
        return Err(RemoteError::new(
            RemoteErrorCode::Failed,
            "the file changed while it was read; try again",
        ));
    }
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok());
    Ok(Hashed {
        size,
        hash: hasher.finalize(),
        modified_ms,
    })
}

/// Answer one drop offer from a trusted peer.
pub(crate) async fn serve(
    inner: &Inner,
    peer: DeviceId,
    mut send: SendStream,
    mut recv: RecvStream,
    offer: DropOffer,
) -> Result<(), String> {
    let (inbox, name, _permit) = match accept(inner, peer, &offer).await {
        Ok(accepted) => accepted,
        Err(err) => {
            tracing::debug!(peer = %peer, code = err.code.as_str(), error = %err, "drop refused");
            let header = ObjectHeader {
                error: Some(error_to_wire(&err)),
                ..Default::default()
            };
            let bytes = encode_frame(&header).map_err(|e| e.to_string())?;
            send.write_all(&bytes).await.map_err(|e| e.to_string())?;
            let _ = send.finish();
            return Ok(());
        }
    };
    let accept = encode_frame(&ObjectHeader {
        found: true,
        size: offer.size,
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;
    send.write_all(&accept).await.map_err(|e| e.to_string())?;

    let done = match receive(&mut recv, &inbox, &name, &offer).await {
        Ok((path, saved_name)) => {
            tracing::debug!(peer = %peer, path = %path.display(), "drop saved");
            if let Some(handler) = inner.control.clone() {
                let size = offer.size;
                let _ =
                    tokio::task::spawn_blocking(move || handler.dropped(peer, &path, size)).await;
            }
            DropDone {
                saved_name,
                error: None,
            }
        }
        Err(err) => {
            tracing::debug!(peer = %peer, error = %err, "drop failed");
            let _ = recv.stop(0u32.into());
            DropDone {
                saved_name: String::new(),
                error: Some(error_to_wire(&err)),
            }
        }
    };
    let bytes = encode_frame(&done).map_err(|e| e.to_string())?;
    send.write_all(&bytes).await.map_err(|e| e.to_string())?;
    let _ = send.finish();
    Ok(())
}

/// Check the offer and ask the handler where it goes. Returns the inbox,
/// the cleaned name, and a slot among [`MAX_DROPS_IN`].
async fn accept(
    inner: &Inner,
    peer: DeviceId,
    offer: &DropOffer,
) -> Result<(PathBuf, String, tokio::sync::OwnedSemaphorePermit), RemoteError> {
    let Some(handler) = inner.control.clone() else {
        return Err(RemoteError::new(
            RemoteErrorCode::Unsupported,
            "this device does not take drops",
        ));
    };
    if offer.size > DROP_MAX_BYTES {
        return Err(RemoteError::new(
            RemoteErrorCode::Invalid,
            format!(
                "files over {} MiB cannot be dropped",
                DROP_MAX_BYTES / (1024 * 1024)
            ),
        ));
    }
    if offer.blake3.len() != blake3::OUT_LEN {
        return Err(RemoteError::new(
            RemoteErrorCode::Invalid,
            "the offer has no valid hash",
        ));
    }
    let name = clean_drop_name(&offer.name).ok_or_else(|| {
        RemoteError::new(
            RemoteErrorCode::Invalid,
            "that file name cannot be saved here",
        )
    })?;
    let permit = inner.drops_in.clone().try_acquire_owned().map_err(|_| {
        RemoteError::new(
            RemoteErrorCode::Busy,
            "this device is taking other files; try again in a moment",
        )
    })?;
    let size = offer.size;
    let inbox = on_handler(INBOX_TIMEOUT, move || handler.drop_inbox(peer, size)).await?;
    Ok((inbox, name, permit))
}

/// Read the bytes into a partial file in `inbox`, verify them, and give the
/// file its final name. Returns the saved path and name.
async fn receive(
    recv: &mut RecvStream,
    inbox: &Path,
    name: &str,
    offer: &DropOffer,
) -> Result<(PathBuf, String), RemoteError> {
    let partial = inbox.join(format!(
        ".relay-drop-{:016x}.partial",
        rand::random::<u64>()
    ));
    let result = async {
        write_partial(recv, &partial, offer).await?;
        let (partial, inbox, name) = (partial.clone(), inbox.to_path_buf(), name.to_owned());
        tokio::task::spawn_blocking(move || place(&partial, &inbox, &name))
            .await
            .map_err(failed)?
            .map_err(failed)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    result
}

async fn write_partial(
    recv: &mut RecvStream,
    partial: &Path,
    offer: &DropOffer,
) -> Result<(), RemoteError> {
    let mut file = tokio::fs::File::create_new(partial).await.map_err(failed)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut have = 0u64;
    loop {
        let read = tokio::time::timeout(COPY_IDLE_TIMEOUT, recv.read(&mut buf))
            .await
            .map_err(|_| RemoteError::new(RemoteErrorCode::Timeout, "the file stopped arriving"))?
            .map_err(failed)?;
        let Some(n) = read else {
            break;
        };
        have += n as u64;
        if have > offer.size {
            return Err(RemoteError::new(
                RemoteErrorCode::Invalid,
                "more bytes arrived than were offered",
            ));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await.map_err(failed)?;
    }
    if have != offer.size || hasher.finalize().as_bytes()[..] != offer.blake3[..] {
        return Err(RemoteError::new(
            RemoteErrorCode::Failed,
            "the file arrived damaged or changed while it was sent; try again",
        ));
    }
    file.sync_all().await.map_err(failed)?;
    let file = file.into_std().await;
    if let Some(taken) = offer
        .taken_ms
        .and_then(|ms| u64::try_from(ms).ok())
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
    {
        let _ = file.set_modified(taken);
    }
    Ok(())
}

/// Move `partial` to `name` in `inbox`, numbering the name until it is
/// free. A hard link fails when the name exists, so nothing is replaced;
/// where links are not supported (FAT, some network shares) a rename
/// follows a check, which a file created in between could race.
fn place(partial: &Path, inbox: &Path, name: &str) -> io::Result<(PathBuf, String)> {
    for n in 1..=MAX_NUMBER {
        let candidate = if n == 1 {
            name.to_owned()
        } else {
            numbered_name(name, n)
        };
        let dest = inbox.join(&candidate);
        match std::fs::hard_link(partial, &dest) {
            Ok(()) => {
                let _ = std::fs::remove_file(partial);
                return Ok((dest, candidate));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                if dest.symlink_metadata().is_ok() {
                    continue;
                }
                std::fs::rename(partial, &dest)?;
                return Ok((dest, candidate));
            }
        }
    }
    Err(io::Error::other(
        "the inbox already has too many files with that name",
    ))
}

fn failed(err: impl ToString) -> RemoteError {
    RemoteError::new(RemoteErrorCode::Failed, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placing_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"old").unwrap();
        std::fs::write(dir.path().join("a (2).jpg"), b"old 2").unwrap();
        let partial = dir.path().join(".p");
        std::fs::write(&partial, b"new").unwrap();
        let (path, name) = place(&partial, dir.path(), "a.jpg").unwrap();
        assert_eq!(name, "a (3).jpg");
        assert_eq!(std::fs::read(path).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.path().join("a.jpg")).unwrap(), b"old");
        assert!(!partial.exists());
    }
}
