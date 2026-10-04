//! Remote calls between devices (D37).
//!
//! Each call rides its own bidirectional stream (see `relay_proto::control`).
//! The answering side checks the manage grant before anything else sees the
//! request, then runs the daemon's [`ControlHandler`] on a blocking thread
//! with a timeout: a filesystem call can stall, for example on a macOS
//! privacy prompt nobody is there to answer.
//!
//! A read-only copy (D41) is admitted the same way, then streams like an
//! object: an `ObjectHeader`, then the file's bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use quinn::{Connection, RecvStream, SendStream};
use relay_core::DeviceId;
use relay_core::remote::{CopiedFile, RemoteCall, RemoteError, RemoteErrorCode, RemoteResult};
use relay_proto::{
    ControlRequest, ControlResponse, FEATURE_CONTROL, Frame, ObjectHeader, ObjectRequest,
    PeerGrants, ReadFileRequest, call_from_wire, call_to_wire, encode_frame, error_from_wire,
    error_to_wire, frame::Body, result_from_wire, result_to_wire,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;

use crate::io::read_message;
use crate::session::Inner;

/// How long one call may run on the answering device.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a caller waits for an answer, including the network.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Calls one peer may have running here at once.
pub(crate) const MAX_CONCURRENT_CALLS: usize = 4;
/// A copy whose bytes stop arriving for this long has failed.
const COPY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const COPY_CHUNK: usize = 64 * 1024;

/// Answers remote calls from peers that hold the manage grant. The daemon
/// implements this; the network layer only checks the grant and transports.
pub trait ControlHandler: Send + Sync {
    /// Runs on a blocking thread. `peer` already holds the grant here, but an
    /// implementation should check its own records too.
    fn handle(&self, peer: DeviceId, call: RemoteCall) -> RemoteResult;

    /// Open one file for a read-only copy (D41), refusing anything over
    /// `max_bytes`. Runs on a blocking thread, like [`Self::handle`].
    fn open_file(
        &self,
        peer: DeviceId,
        path: &str,
        max_bytes: u64,
    ) -> Result<std::fs::File, RemoteError> {
        let _ = (peer, path, max_bytes);
        Err(RemoteError::new(
            RemoteErrorCode::Unsupported,
            "this device does not hand out file copies",
        ))
    }
}

/// The connection to `peer`, if it is up and answers remote calls.
fn connection(inner: &Inner, peer: DeviceId) -> Result<Connection, RemoteError> {
    let Some(conn) = inner.control_connection(peer) else {
        return Err(RemoteError::new(
            RemoteErrorCode::Offline,
            "that device is not connected",
        ));
    };
    if !inner.peer_has_feature(peer, FEATURE_CONTROL) {
        return Err(RemoteError::new(
            RemoteErrorCode::Unsupported,
            "that device runs a Relay without remote management; update it",
        ));
    }
    Ok(conn)
}

/// Make `call` on `peer` and wait for its answer.
pub(crate) async fn call(inner: Arc<Inner>, peer: DeviceId, call: RemoteCall) -> RemoteResult {
    let conn = connection(&inner, peer)?;
    let exchange = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(failed)?;
        let request = encode_frame(&ObjectRequest {
            control: Some(call_to_wire(&call)),
            ..Default::default()
        })
        .map_err(failed)?;
        send.write_all(&request).await.map_err(failed)?;
        send.finish().map_err(failed)?;
        let response: ControlResponse = read_message(&mut recv).await.map_err(failed)?;
        result_from_wire(response).map_err(failed)?
    };
    match tokio::time::timeout(CALL_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(RemoteError::new(
            RemoteErrorCode::Timeout,
            "that device did not answer in time",
        )),
    }
}

/// Answer one call that arrived on an object stream.
pub(crate) async fn serve(
    inner: &Inner,
    peer: DeviceId,
    mut send: SendStream,
    _recv: RecvStream,
    request: ControlRequest,
) -> Result<(), String> {
    let result = answer(inner, peer, request).await;
    if let Err(err) = &result {
        tracing::debug!(peer = %peer, code = err.code.as_str(), error = %err, "remote call refused");
    }
    let bytes = encode_frame(&result_to_wire(&result)).map_err(|e| e.to_string())?;
    send.write_all(&bytes).await.map_err(|e| e.to_string())?;
    let _ = send.finish();
    Ok(())
}

/// Let `peer` in: it holds the grant, a handler exists, and it has a free
/// call slot. The permit holds the slot until dropped.
fn admit(
    inner: &Inner,
    peer: DeviceId,
) -> Result<(Arc<dyn ControlHandler>, OwnedSemaphorePermit), RemoteError> {
    if !inner.may_manage(peer) {
        return Err(RemoteError::new(
            RemoteErrorCode::Forbidden,
            "this device has not allowed yours to manage it",
        ));
    }
    let Some(handler) = inner.control.clone() else {
        return Err(RemoteError::new(
            RemoteErrorCode::Unsupported,
            "this device does not answer remote calls",
        ));
    };
    let Some(slots) = inner.control_slots(peer) else {
        return Err(RemoteError::new(RemoteErrorCode::Offline, "session closed"));
    };
    let permit = slots.try_acquire_owned().map_err(|_| {
        RemoteError::new(RemoteErrorCode::Busy, "too many calls in flight; try again")
    })?;
    Ok((handler, permit))
}

/// Run handler work on a blocking thread, bounded by [`HANDLER_TIMEOUT`].
async fn on_handler<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, RemoteError> + Send + 'static,
) -> Result<T, RemoteError> {
    match tokio::time::timeout(HANDLER_TIMEOUT, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(failed(join)),
        Err(_) => Err(RemoteError::new(
            RemoteErrorCode::Timeout,
            "this call took too long here (a permission prompt may be waiting)",
        )),
    }
}

async fn answer(inner: &Inner, peer: DeviceId, request: ControlRequest) -> RemoteResult {
    let (handler, _permit) = admit(inner, peer)?;
    let call = call_from_wire(request)
        .map_err(|err| RemoteError::new(RemoteErrorCode::Invalid, err.to_string()))?;
    on_handler(move || handler.handle(peer, call)).await
}

/// Answer one read-file request: a header, then exactly `size` bytes.
pub(crate) async fn serve_read(
    inner: &Inner,
    peer: DeviceId,
    mut send: SendStream,
    request: ReadFileRequest,
) -> Result<(), String> {
    let opened = async {
        let (handler, permit) = admit(inner, peer)?;
        let ReadFileRequest { path, max_bytes } = request;
        let file = on_handler(move || handler.open_file(peer, &path, max_bytes)).await?;
        let meta = file.metadata().map_err(failed)?;
        Ok::<_, RemoteError>((file, meta, permit))
    }
    .await;
    let (file, meta, _permit) = match opened {
        Ok(opened) => opened,
        Err(err) => {
            tracing::debug!(peer = %peer, code = err.code.as_str(), error = %err, "read-only copy refused");
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
    let size = meta.len();
    let header = ObjectHeader {
        found: true,
        size,
        error: None,
        modified_ms: modified_ms(&meta),
    };
    let bytes = encode_frame(&header).map_err(|e| e.to_string())?;
    send.write_all(&bytes).await.map_err(|e| e.to_string())?;
    // A file that grows meanwhile is cut at `size`; one that shrinks ends
    // short and the caller refuses it.
    let mut body = tokio::fs::File::from_std(file).take(size);
    tokio::io::copy(&mut body, &mut send)
        .await
        .map_err(|e| e.to_string())?;
    let _ = send.finish();
    Ok(())
}

/// Copy one file from `peer` into `dest`, which must not exist yet.
pub(crate) async fn read_file(
    inner: Arc<Inner>,
    peer: DeviceId,
    path: String,
    max_bytes: u64,
    dest: PathBuf,
) -> Result<CopiedFile, RemoteError> {
    let conn = connection(&inner, peer)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(failed)?;
    let request = encode_frame(&ObjectRequest {
        read_file: Some(ReadFileRequest { path, max_bytes }),
        ..Default::default()
    })
    .map_err(failed)?;
    send.write_all(&request).await.map_err(failed)?;
    send.finish().map_err(failed)?;
    let header: ObjectHeader = tokio::time::timeout(CALL_TIMEOUT, read_message(&mut recv))
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
            RemoteErrorCode::NotFound,
            "that file is gone",
        ));
    }
    let partial = dest.with_extension("relay-partial");
    let written = receive(&mut recv, &partial, header.size).await;
    if let Err(err) = written {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(err);
    }
    tokio::fs::rename(&partial, &dest).await.map_err(failed)?;
    Ok(CopiedFile {
        size: header.size,
        modified_ms: header.modified_ms,
    })
}

async fn receive(recv: &mut RecvStream, dest: &Path, size: u64) -> Result<(), RemoteError> {
    let mut file = tokio::fs::File::create_new(dest).await.map_err(failed)?;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut have = 0u64;
    while have < size {
        let read = tokio::time::timeout(COPY_IDLE_TIMEOUT, recv.read(&mut buf))
            .await
            .map_err(|_| RemoteError::new(RemoteErrorCode::Timeout, "the copy stalled"))?
            .map_err(failed)?;
        let Some(n) = read else {
            break;
        };
        let n = n.min(usize::try_from(size - have).unwrap_or(usize::MAX));
        file.write_all(&buf[..n]).await.map_err(failed)?;
        have += n as u64;
    }
    if have != size {
        return Err(RemoteError::new(
            RemoteErrorCode::Failed,
            "the file changed on that device while it was copied; try again",
        ));
    }
    file.sync_all().await.map_err(failed)?;
    Ok(())
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// `PeerGrants` frame telling `peer` whether it may manage this device.
pub(crate) fn grants_frame(may_manage_you: bool) -> Option<Vec<u8>> {
    encode_frame(&Frame::new(Body::PeerGrants(PeerGrants { may_manage_you }))).ok()
}

fn failed(err: impl ToString) -> RemoteError {
    RemoteError::new(RemoteErrorCode::Failed, err.to_string())
}
