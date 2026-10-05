use quinn::RecvStream;
use relay_proto::{MAX_FRAME_BYTES, ProtoError, decode_message, frame_len};

#[derive(Debug, thiserror::Error)]
pub(crate) enum IoErr {
    #[error("stream ended before the frame was complete")]
    UnexpectedEnd,
    #[error("read error: {0}")]
    Read(String),
    #[error("message of {len} bytes exceeds the {max} byte limit for its type")]
    TooLarge { len: usize, max: usize },
    #[error(transparent)]
    Proto(#[from] ProtoError),
}

impl IoErr {
    pub(crate) fn is_malformed(&self) -> bool {
        matches!(self, IoErr::Proto(_) | IoErr::TooLarge { .. })
    }
}

/// Length of the message a prefix announces, refused before anything is
/// allocated when it exceeds `max` (or the frame limit).
fn bounded_len(prefix: [u8; 4], max: usize) -> Result<usize, IoErr> {
    let len = frame_len(prefix)?;
    if len > max {
        return Err(IoErr::TooLarge { len, max });
    }
    Ok(len)
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

/// Read one length-prefixed protobuf message (`encode_frame` layout) of up
/// to [`MAX_FRAME_BYTES`]. Only a `Frame` needs that much; see
/// [`read_message_max`].
pub(crate) async fn read_message<M: prost::Message + Default>(
    recv: &mut RecvStream,
) -> Result<M, IoErr> {
    read_message_max(recv, MAX_FRAME_BYTES).await
}

/// Like [`read_message`] with a tighter bound: the prefix is untrusted, and
/// the buffer it sizes should match what the message type can hold. Same
/// wire layout.
pub(crate) async fn read_message_max<M: prost::Message + Default>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<M, IoErr> {
    let mut prefix = [0u8; 4];
    read_exact(recv, &mut prefix).await?;
    let len = bounded_len(prefix, max)?;
    let mut buf = vec![0u8; len];
    read_exact(recv, &mut buf).await?;
    Ok(decode_message(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_message_is_refused_before_allocation() {
        let prefix = 4097u32.to_be_bytes();
        let err = bounded_len(prefix, 4096).unwrap_err();
        assert!(
            matches!(
                err,
                IoErr::TooLarge {
                    len: 4097,
                    max: 4096
                }
            ),
            "{err}"
        );
        assert!(err.is_malformed());
        assert_eq!(bounded_len(4096u32.to_be_bytes(), 4096).unwrap(), 4096);
        // The frame limit still applies above any per-type bound.
        let huge = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_be_bytes();
        assert!(matches!(
            bounded_len(huge, usize::MAX),
            Err(IoErr::Proto(ProtoError::FrameTooLarge(_)))
        ));
    }
}
