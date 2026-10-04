//! Remote calls between devices (D37).
//!
//! Each call rides its own bidirectional stream (see `relay_proto::control`).
//! The answering side checks the manage grant before anything else sees the
//! request, then runs the daemon's [`ControlHandler`] on a blocking thread
//! with a timeout: a filesystem call can stall, for example on a macOS
//! privacy prompt nobody is there to answer.

use std::sync::Arc;
use std::time::Duration;

use quinn::{RecvStream, SendStream};
use relay_core::DeviceId;
use relay_core::remote::{RemoteCall, RemoteError, RemoteErrorCode, RemoteResult};
use relay_proto::{
    ControlRequest, ControlResponse, FEATURE_CONTROL, Frame, ObjectRequest, PeerGrants,
    call_from_wire, call_to_wire, encode_frame, frame::Body, result_from_wire, result_to_wire,
};

use crate::io::read_message;
use crate::session::Inner;

/// How long one call may run on the answering device.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a caller waits for an answer, including the network.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Calls one peer may have running here at once.
pub(crate) const MAX_CONCURRENT_CALLS: usize = 4;

/// Answers remote calls from peers that hold the manage grant. The daemon
/// implements this; the network layer only checks the grant and transports.
pub trait ControlHandler: Send + Sync {
    /// Runs on a blocking thread. `peer` already holds the grant here, but an
    /// implementation should check its own records too.
    fn handle(&self, peer: DeviceId, call: RemoteCall) -> RemoteResult;
}

/// Make `call` on `peer` and wait for its answer.
pub(crate) async fn call(inner: Arc<Inner>, peer: DeviceId, call: RemoteCall) -> RemoteResult {
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
    let exchange = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(failed)?;
        let request = encode_frame(&ObjectRequest {
            object_id: Vec::new(),
            control: Some(call_to_wire(&call)),
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

async fn answer(inner: &Inner, peer: DeviceId, request: ControlRequest) -> RemoteResult {
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
    let call = call_from_wire(request)
        .map_err(|err| RemoteError::new(RemoteErrorCode::Invalid, err.to_string()))?;
    let Some(slots) = inner.control_slots(peer) else {
        return Err(RemoteError::new(RemoteErrorCode::Offline, "session closed"));
    };
    let _permit = slots.try_acquire_owned().map_err(|_| {
        RemoteError::new(RemoteErrorCode::Busy, "too many calls in flight; try again")
    })?;
    let work = tokio::task::spawn_blocking(move || handler.handle(peer, call));
    match tokio::time::timeout(HANDLER_TIMEOUT, work).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(failed(join)),
        Err(_) => Err(RemoteError::new(
            RemoteErrorCode::Timeout,
            "this call took too long here (a permission prompt may be waiting)",
        )),
    }
}

/// `PeerGrants` frame telling `peer` whether it may manage this device.
pub(crate) fn grants_frame(may_manage_you: bool) -> Option<Vec<u8>> {
    encode_frame(&Frame::new(Body::PeerGrants(PeerGrants { may_manage_you }))).ok()
}

fn failed(err: impl ToString) -> RemoteError {
    RemoteError::new(RemoteErrorCode::Failed, err.to_string())
}
