//! Command-line configuration and TLS material loading for the relay server.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use libongrok::TokenKind;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::{ServerConfig, pki_types::CertificateDer};
use std::{fs::File, io::BufReader, net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser, Debug)]
#[command(name = "ongrok-relay-server", version, about = "ongrok relay server")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Initialize an empty control database and print fresh long-lived tokens.
    Init {
        #[arg(long, env = "ONGROK_DB_PATH", default_value = "ongrok.redb")]
        db_path: PathBuf,
    },
    /// Validate certificate, key, and database paths without starting listeners.
    Doctor {
        #[arg(long, env = "ONGROK_TLS_CERT")]
        tls_cert: PathBuf,
        #[arg(long, env = "ONGROK_TLS_KEY")]
        tls_key: PathBuf,
        #[arg(long, env = "ONGROK_DB_PATH", default_value = "ongrok.redb")]
        db_path: PathBuf,
    },
    /// Print a new long-lived token.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Validate certificate material and start listeners.
    Run {
        #[command(flatten)]
        options: Box<RunOptions>,
    },
}

#[derive(Args, Debug)]
pub(crate) struct RunOptions {
    #[arg(long, env = "ONGROK_TLS_CERT")]
    pub(crate) tls_cert: PathBuf,
    #[arg(long, env = "ONGROK_TLS_KEY")]
    pub(crate) tls_key: PathBuf,
    #[arg(long, env = "ONGROK_API_LISTEN", default_value = "127.0.0.1:8080")]
    pub(crate) api_listen: SocketAddr,
    #[arg(long, env = "ONGROK_QUIC_LISTEN", default_value = "0.0.0.0:443")]
    pub(crate) quic_listen: SocketAddr,
    #[arg(long, env = "ONGROK_TCP_TLS_LISTEN", default_value = "0.0.0.0:443")]
    pub(crate) tcp_tls_listen: SocketAddr,
    #[arg(long, env = "ONGROK_HTTP_LISTEN")]
    pub(crate) http_listen: Option<SocketAddr>,
    #[arg(long, env = "ONGROK_HTTPS_LISTEN")]
    pub(crate) https_listen: Option<SocketAddr>,
    #[arg(long, env = "ONGROK_HTTP_DOMAIN")]
    pub(crate) http_domain: Option<String>,
    #[arg(long, env = "ONGROK_PUBLIC_HOST", default_value = "localhost")]
    pub(crate) public_host: String,
    #[arg(long, env = "ONGROK_TCP_PORT_START", default_value_t = 20_000)]
    pub(crate) tcp_port_start: u16,
    #[arg(long, env = "ONGROK_TCP_PORT_END", default_value_t = 30_000)]
    pub(crate) tcp_port_end: u16,
    #[arg(long, env = "ONGROK_ADMIN_TOKEN")]
    pub(crate) admin_token: String,
    #[arg(long, env = "ONGROK_USER_TOKEN")]
    pub(crate) user_token: String,
    #[arg(long, env = "ONGROK_DB_PATH", default_value = "ongrok.redb")]
    pub(crate) db_path: PathBuf,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TokenCommand {
    Create {
        #[arg(long, value_enum)]
        kind: TokenKindArg,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum TokenKindArg {
    Admin,
    User,
}

impl From<TokenKindArg> for TokenKind {
    fn from(value: TokenKindArg) -> Self {
        match value {
            TokenKindArg::Admin => Self::Admin,
            TokenKindArg::User => Self::User,
        }
    }
}

pub(crate) fn validate_tls_material(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<Arc<ServerConfig>> {
    let mut cert_reader = BufReader::new(
        File::open(cert_path).with_context(|| format!("failed to open {}", cert_path.display()))?,
    );
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<CertificateDer<'static>>, _>>()
        .context("failed to parse TLS certificate chain")?;
    if certificates.is_empty() {
        anyhow::bail!("TLS certificate chain is empty");
    }

    let mut key_reader = BufReader::new(
        File::open(key_path).with_context(|| format!("failed to open {}", key_path.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("failed to parse TLS private key")?
        .context("TLS private key is missing")?;
    let crypto = rustls::crypto::ring::default_provider();
    let server = ServerConfig::builder_with_provider(crypto.into())
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .context("TLS certificate and private key do not match")?;
    let mut server = server;
    server.alpn_protocols = vec![b"ongrok/1".to_vec()];
    let _ = QuicServerConfig::try_from(server.clone()).context("failed to configure QUIC TLS")?;
    Ok(Arc::new(server))
}
