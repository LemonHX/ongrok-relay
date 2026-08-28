//! Network listener adapters for the relay transports.

use crate::{
    server::{handle_quic_connection, handle_yamux_session},
    state::AppState,
};
use anyhow::{Context, Result};
use libongrok::YamuxSession;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::ServerConfig;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, time::timeout};
use tokio_rustls::TlsAcceptor;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{info, warn};

pub(crate) async fn run_quic(
    address: SocketAddr,
    tls: Arc<ServerConfig>,
    state: AppState,
) -> Result<()> {
    let mut crypto = (*tls).clone();
    crypto.alpn_protocols = vec![b"ongrok/1".to_vec()];
    let mut config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    Arc::get_mut(&mut config.transport)
        .expect("fresh QUIC transport config")
        .max_concurrent_uni_streams(0_u8.into());
    let endpoint = quinn::Endpoint::server(config, address)
        .with_context(|| format!("failed to bind QUIC listener at {address}"))?;
    info!(address = %endpoint.local_addr()?, "QUIC listener ready");
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_quic_connection(incoming, state).await {
                warn!(%error, "QUIC client session failed");
            }
        });
    }
    Ok(())
}

pub(crate) async fn run_tcp_tls(
    address: SocketAddr,
    tls: Arc<ServerConfig>,
    state: AppState,
) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind TCP/TLS listener at {address}"))?;
    let mut config = (*tls).clone();
    config.alpn_protocols = vec![b"ongrok/1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    info!(%address, "TCP/TLS fallback listener ready");
    loop {
        let (socket, remote) = listener.accept().await.context("TCP/TLS accept failed")?;
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let result = async {
                let stream = timeout(Duration::from_secs(15), acceptor.accept(socket))
                    .await
                    .context("TCP/TLS handshake timed out")??;
                if stream.get_ref().1.alpn_protocol() != Some(b"ongrok/1") {
                    anyhow::bail!("TCP/TLS client did not negotiate ongrok/1");
                }
                let session = YamuxSession::spawn(stream.compat(), yamux::Mode::Server);
                let control = timeout(Duration::from_secs(10), session.next_inbound())
                    .await
                    .context("client did not open a Yamux control stream")?
                    .context("Yamux session ended before opening a control stream")?;
                handle_yamux_session(control, session, remote, state).await
            }
            .await;
            if let Err(error) = result {
                warn!(%remote, %error, "TCP/TLS client session failed");
            }
        });
    }
}
