//! Shared protocol and domain types for ongrok relay components.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex, mpsc, oneshot, watch},
};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// A single bidirectional QUIC stream presented as Tokio byte I/O.
///
/// The first protocol frame is consumed before constructing this wrapper; the
/// remaining bytes can be handed directly to streaming protocols such as Hyper.
pub struct QuicIo {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

/// A Yamux stream exposed through Tokio's I/O traits.
///
/// Yamux deliberately uses `futures::io`; keeping this conversion here makes
/// the TCP/TLS fallback use the same Tokio-facing framing helpers as QUIC.
pub struct YamuxIo {
    inner: Compat<yamux::Stream>,
}

impl YamuxIo {
    pub fn new(stream: yamux::Stream) -> Self {
        Self {
            inner: stream.compat(),
        }
    }
}

/// Owns a Yamux connection in one task and exposes streams through Tokio I/O.
///
/// `yamux::Connection` is a poll-driven state machine. It must never be
/// polled concurrently by the control-plane task and ingress tasks, so callers
/// request outbound streams and receive inbound streams through this handle.
#[derive(Clone)]
pub struct YamuxSession {
    outbound: mpsc::Sender<oneshot::Sender<io::Result<YamuxIo>>>,
    inbound: std::sync::Arc<Mutex<mpsc::Receiver<YamuxIo>>>,
    shutdown: watch::Sender<()>,
}

impl YamuxSession {
    pub fn spawn<T>(socket: T, mode: yamux::Mode) -> Self
    where
        T: futures::AsyncRead + futures::AsyncWrite + Send + Unpin + 'static,
    {
        let (outbound_tx, mut outbound_rx) =
            mpsc::channel::<oneshot::Sender<io::Result<YamuxIo>>>(128);
        let (inbound_tx, inbound_rx) = mpsc::channel::<YamuxIo>(128);
        let (shutdown, mut shutdown_rx) = watch::channel(());
        tokio::spawn(async move {
            let mut connection = yamux::Connection::new(socket, yamux::Config::default(), mode);
            loop {
                tokio::select! {
                    request = outbound_rx.recv() => {
                        let Some(response) = request else { break };
                        let result = std::future::poll_fn(|cx| connection.poll_new_outbound(cx))
                            .await
                            .map(YamuxIo::new)
                            .map_err(yamux_io_error);
                        let _ = response.send(result);
                    }
                    next = std::future::poll_fn(|cx| connection.poll_next_inbound(cx)) => {
                        match next {
                            Some(Ok(stream)) => {
                                if inbound_tx.send(YamuxIo::new(stream)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Err(error)) => {
                                tracing::debug!(%error, "Yamux session stopped");
                                break;
                            }
                            None => break,
                        }
                    }
                    _ = shutdown_rx.changed() => break,
                }
            }
        });
        Self {
            outbound: outbound_tx,
            inbound: std::sync::Arc::new(Mutex::new(inbound_rx)),
            shutdown,
        }
    }

    pub async fn open_stream(&self) -> io::Result<YamuxIo> {
        let (response_tx, response_rx) = oneshot::channel();
        self.outbound
            .send(response_tx)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Yamux session is closed"))?;
        response_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Yamux session is closed"))?
    }

    pub async fn next_inbound(&self) -> Option<YamuxIo> {
        self.inbound.lock().await.recv().await
    }

    pub fn close(&self) {
        let _ = self.shutdown.send(());
    }
}

fn yamux_io_error(error: yamux::ConnectionError) -> io::Error {
    io::Error::other(error)
}

impl AsyncRead for YamuxIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl AsyncWrite for YamuxIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncRead for QuicIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buffer)
    }
}

impl AsyncWrite for QuicIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        <quinn::SendStream as AsyncWrite>::poll_write(Pin::new(&mut self.send), cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <quinn::SendStream as AsyncWrite>::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        <quinn::SendStream as AsyncWrite>::poll_shutdown(Pin::new(&mut self.send), cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.send.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        <quinn::SendStream as AsyncWrite>::poll_write_vectored(
            Pin::new(&mut self.send),
            cx,
            buffers,
        )
    }
}

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_id!(NodeId);
uuid_id!(ServiceId);
uuid_id!(TunnelId);
uuid_id!(PortLeaseId);
uuid_id!(EventId);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TokenKind {
    Admin,
    User,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Protocol {
    Http,
    Https,
    Tcp,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TransportKind {
    Quic,
    TcpTlsYamux,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ServiceStatus {
    Online,
    Offline,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NodeStatus {
    Online,
    Offline,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EventKind {
    NodeOnline,
    NodeOffline,
    ServiceRegistered,
    ServiceDeleted,
    TokenRotated,
    TokenRevoked,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Metadata(pub BTreeMap<String, String>);
impl Metadata {
    pub const MAX_ENTRIES: usize = 32;
    pub const MAX_KEY_LEN: usize = 64;
    pub const MAX_VALUE_LEN: usize = 256;
    pub const MAX_TOTAL_BYTES: usize = 4096;
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.0.len() > Self::MAX_ENTRIES {
            return Err(ValidationError::TooManyMetadataEntries);
        }
        let mut total = 0usize;
        for (key, value) in &self.0 {
            let valid = !key.is_empty()
                && key.len() <= Self::MAX_KEY_LEN
                && key.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'-'
                        || b == b'_'
                        || b == b'.'
                });
            if !valid {
                return Err(ValidationError::InvalidMetadataKey(key.clone()));
            }
            if value.len() > Self::MAX_VALUE_LEN {
                return Err(ValidationError::MetadataValueTooLong(key.clone()));
            }
            total = total.saturating_add(key.len()).saturating_add(value.len());
        }
        if total > Self::MAX_TOTAL_BYTES {
            return Err(ValidationError::MetadataTooLarge);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("too many metadata entries")]
    TooManyMetadataEntries,
    #[error("invalid metadata key: {0}")]
    InvalidMetadataKey(String),
    #[error("metadata value too long for key: {0}")]
    MetadataValueTooLong(String),
    #[error("metadata exceeds total size limit")]
    MetadataTooLarge,
    #[error("service name is invalid")]
    InvalidServiceName,
}
pub fn validate_service_name(name: &str) -> Result<(), ValidationError> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(ValidationError::InvalidServiceName)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeMetadata {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub client_version: String,
    pub metadata: Metadata,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeRecord {
    pub node_id: NodeId,
    #[serde(default)]
    pub public_key: Option<[u8; 32]>,
    pub metadata: NodeMetadata,
    pub public_ip: String,
    pub source_port: u16,
    pub transport: TransportKind,
    pub status: NodeStatus,
    pub connected_at_unix_ms: i64,
    pub last_heartbeat_at_unix_ms: Option<i64>,
    pub rtt_ms: Option<u32>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceDefinition {
    pub service_id: ServiceId,
    pub service_name: String,
    pub node_id: NodeId,
    pub protocol: Protocol,
    pub local_address: String,
    pub public_host: Option<String>,
    pub public_port: Option<u16>,
    pub metadata: Metadata,
}
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct HeartbeatSnapshot {
    pub sequence: u64,
    pub sent_at_unix_ms: i64,
    pub cpu_percent: Option<f32>,
    pub memory_percent: Option<f32>,
    pub load_average: Option<f32>,
    pub network_rx_bytes: Option<u64>,
    pub network_tx_bytes: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NodeMetric {
    pub node_id: NodeId,
    pub recorded_at_unix_ms: i64,
    pub rtt_ms: Option<u32>,
    pub snapshot: HeartbeatSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlEvent {
    pub event_id: EventId,
    pub occurred_at_unix_ms: i64,
    pub kind: EventKind,
    pub node_id: Option<NodeId>,
    pub service_id: Option<ServiceId>,
    pub token_kind: Option<TokenKind>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Frame {
    Hello {
        version: u16,
    },
    Auth {
        token: String,
        node_id: NodeId,
    },
    AuthAccepted {
        kind: TokenKind,
    },
    AuthRejected,
    RegisterNode {
        metadata: NodeMetadata,
        public_key: [u8; 32],
    },
    RegisterService {
        service: ServiceDefinition,
    },
    RegisterServiceAccepted {
        service: ServiceDefinition,
    },
    UnregisterService {
        service_id: ServiceId,
    },
    ServiceList {
        services: Vec<ServiceDefinition>,
    },
    Heartbeat {
        snapshot: HeartbeatSnapshot,
    },
    HeartbeatAck {
        sequence: u64,
        server_time_unix_ms: i64,
    },
    OpenStream {
        tunnel_id: TunnelId,
        service_id: ServiceId,
    },
    OpenStreamAck {
        tunnel_id: TunnelId,
    },
    StreamData {
        tunnel_id: TunnelId,
        data: Bytes,
    },
    CloseStream {
        tunnel_id: TunnelId,
    },
    Error {
        message: String,
    },
    Goodbye,
}
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CodecError {
    #[error("frame exceeds maximum size")]
    FrameTooLarge,
    #[error("invalid frame length")]
    InvalidLength,
    #[error("malformed frame: {0}")]
    Malformed(String),
}
pub fn encode_frame(frame: &Frame) -> Result<Bytes, CodecError> {
    let payload = postcard::to_allocvec(frame).map_err(|e| CodecError::Malformed(e.to_string()))?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(CodecError::FrameTooLarge);
    }
    let mut out = BytesMut::with_capacity(4 + payload.len());
    out.put_u32(payload.len() as u32);
    out.extend_from_slice(&payload);
    Ok(out.freeze())
}
pub fn decode_frames(buffer: &mut BytesMut) -> Result<Vec<Frame>, CodecError> {
    let mut frames = Vec::new();
    loop {
        if buffer.len() < 4 {
            break;
        }
        let len = (&buffer[..4]).get_u32() as usize;
        if len > MAX_FRAME_SIZE {
            return Err(CodecError::FrameTooLarge);
        }
        if len == 0 {
            return Err(CodecError::InvalidLength);
        }
        if buffer.len() < 4 + len {
            break;
        }
        buffer.advance(4);
        let payload = buffer.split_to(len);
        frames.push(decode_frame_payload(&payload)?);
    }
    Ok(frames)
}

pub fn decode_frame_payload(payload: &[u8]) -> Result<Frame, CodecError> {
    postcard::from_bytes(payload).map_err(|e| CodecError::Malformed(e.to_string()))
}
pub fn generate_token(kind: TokenKind) -> (String, [u8; 32]) {
    let mut raw = [0u8; 32];
    rand::rng().fill(&mut raw);
    let prefix = match kind {
        TokenKind::Admin => "admin",
        TokenKind::User => "user",
    };
    let token = format!("ongrok_{prefix}_{}", hex_encode(&raw));
    (token.clone(), hash_token(&token))
}
pub fn hash_token(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::compat::TokioAsyncReadCompatExt;
    #[test]
    fn metadata_and_names_validate() {
        let mut m = Metadata::default();
        m.0.insert("env".into(), "prod".into());
        assert!(m.validate().is_ok());
        m.0.insert("Bad".into(), "x".into());
        assert!(matches!(
            m.validate(),
            Err(ValidationError::InvalidMetadataKey(_))
        ));
        assert!(validate_service_name("ssh-1").is_ok());
        assert!(validate_service_name("SSH").is_err());
    }
    #[test]
    fn codec_handles_partial_and_multiple_frames() {
        let a = encode_frame(&Frame::Hello {
            version: PROTOCOL_VERSION,
        })
        .unwrap();
        let b = encode_frame(&Frame::Goodbye).unwrap();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&a[..2]);
        assert!(decode_frames(&mut buf).unwrap().is_empty());
        buf.extend_from_slice(&a[2..]);
        buf.extend_from_slice(&b);
        let frames = decode_frames(&mut buf).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(buf.is_empty());
    }
    #[test]
    fn tokens_hash_consistently() {
        let (token, hash) = generate_token(TokenKind::User);
        assert!(token.starts_with("ongrok_user_"));
        assert_eq!(hash, hash_token(&token));
    }

    #[tokio::test]
    async fn yamux_adapter_carries_a_multiplexed_stream() {
        let (client_socket, server_socket) = tokio::io::duplex(16 * 1024);
        let client = YamuxSession::spawn(client_socket.compat(), yamux::Mode::Client);
        let server = YamuxSession::spawn(server_socket.compat(), yamux::Mode::Server);
        let mut client_stream = client.open_stream().await.unwrap();
        client_stream.write_all(b"ongrok-yamux").await.unwrap();
        client_stream.shutdown().await.unwrap();
        let mut stream = server
            .next_inbound()
            .await
            .expect("connection must stay open");
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"ongrok-yamux");
    }
}
