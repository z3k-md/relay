use quinn::RecvStream;
use relay_proto::{ProtoError, decode_message, frame_len};

#[derive(Debug, thiserror::Error)]
pub(crate) enum IoErr {
    #[error("stream ended before the frame was complete")]
    UnexpectedEnd,
    #[error("read error: {0}")]
    Read(String),
    #[error(transparent)]
    Proto(#[from] ProtoError),
}

impl IoErr {
    pub(crate) fn is_malformed(&self) -> bool {
        matches!(self, IoErr::Proto(_))
    }
}

pub(crate) async fn read_exact(recv: &mut RecvStream, buf: &mut [u8]) -> Result<(), IoErr> {
    let mut off = 0;
    while off < buf.len() {
        match recv.read(&mut buf[off..]).await {
            Ok(Some(n)) => off += n,
            Ok(None) => return Err(IoErr::UnexpectedEnd),
            Err(e) => return Err(IoErr::Read(e.to_string())),
        }
    }
    Ok(())
}

/// Read one length-prefixed protobuf message (`encode_frame` layout).
pub(crate) async fn read_message<M: prost::Message + Default>(
    recv: &mut RecvStream,
) -> Result<M, IoErr> {
    let mut prefix = [0u8; 4];
    read_exact(recv, &mut prefix).await?;
    let len = frame_len(prefix)?;
    let mut buf = vec![0u8; len];
    read_exact(recv, &mut buf).await?;
    Ok(decode_message(&buf)?)
}
