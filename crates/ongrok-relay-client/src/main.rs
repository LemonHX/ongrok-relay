use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use ed25519_dalek::SigningKey;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, Uri, header};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use libongrok::{
    Frame, HeartbeatSnapshot, MAX_FRAME_SIZE, Metadata, NodeId, NodeMetadata, PROTOCOL_VERSION,
    Protocol, QuicIo, ServiceDefinition, ServiceId, YamuxSession, decode_frame_payload,
    encode_frame, validate_service_name,
};
use mimalloc::MiMalloc;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, ServerName},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::BufReader,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};
use sysinfo::{Networks, System};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy},
    net::TcpStream,
    sync::Mutex,
    time::{Duration, timeout},
};
use tokio_rustls::TlsConnector;
use tokio_util::compat::TokioAsyncReadCompatExt;

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

trait RelayIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> RelayIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}
type BoxedIo = Box<dyn RelayIo>;

#[derive(Parser, Debug)]
#[command(name = "ongrok-relay-client", version, about = "ongrok relay client")]
struct Cli {
    #[arg(long, global = true, env = "ONGROK_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create or show the stable local node identity.
    Init,
    /// Show the stable local node identity.
    Status,
    /// Connect to the relay over QUIC and register this node.
    Run {
        #[arg(long, env = "ONGROK_QUIC_SERVER")]
        server: SocketAddr,
        #[arg(long, env = "ONGROK_SERVER_NAME", default_value = "localhost")]
        server_name: String,
        #[arg(long, env = "ONGROK_CA_CERT")]
        ca_cert: PathBuf,
        #[arg(long, env = "ONGROK_TCP_TLS_SERVER")]
        tcp_tls_server: Option<SocketAddr>,
        #[arg(long, env = "ONGROK_TOKEN")]
        token: String,
        #[arg(long)]
        once: bool,
    },
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Query the server's shared service directory.
    Services {
        #[command(subcommand)]
        command: ServicesCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    Publish {
        #[arg(long, env = "ONGROK_QUIC_SERVER")]
        server: SocketAddr,
        #[arg(long, env = "ONGROK_SERVER_NAME", default_value = "localhost")]
        server_name: String,
        #[arg(long, env = "ONGROK_CA_CERT")]
        ca_cert: PathBuf,
        #[arg(long, env = "ONGROK_TCP_TLS_SERVER")]
        tcp_tls_server: Option<SocketAddr>,
        #[arg(long, env = "ONGROK_TOKEN")]
        token: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        local_address: String,
        #[arg(long, value_enum)]
        protocol: ProtocolArg,
        #[arg(long)]
        public_host: Option<String>,
        #[arg(long)]
        public_port: Option<u16>,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ProtocolArg {
    Http,
    Https,
    Tcp,
}

impl From<ProtocolArg> for Protocol {
    fn from(value: ProtocolArg) -> Self {
        match value {
            ProtocolArg::Http => Self::Http,
            ProtocolArg::Https => Self::Https,
            ProtocolArg::Tcp => Self::Tcp,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ServicesCommand {
    List {
        #[arg(long, env = "ONGROK_SERVER")]
        server: String,
        #[arg(long, env = "ONGROK_TOKEN")]
        token: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct NodeState {
    node_id: NodeId,
    #[serde(default)]
    private_key: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
struct RelayEndpoints {
    quic: SocketAddr,
    tcp_tls: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Init | Command::Status => {
            let path = state_path(cli.state_dir)?;
            let state = load_or_create_state(&path)?;
            println!("node_id={}", state.node_id.0);
            println!("state_path={}", path.display());
            Ok(())
        }
        Command::Services {
            command: ServicesCommand::List { server, token },
        } => services_list(&server, &token).await,
        Command::Run {
            server,
            server_name,
            ca_cert,
            tcp_tls_server,
            token,
            once,
        } => {
            let state = load_or_create_state(&state_path(cli.state_dir)?)?;
            run_with_fallback(
                state,
                RelayEndpoints {
                    quic: server,
                    tcp_tls: tcp_tls_server,
                },
                &server_name,
                &ca_cert,
                &token,
                None,
                once,
            )
            .await
        }
        Command::Service {
            command:
                ServiceCommand::Publish {
                    server,
                    server_name,
                    ca_cert,
                    tcp_tls_server,
                    token,
                    name,
                    local_address,
                    protocol,
                    public_host,
                    public_port,
                    once,
                },
        } => {
            validate_service_name(&name)?;
            let state = load_or_create_state(&state_path(cli.state_dir)?)?;
            let service = ServiceDefinition {
                service_id: ServiceId::new(),
                service_name: name,
                node_id: state.node_id,
                protocol: protocol.into(),
                local_address,
                public_host,
                public_port,
                metadata: Metadata::default(),
            };
            run_with_fallback(
                state,
                RelayEndpoints {
                    quic: server,
                    tcp_tls: tcp_tls_server,
                },
                &server_name,
                &ca_cert,
                &token,
                Some(service),
                once,
            )
            .await
        }
    }
}

fn state_path(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    let base = match override_dir {
        Some(path) => path,
        None => ProjectDirs::from("moe", "lemonhx", "ongrok")
            .context("unable to determine an OS state directory")?
            .state_dir()
            .context("the current platform has no usable state directory")?
            .to_path_buf(),
    };
    Ok(base.join("node.json"))
}

fn load_or_create_state(path: &PathBuf) -> Result<NodeState> {
    if path.exists() {
        let json = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let mut state: NodeState = serde_json::from_slice(&json)
            .with_context(|| format!("invalid node state at {}", path.display()))?;
        if state.private_key.iter().all(|byte| *byte == 0) {
            state.private_key = random_private_key();
            write_state(path, &state)?;
        }
        return Ok(state);
    }
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let state = NodeState {
        node_id: NodeId::new(),
        private_key: random_private_key(),
    };
    write_state(path, &state)?;
    Ok(state)
}

fn random_private_key() -> [u8; 32] {
    std::array::from_fn(|_| rand::random())
}

fn write_state(path: &PathBuf, state: &NodeState) -> Result<()> {
    let json = serde_json::to_vec_pretty(&state)?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn node_public_key(state: &NodeState) -> [u8; 32] {
    SigningKey::from_bytes(&state.private_key)
        .verifying_key()
        .to_bytes()
}

async fn services_list(server: &str, token: &str) -> Result<()> {
    let base: Uri = server
        .parse()
        .context("server must be an absolute HTTP URL")?;
    let scheme = base.scheme_str().context("server URL needs a scheme")?;
    if scheme != "http" {
        anyhow::bail!("only http:// control API is available during this development phase");
    }
    let authority = base.authority().context("server URL needs an authority")?;
    let uri: Uri = format!("http://{authority}/v1/services")
        .parse()
        .context("failed to build services URL")?;
    let request = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Empty::<Bytes>::new())?;
    let client = Client::builder(TokioExecutor::new()).build_http();
    let response = client
        .request(request)
        .await
        .context("services request failed")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("failed to read services response")?
        .to_bytes();
    if !status.is_success() {
        anyhow::bail!(
            "server returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    println!("{}", String::from_utf8_lossy(&body));
    Ok(())
}

async fn run_with_fallback(
    state: NodeState,
    endpoints: RelayEndpoints,
    server_name: &str,
    ca_cert: &PathBuf,
    token: &str,
    service: Option<ServiceDefinition>,
    once: bool,
) -> Result<()> {
    let mut retry_attempt = 0_u32;
    loop {
        match run_once_with_fallback(
            state.clone(),
            endpoints,
            server_name,
            ca_cert,
            token,
            service.clone(),
            once,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) if once || is_authentication_error(&error) => return Err(error),
            Err(error) => {
                retry_attempt = retry_attempt.saturating_add(1);
                let base_delay = 2_u64.pow(retry_attempt.min(5));
                let jitter = rand::random_range(0..=base_delay / 3);
                let delay = Duration::from_secs(base_delay + jitter);
                tracing::warn!(
                    %error,
                    retry_attempt,
                    retry_after_seconds = delay.as_secs(),
                    "relay connection ended; retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn run_once_with_fallback(
    state: NodeState,
    endpoints: RelayEndpoints,
    server_name: &str,
    ca_cert: &PathBuf,
    token: &str,
    service: Option<ServiceDefinition>,
    once: bool,
) -> Result<()> {
    match timeout(
        Duration::from_secs(5),
        run_quic(
            state.clone(),
            endpoints.quic,
            server_name,
            ca_cert,
            token,
            service.clone(),
            once,
        ),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(%error, "QUIC unavailable; falling back to TCP/TLS Yamux");
            run_yamux(
                state,
                endpoints.tcp_tls.unwrap_or(endpoints.quic),
                server_name,
                ca_cert,
                token,
                service,
                once,
            )
            .await
        }
        Err(_) => {
            tracing::warn!("QUIC connection timed out; falling back to TCP/TLS Yamux");
            run_yamux(
                state,
                endpoints.tcp_tls.unwrap_or(endpoints.quic),
                server_name,
                ca_cert,
                token,
                service,
                once,
            )
            .await
        }
    }
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("rejected token") || message.contains("authentication rejected")
}

async fn run_yamux(
    state: NodeState,
    server: SocketAddr,
    server_name: &str,
    ca_cert: &PathBuf,
    token: &str,
    service: Option<ServiceDefinition>,
    once: bool,
) -> Result<()> {
    let roots = load_roots(ca_cert)?;
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"ongrok/1".to_vec()];
    let connector = TlsConnector::from(Arc::new(crypto));
    let socket = timeout(Duration::from_secs(10), TcpStream::connect(server))
        .await
        .context("TCP/TLS fallback connection timed out")??;
    let name = ServerName::try_from(server_name.to_owned())
        .context("server name is not a valid DNS name")?;
    let stream = timeout(Duration::from_secs(15), connector.connect(name, socket))
        .await
        .context("TCP/TLS fallback handshake timed out")??;
    if stream.get_ref().1.alpn_protocol() != Some(b"ongrok/1") {
        anyhow::bail!("relay did not negotiate ongrok/1 over TCP/TLS");
    }
    let session = YamuxSession::spawn(stream.compat(), yamux::Mode::Client);
    let mut control = session
        .open_stream()
        .await
        .context("failed to open Yamux control stream")?;
    write_control_frame(
        &mut control,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    match read_control_frame(&mut control).await? {
        Frame::Hello { version } if version == PROTOCOL_VERSION => {}
        Frame::Error { message } => anyhow::bail!("server rejected protocol: {message}"),
        _ => anyhow::bail!("server sent an invalid Hello response"),
    }
    write_control_frame(
        &mut control,
        &Frame::Auth {
            token: token.into(),
            node_id: state.node_id,
        },
    )
    .await?;
    match read_control_frame(&mut control).await? {
        Frame::AuthAccepted { .. } => {}
        Frame::AuthRejected => anyhow::bail!("server rejected token"),
        _ => anyhow::bail!("server sent an invalid auth response"),
    }
    write_control_frame(
        &mut control,
        &Frame::RegisterNode {
            metadata: local_node_metadata(),
            public_key: node_public_key(&state),
        },
    )
    .await?;
    let local_services = Arc::new(Mutex::new(BTreeMap::new()));
    if let Some(service) = service {
        let local_address = service.local_address.clone();
        write_control_frame(&mut control, &Frame::RegisterService { service }).await?;
        match read_control_frame(&mut control).await? {
            Frame::RegisterServiceAccepted { service } => {
                local_services
                    .lock()
                    .await
                    .insert(service.service_id, local_address);
                println!(
                    "published service={} endpoint={}{}",
                    service.service_name,
                    service.public_host.as_deref().unwrap_or("unassigned"),
                    service
                        .public_port
                        .map(|port| format!(":{port}"))
                        .unwrap_or_default()
                );
            }
            Frame::Error { message } => anyhow::bail!("server rejected service: {message}"),
            _ => anyhow::bail!("server sent an invalid service registration response"),
        }
    }
    let data_task = tokio::spawn(accept_yamux_data_streams(session, local_services));
    let mut sequence = 0;
    let mut system = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    loop {
        sequence += 1;
        write_control_frame(
            &mut control,
            &Frame::Heartbeat {
                snapshot: heartbeat_snapshot(sequence, &mut system, &mut networks),
            },
        )
        .await?;
        match read_control_frame(&mut control).await? {
            Frame::HeartbeatAck { sequence: ack, .. } if ack == sequence => {}
            _ => anyhow::bail!("server sent an invalid heartbeat acknowledgement"),
        }
        println!(
            "connected transport=tcp-tls-yamux node_id={} heartbeat={sequence}",
            state.node_id.0
        );
        if once {
            write_control_frame(&mut control, &Frame::Goodbye).await?;
            break;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    data_task.abort();
    let _ = data_task.await;
    Ok(())
}

async fn run_quic(
    state: NodeState,
    server: SocketAddr,
    server_name: &str,
    ca_cert: &PathBuf,
    token: &str,
    service: Option<ServiceDefinition>,
    once: bool,
) -> Result<()> {
    let roots = load_roots(ca_cert)?;
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"ongrok/1".to_vec()];
    let config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(server, server_name)?
        .await
        .context("QUIC connection failed")?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("failed to open QUIC control stream")?;
    write_control_frame(
        &mut send,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    match read_control_frame(&mut recv).await? {
        Frame::Hello { version } if version == PROTOCOL_VERSION => {}
        Frame::Error { message } => anyhow::bail!("server rejected protocol: {message}"),
        _ => anyhow::bail!("server sent an invalid Hello response"),
    }
    write_control_frame(
        &mut send,
        &Frame::Auth {
            token: token.into(),
            node_id: state.node_id,
        },
    )
    .await?;
    match read_control_frame(&mut recv).await? {
        Frame::AuthAccepted { .. } => {}
        Frame::AuthRejected => anyhow::bail!("server rejected token"),
        _ => anyhow::bail!("server sent an invalid auth response"),
    }
    write_control_frame(
        &mut send,
        &Frame::RegisterNode {
            metadata: local_node_metadata(),
            public_key: node_public_key(&state),
        },
    )
    .await?;
    let local_services = Arc::new(Mutex::new(BTreeMap::new()));
    if let Some(service) = service {
        let local_address = service.local_address.clone();
        write_control_frame(&mut send, &Frame::RegisterService { service }).await?;
        match read_control_frame(&mut recv).await? {
            Frame::RegisterServiceAccepted { service } => {
                local_services
                    .lock()
                    .await
                    .insert(service.service_id, local_address);
                println!(
                    "published service={} endpoint={}{}",
                    service.service_name,
                    service.public_host.as_deref().unwrap_or("unassigned"),
                    service
                        .public_port
                        .map(|port| format!(":{port}"))
                        .unwrap_or_default()
                );
            }
            Frame::Error { message } => anyhow::bail!("server rejected service: {message}"),
            _ => anyhow::bail!("server sent an invalid service registration response"),
        }
    }
    let data_task = tokio::spawn(accept_data_streams(connection.clone(), local_services));
    let mut sequence = 0;
    let mut system = System::new_all();
    let mut networks = Networks::new_with_refreshed_list();
    loop {
        sequence += 1;
        write_control_frame(
            &mut send,
            &Frame::Heartbeat {
                snapshot: heartbeat_snapshot(sequence, &mut system, &mut networks),
            },
        )
        .await?;
        match read_control_frame(&mut recv).await? {
            Frame::HeartbeatAck { sequence: ack, .. } if ack == sequence => {}
            _ => anyhow::bail!("server sent an invalid heartbeat acknowledgement"),
        }
        println!(
            "connected transport=quic node_id={} heartbeat={sequence}",
            state.node_id.0
        );
        if once {
            write_control_frame(&mut send, &Frame::Goodbye).await?;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
    data_task.abort();
    let _ = data_task.await;
    endpoint.close(0_u32.into(), b"client shutdown");
    Ok(())
}

async fn accept_data_streams(
    connection: quinn::Connection,
    local_services: Arc<Mutex<BTreeMap<ServiceId, String>>>,
) {
    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                let local_services = Arc::clone(&local_services);
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_data_stream(Box::new(QuicIo { send, recv }), local_services).await
                    {
                        tracing::warn!(%error, "relay data stream failed");
                    }
                });
            }
            Err(error) => {
                tracing::debug!(%error, "relay data stream accept loop stopped");
                break;
            }
        }
    }
}

async fn accept_yamux_data_streams(
    session: YamuxSession,
    local_services: Arc<Mutex<BTreeMap<ServiceId, String>>>,
) {
    while let Some(stream) = session.next_inbound().await {
        let local_services = Arc::clone(&local_services);
        tokio::spawn(async move {
            if let Err(error) = handle_data_stream(Box::new(stream), local_services).await {
                tracing::warn!(%error, "Yamux relay data stream failed");
            }
        });
    }
}

async fn handle_data_stream(
    mut stream: BoxedIo,
    local_services: Arc<Mutex<BTreeMap<ServiceId, String>>>,
) -> Result<()> {
    let (tunnel_id, service_id) = match read_control_frame(&mut stream).await? {
        Frame::OpenStream {
            tunnel_id,
            service_id,
        } => (tunnel_id, service_id),
        _ => anyhow::bail!("expected OpenStream as first data stream frame"),
    };
    let local_address = local_services
        .lock()
        .await
        .get(&service_id)
        .cloned()
        .context("server requested an unknown service")?;
    let local = match timeout(Duration::from_secs(10), TcpStream::connect(&local_address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            write_control_frame(
                &mut stream,
                &Frame::Error {
                    message: format!("failed to connect to {local_address}: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
        Err(_) => {
            write_control_frame(
                &mut stream,
                &Frame::Error {
                    message: format!("timed out connecting to {local_address}"),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_control_frame(&mut stream, &Frame::OpenStreamAck { tunnel_id }).await?;
    let (mut recv, mut send) = tokio::io::split(stream);
    let (mut local_read, mut local_write) = local.into_split();
    let relay_to_local = async {
        let copied = copy(&mut recv, &mut local_write).await?;
        local_write.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let local_to_relay = async {
        let copied = copy(&mut local_read, &mut send).await?;
        send.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let (from_relay, from_local) =
        tokio::try_join!(relay_to_local, local_to_relay).context("local TCP relay copy failed")?;
    tracing::debug!(
        service_id = %service_id.0,
        tunnel_id = %tunnel_id.0,
        bytes_from_relay = from_relay,
        bytes_from_local = from_local,
        "relay data stream completed"
    );
    Ok(())
}

fn load_roots(path: &PathBuf) -> Result<RootCertStore> {
    let file = File::open(path)
        .with_context(|| format!("failed to open CA certificate {}", path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<_, _>>()
        .context("failed to parse CA certificate PEM")?;
    if certs.is_empty() {
        anyhow::bail!("CA certificate file is empty");
    }
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).context("invalid CA certificate")?;
    }
    Ok(roots)
}

fn local_node_metadata() -> NodeMetadata {
    NodeMetadata {
        hostname: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        metadata: Metadata::default(),
    }
}

async fn read_control_frame<R>(recv: &mut R) -> Result<Frame>
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

async fn write_control_frame<W>(send: &mut W, frame: &Frame) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let encoded = encode_frame(frame)?;
    send.write_all(&encoded)
        .await
        .context("failed to write control frame")?;
    Ok(())
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

fn heartbeat_snapshot(
    sequence: u64,
    system: &mut System,
    networks: &mut Networks,
) -> HeartbeatSnapshot {
    system.refresh_cpu_usage();
    system.refresh_memory();
    networks.refresh(false);
    let total_memory = system.total_memory();
    let memory_percent = (total_memory > 0)
        .then(|| (system.used_memory() as f64 / total_memory as f64 * 100.0) as f32);
    let load_average = System::load_average().one as f32;
    HeartbeatSnapshot {
        sequence,
        sent_at_unix_ms: now_unix_ms(),
        cpu_percent: Some(system.global_cpu_usage()),
        memory_percent,
        load_average: Some(load_average),
        network_rx_bytes: Some(
            networks
                .values()
                .map(|network| network.total_received())
                .sum(),
        ),
        network_tx_bytes: Some(
            networks
                .values()
                .map(|network| network.total_transmitted())
                .sum(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_is_stable_after_first_creation() {
        let directory =
            std::env::temp_dir().join(format!("ongrok-client-test-{}", NodeId::new().0));
        let path = directory.join("node.json");
        let first = load_or_create_state(&path).unwrap();
        let second = load_or_create_state(&path).unwrap();
        assert_eq!(first.node_id.0, second.node_id.0);
        assert_eq!(first.private_key, second.private_key);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_state_is_migrated_without_changing_its_node_id() {
        let directory =
            std::env::temp_dir().join(format!("ongrok-client-test-{}", NodeId::new().0));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("node.json");
        let node_id = NodeId::new();
        fs::write(&path, format!(r#"{{"node_id":"{}"}}"#, node_id.0)).unwrap();
        let state = load_or_create_state(&path).unwrap();
        assert_eq!(state.node_id.0, node_id.0);
        assert_ne!(state.private_key, [0; 32]);
        assert_eq!(
            node_public_key(&state),
            node_public_key(&load_or_create_state(&path).unwrap())
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
