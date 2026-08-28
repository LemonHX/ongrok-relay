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

pub(crate) fn validate_run_options(options: &RunOptions) -> Result<()> {
    if options.tcp_port_start > options.tcp_port_end {
        anyhow::bail!("tcp port start must not exceed tcp port end");
    }
    if (options.http_listen.is_some() || options.https_listen.is_some())
        != options.http_domain.is_some()
    {
        anyhow::bail!(
            "--http-domain is required when --http-listen or --https-listen is configured"
        );
    }
    if options.http_domain.as_deref().is_some_and(|domain| {
        domain.is_empty() || domain.len() > 253 || domain.chars().any(char::is_whitespace)
    }) {
        anyhow::bail!("--http-domain must be a non-empty hostname without whitespace");
    }
    let tcp_listeners = [
        ("api", options.api_listen),
        ("tcp-tls", options.tcp_tls_listen),
        (
            "http",
            options
                .http_listen
                .unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid wildcard address")),
        ),
        (
            "https",
            options
                .https_listen
                .unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid wildcard address")),
        ),
    ];
    for (index, (left_name, left)) in tcp_listeners.iter().enumerate() {
        if *left
            == "0.0.0.0:0"
                .parse::<SocketAddr>()
                .expect("valid wildcard address")
        {
            continue;
        }
        for (right_name, right) in tcp_listeners.iter().skip(index + 1) {
            if *right
                != "0.0.0.0:0"
                    .parse::<SocketAddr>()
                    .expect("valid wildcard address")
                && left == right
            {
                anyhow::bail!(
                    "{left_name} and {right_name} listeners use the same TCP address {left}"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{RunOptions, validate_run_options};
    use std::net::SocketAddr;
    use std::path::PathBuf;

    fn options() -> RunOptions {
        RunOptions {
            tls_cert: PathBuf::from("cert.pem"),
            tls_key: PathBuf::from("key.pem"),
            api_listen: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
            quic_listen: "0.0.0.0:443".parse::<SocketAddr>().unwrap(),
            tcp_tls_listen: "0.0.0.0:443".parse::<SocketAddr>().unwrap(),
            http_listen: None,
            https_listen: None,
            http_domain: None,
            public_host: "example.test".to_owned(),
            tcp_port_start: 20_000,
            tcp_port_end: 30_000,
            admin_token: "admin".to_owned(),
            user_token: "user".to_owned(),
            db_path: PathBuf::from("ongrok.redb"),
        }
    }

    #[test]
    fn rejects_reversed_port_range() {
        let mut value = options();
        value.tcp_port_start = 30_001;
        assert!(validate_run_options(&value).is_err());
    }

    #[test]
    fn requires_domain_for_http_listener() {
        let mut value = options();
        value.http_listen = Some("0.0.0.0:80".parse().unwrap());
        assert!(validate_run_options(&value).is_err());
    }

    #[test]
    fn accepts_valid_http_configuration() {
        let mut value = options();
        value.https_listen = Some("0.0.0.0:443".parse().unwrap());
        value.http_domain = Some("relay.example.test".to_owned());
        value.tcp_tls_listen = "0.0.0.0:8443".parse().unwrap();
        assert!(validate_run_options(&value).is_ok());
    }

    #[test]
    fn rejects_tcp_listener_address_conflicts() {
        let mut value = options();
        value.http_listen = Some("0.0.0.0:8443".parse().unwrap());
        value.http_domain = Some("relay.example.test".to_owned());
        value.tcp_tls_listen = "0.0.0.0:8443".parse().unwrap();
        assert!(validate_run_options(&value).is_err());
    }
}
