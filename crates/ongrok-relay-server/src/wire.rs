//! Bounded control-frame I/O shared by QUIC and Yamux sessions.

use anyhow::{Context, Result};
use libongrok::{Frame, MAX_FRAME_SIZE, decode_frame_payload, encode_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) async fn read_control_frame<R>(recv: &mut R) -> Result<Frame>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut header = [0u8; 4];
    recv.read_exact(&mut header)
        .await
        .context("failed to read control frame length")?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_SIZE {
        anyhow::bail!("invalid control frame length {length}");
    }
    let mut payload = vec![0; length];
    recv.read_exact(&mut payload)
        .await
        .context("failed to read control frame payload")?;
    decode_frame_payload(&payload).map_err(Into::into)
}

pub(crate) async fn write_control_frame<W>(send: &mut W, frame: &Frame) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let encoded = encode_frame(frame)?;
    send.write_all(&encoded)
        .await
        .context("failed to write control frame")?;
    Ok(())
}

pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
