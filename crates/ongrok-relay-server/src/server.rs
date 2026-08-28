use crate::{
    api_models::{
        AuthResponse, ErrorResponse, HealthResponse, ServiceCreateRequest, ServiceView,
        TokenMutationRequest, TokenRevocationResponse, TokenRotationResponse,
    },
    config::{Cli, Command, RunOptions, TokenCommand, validate_tls_material},
    ingress::{run_http_ingress, run_https_ingress},
    relay::{ensure_tcp_ingress, first_available_tcp_port},
    state::{AppState, ClientSession, activate_session, validate_node_identity},
    store::ServiceStore,
    transport::{run_quic, run_tcp_tls},
    wire::{now_unix_ms, read_control_frame, write_control_frame},
};
use anyhow::{Context, Result};
use bytes::Bytes;
use clap::Parser;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};
use hyper_util::rt::TokioIo;
use libongrok::{
    Frame, NodeMetric, NodeRecord, NodeStatus, PROTOCOL_VERSION, QuicIo, ServiceDefinition,
    ServiceId, ServiceStatus, TokenKind, TransportKind, TunnelId, YamuxIo, YamuxSession,
    generate_token, hash_token, validate_service_name,
};
use mimalloc::MiMalloc;
use redb::Database;
use rustls::ServerConfig;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    signal,
    sync::Mutex,
    task::JoinSet,
    time::timeout,
};
use tracing::{error, info, warn};

#[cfg(any())]
const SERVICES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("services");
#[cfg(any())]
const NODES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
#[cfg(any())]
const METRICS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metrics");
#[cfg(any())]
const TOKENS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("tokens");
#[cfg(any())]
const TOKEN_STATE_KEY: &str = "state";
const MAX_API_BODY_BYTES: usize = 16 * 1024;

#[cfg(any())]
struct ServiceStore {
    db: Database,
}

#[cfg(any())]
impl ServiceStore {
    fn open(path: &PathBuf) -> Result<Self> {
        let db = Database::create(path)
            .with_context(|| format!("failed to open database {}", path.display()))?;
        let write_txn = db
            .begin_write()
            .context("failed to begin database transaction")?;
        write_txn
            .open_table(SERVICES_TABLE)
            .context("failed to open services table")?;
        write_txn
            .open_table(NODES_TABLE)
            .context("failed to open nodes table")?;
        write_txn
            .open_table(METRICS_TABLE)
            .context("failed to open metrics table")?;
        write_txn
            .open_table(TOKENS_TABLE)
            .context("failed to open tokens table")?;
        write_txn
            .commit()
            .context("failed to initialize database")?;
        Ok(Self { db })
    }

    fn load_or_initialize_tokens(
        &self,
        initial_admin_hash: [u8; 32],
        initial_user_hash: [u8; 32],
    ) -> Result<TokenState> {
        let read_txn = self
            .db
            .begin_read()
            .context("failed to begin token read transaction")?;
        let table = read_txn
            .open_table(TOKENS_TABLE)
            .context("failed to open token table")?;
        if let Some(value) = table
            .get(TOKEN_STATE_KEY)
            .context("failed to read token state")?
        {
            return postcard::from_bytes(value.value()).context("failed to decode token state");
        }
        drop(table);
        drop(read_txn);
        let tokens = TokenState {
            admin_hash: Some(initial_admin_hash),
            user_hash: Some(initial_user_hash),
        };
        self.put_tokens(&tokens)?;
        Ok(tokens)
    }

    fn put_tokens(&self, tokens: &TokenState) -> Result<()> {
        let encoded = postcard::to_allocvec(tokens).context("failed to encode token state")?;
        let write_txn = self
            .db
            .begin_write()
            .context("failed to begin token write transaction")?;
        {
            let mut table = write_txn
                .open_table(TOKENS_TABLE)
                .context("failed to open token table")?;
            table
                .insert(TOKEN_STATE_KEY, encoded.as_slice())
                .context("failed to persist token state")?;
        }
        write_txn.commit().context("failed to commit token state")?;
        Ok(())
    }

    fn load_services(&self) -> Result<BTreeMap<ServiceId, ServiceDefinition>> {
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(SERVICES_TABLE)
            .context("failed to open services table")?;
        let mut services = BTreeMap::new();
        for item in table.iter().context("failed to iterate services")? {
            let (_key, value) = item.context("failed to read service record")?;
            let service: ServiceDefinition =
                postcard::from_bytes(value.value()).context("failed to decode service record")?;
            services.insert(service.service_id, service);
        }
        Ok(services)
    }

    fn put(&self, service: &ServiceDefinition) -> Result<()> {
        let encoded = postcard::to_allocvec(service).context("failed to encode service record")?;
        let key = service.service_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(SERVICES_TABLE)
                .context("failed to open services table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist service")?;
        }
        txn.commit().context("failed to commit service")?;
        Ok(())
    }

    fn delete(&self, service_id: ServiceId) -> Result<()> {
        let key = service_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(SERVICES_TABLE)
                .context("failed to open services table")?;
            table
                .remove(key.as_str())
                .context("failed to remove service")?;
        }
        txn.commit().context("failed to commit service removal")?;
        Ok(())
    }

    fn load_nodes(&self) -> Result<BTreeMap<libongrok::NodeId, NodeRecord>> {
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(NODES_TABLE)
            .context("failed to open nodes table")?;
        let mut nodes = BTreeMap::new();
        for item in table.iter().context("failed to iterate nodes")? {
            let (_, value) = item.context("failed to read node record")?;
            let node: NodeRecord =
                postcard::from_bytes(value.value()).context("failed to decode node record")?;
            nodes.insert(node.node_id, node);
        }
        Ok(nodes)
    }

    fn put_node(&self, node: &NodeRecord) -> Result<()> {
        let encoded = postcard::to_allocvec(node).context("failed to encode node record")?;
        let key = node.node_id.0.to_string();
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(NODES_TABLE)
                .context("failed to open nodes table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist node")?;
        }
        txn.commit().context("failed to commit node")?;
        Ok(())
    }

    fn put_metric(&self, metric: &NodeMetric) -> Result<()> {
        let encoded = postcard::to_allocvec(metric).context("failed to encode node metric")?;
        let key = format!(
            "{}:{:020}:{:020}",
            metric.node_id.0, metric.recorded_at_unix_ms, metric.snapshot.sequence
        );
        let txn = self
            .db
            .begin_write()
            .context("failed to begin database write")?;
        {
            let mut table = txn
                .open_table(METRICS_TABLE)
                .context("failed to open metrics table")?;
            table
                .insert(key.as_str(), encoded.as_slice())
                .context("failed to persist node metric")?;
        }
        txn.commit().context("failed to commit node metric")?;
        Ok(())
    }

    fn metrics_for_node(&self, node_id: libongrok::NodeId) -> Result<Vec<NodeMetric>> {
        let prefix = format!("{}:", node_id.0);
        let txn = self
            .db
            .begin_read()
            .context("failed to begin database read")?;
        let table = txn
            .open_table(METRICS_TABLE)
            .context("failed to open metrics table")?;
        let mut metrics = Vec::new();
        for item in table.iter().context("failed to iterate metrics")? {
            let (key, value) = item.context("failed to read metric record")?;
            if key.value().starts_with(&prefix) {
                metrics.push(
                    postcard::from_bytes(value.value()).context("failed to decode node metric")?,
                );
            }
        }
        Ok(metrics)
    }
}

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

type ApiBody = Full<Bytes>;
pub(crate) trait RelayIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> RelayIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}
pub(crate) type BoxedIo = Box<dyn RelayIo>;

#[cfg(any())]
#[derive(Clone)]
struct AppState {
    tokens: Arc<RwLock<TokenState>>,
    services: Arc<Mutex<BTreeMap<ServiceId, ServiceDefinition>>>,
    nodes: Arc<Mutex<BTreeMap<libongrok::NodeId, NodeRecord>>>,
    sessions: Arc<Mutex<BTreeMap<libongrok::NodeId, ActiveSession>>>,
    tcp_ingress_tasks: Arc<Mutex<BTreeMap<ServiceId, JoinHandle<()>>>>,
    public_host: Arc<str>,
    http_domain: Option<Arc<str>>,
    tcp_port_start: u16,
    tcp_port_end: u16,
    store: Arc<ServiceStore>,
}

#[cfg(any())]
#[derive(Clone, Deserialize, Serialize)]
struct TokenState {
    admin_hash: Option<[u8; 32]>,
    user_hash: Option<[u8; 32]>,
}

#[cfg(any())]
impl TokenState {
    fn authenticate(&self, token_hash: [u8; 32]) -> Option<TokenKind> {
        if self.admin_hash == Some(token_hash) {
            Some(TokenKind::Admin)
        } else if self.user_hash == Some(token_hash) {
            Some(TokenKind::User)
        } else {
            None
        }
    }

    fn set(&mut self, kind: TokenKind, token_hash: Option<[u8; 32]>) {
        match kind {
            TokenKind::Admin => self.admin_hash = token_hash,
            TokenKind::User => self.user_hash = token_hash,
        }
    }
}

#[cfg(any())]
#[derive(Clone)]
struct ActiveSession {
    id: TunnelId,
    token_kind: TokenKind,
    connection: ClientSession,
}

#[cfg(any())]
#[derive(Clone)]
enum ClientSession {
    Quic(quinn::Connection),
    Yamux(YamuxSession),
}

#[cfg(any())]
impl ClientSession {
    fn close(&self) {
        match self {
            Self::Quic(connection) => connection.close(0_u32.into(), b"token revoked"),
            Self::Yamux(session) => session.close(),
        }
    }
}

#[cfg(any())]
async fn activate_session(
    node_id: libongrok::NodeId,
    session_id: TunnelId,
    token_kind: TokenKind,
    connection: ClientSession,
    state: &AppState,
) -> Result<()> {
    let replaced = state.sessions.lock().await.insert(
        node_id,
        ActiveSession {
            id: session_id,
            token_kind,
            connection,
        },
    );
    if let Some(previous) = replaced {
        previous.connection.close();
    }
    Ok(())
}

#[cfg(any())]
async fn validate_node_identity(
    node_id: libongrok::NodeId,
    public_key: [u8; 32],
    state: &AppState,
) -> Result<()> {
    let nodes = state.nodes.lock().await;
    if let Some(existing) = nodes.get(&node_id)
        && let Some(existing_key) = existing.public_key
        && existing_key != public_key
    {
        anyhow::bail!("node public key does not match persisted identity");
    }
    Ok(())
}

pub(crate) async fn run_cli() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Init { db_path } => {
            ServiceStore::open(&db_path)?;
            let (admin, _) = generate_token(TokenKind::Admin);
            let (user, _) = generate_token(TokenKind::User);
            println!("database={}", db_path.display());
            println!("admin_token={admin}");
            println!("user_token={user}");
            Ok(())
        }
        Command::Doctor {
            tls_cert,
            tls_key,
            db_path,
        } => {
            validate_tls_material(&tls_cert, &tls_key)?;
            if !db_path.exists() {
                anyhow::bail!("database does not exist: {}", db_path.display());
            }
            let database = Database::open(&db_path)
                .with_context(|| format!("failed to open database {}", db_path.display()))?;
            drop(database);
            println!("tls=ok");
            println!("database=ok");
            Ok(())
        }
        Command::Token {
            command: TokenCommand::Create { kind },
        } => {
            let (token, _) = generate_token(kind.into());
            println!("{token}");
            Ok(())
        }
        Command::Run { options } => {
            let RunOptions {
                tls_cert,
                tls_key,
                api_listen,
                quic_listen,
                tcp_tls_listen,
                http_listen,
                https_listen,
                http_domain,
                public_host,
                tcp_port_start,
                tcp_port_end,
                admin_token,
                user_token,
                db_path,
            } = *options;
            if tcp_port_start > tcp_port_end {
                anyhow::bail!("tcp port start must not exceed tcp port end");
            }
            if (http_listen.is_some() || https_listen.is_some()) != http_domain.is_some() {
                anyhow::bail!(
                    "--http-domain is required when --http-listen or --https-listen is configured"
                );
            }
            let tls = validate_tls_material(&tls_cert, &tls_key)?;
            let store = Arc::new(ServiceStore::open(&db_path)?);
            let services = store.load_services()?;
            let nodes = store.load_nodes()?;
            let tokens = store
                .load_or_initialize_tokens(hash_token(&admin_token), hash_token(&user_token))?;
            run(
                api_listen,
                quic_listen,
                tcp_tls_listen,
                http_listen,
                https_listen,
                tls,
                AppState {
                    tokens: Arc::new(RwLock::new(tokens)),
                    services: Arc::new(Mutex::new(services)),
                    nodes: Arc::new(Mutex::new(nodes)),
                    sessions: Arc::new(Mutex::new(BTreeMap::new())),
                    tcp_ingress_tasks: Arc::new(Mutex::new(BTreeMap::new())),
                    public_host: Arc::from(public_host),
                    http_domain: http_domain.map(Arc::from),
                    tcp_port_start,
                    tcp_port_end,
                    store,
                },
            )
            .await
        }
    }
}

async fn run(
    address: SocketAddr,
    quic_address: SocketAddr,
    tcp_tls_address: SocketAddr,
    http_address: Option<SocketAddr>,
    https_address: Option<SocketAddr>,
    tls: Arc<ServerConfig>,
    state: AppState,
) -> Result<()> {
    // TCP leases survive a server restart, so their public listeners must be
    // restored before accepting new control-plane or ingress connections.
    let persisted_tcp_services = state
        .services
        .lock()
        .await
        .values()
        .filter(|service| service.protocol == libongrok::Protocol::Tcp)
        .cloned()
        .collect::<Vec<_>>();
    for service in persisted_tcp_services {
        ensure_tcp_ingress(state.clone(), &service)
            .await
            .with_context(|| {
                format!(
                    "failed to restore TCP ingress for persisted service {}",
                    service.service_id.0
                )
            })?;
    }
    let quic_task = tokio::spawn(run_quic(quic_address, Arc::clone(&tls), state.clone()));
    let tcp_tls_task = tokio::spawn(run_tcp_tls(
        tcp_tls_address,
        Arc::clone(&tls),
        state.clone(),
    ));
    let http_task =
        http_address.map(|address| tokio::spawn(run_http_ingress(address, state.clone())));
    let https_task = https_address
        .map(|address| tokio::spawn(run_https_ingress(address, Arc::clone(&tls), state.clone())));
    let result = run_api(address, state).await;
    quic_task.abort();
    tcp_tls_task.abort();
    if let Some(http_task) = &http_task {
        http_task.abort();
    }
    if let Some(https_task) = &https_task {
        https_task.abort();
    }
    match quic_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => warn!(%error, "QUIC listener stopped"),
    }
    match tcp_tls_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => warn!(%error, "TCP/TLS listener stopped"),
    }
    if let Some(http_task) = http_task {
        match http_task.await {
            Ok(Ok(())) | Err(_) => {}
            Ok(Err(error)) => warn!(%error, "HTTP ingress listener stopped"),
        }
    }
    if let Some(https_task) = https_task {
        match https_task.await {
            Ok(Ok(())) | Err(_) => {}
            Ok(Err(error)) => warn!(%error, "HTTPS ingress listener stopped"),
        }
    }
    result
}

async fn run_api(address: SocketAddr, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind control API at {address}"))?;
    info!(%address, "control API listening");
    let state = Arc::new(state);
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer) = result.context("control API accept failed")?;
                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    let service = service_fn(move |request| api_handler(request, Arc::clone(&state)));
                    if let Err(error) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                        warn!(%peer, %error, "control API connection failed");
                    }
                });
            }
            _ = signal::ctrl_c() => {
                info!("control API received shutdown signal");
                break;
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

pub(crate) async fn handle_yamux_session(
    mut control: YamuxIo,
    session: YamuxSession,
    remote: SocketAddr,
    state: AppState,
) -> Result<()> {
    match read_control_frame(&mut control).await? {
        Frame::Hello { version } if version == PROTOCOL_VERSION => {
            write_control_frame(
                &mut control,
                &Frame::Hello {
                    version: PROTOCOL_VERSION,
                },
            )
            .await?;
        }
        Frame::Hello { .. } => {
            write_control_frame(
                &mut control,
                &Frame::Error {
                    message: "unsupported protocol version".into(),
                },
            )
            .await?;
            anyhow::bail!("unsupported protocol version");
        }
        _ => anyhow::bail!("expected Hello as first control frame"),
    }
    let (kind, node_id) = match read_control_frame(&mut control).await? {
        Frame::Auth { token, node_id } => match authenticate_token(&token, &state) {
            Some(kind) => (kind, node_id),
            None => {
                write_control_frame(&mut control, &Frame::AuthRejected).await?;
                anyhow::bail!("client authentication rejected");
            }
        },
        _ => anyhow::bail!("expected Auth after Hello"),
    };
    write_control_frame(&mut control, &Frame::AuthAccepted { kind }).await?;
    let session_id = TunnelId::new();
    let client_session = ClientSession::Yamux(session);
    let mut registered_node = false;
    let result = async {
        loop {
            match read_control_frame(&mut control).await? {
                Frame::RegisterNode {
                    metadata,
                    public_key,
                } => {
                    metadata.metadata.validate()?;
                    validate_node_identity(node_id, public_key, &state).await?;
                    activate_session(node_id, session_id, kind, client_session.clone(), &state)
                        .await?;
                    let node = NodeRecord {
                        node_id,
                        public_key: Some(public_key),
                        metadata,
                        public_ip: remote.ip().to_string(),
                        source_port: remote.port(),
                        transport: TransportKind::TcpTlsYamux,
                        status: NodeStatus::Online,
                        connected_at_unix_ms: now_unix_ms(),
                        last_heartbeat_at_unix_ms: None,
                        rtt_ms: None,
                    };
                    state.nodes.lock().await.insert(node_id, node.clone());
                    state.store.put_node(&node)?;
                    registered_node = true;
                    info!(%remote, ?kind, "TCP/TLS Yamux node registered");
                }
                Frame::RegisterService { service } => {
                    let response = if !registered_node || service.node_id != node_id {
                        Err(anyhow::anyhow!(
                            "node must be registered before publishing services"
                        ))
                    } else {
                        register_service(service, node_id, &state).await
                    };
                    match response {
                        Ok(service) => {
                            write_control_frame(
                                &mut control,
                                &Frame::RegisterServiceAccepted { service },
                            )
                            .await?;
                        }
                        Err(error) => {
                            write_control_frame(
                                &mut control,
                                &Frame::Error {
                                    message: error.to_string(),
                                },
                            )
                            .await?;
                        }
                    }
                }
                Frame::UnregisterService { service_id } => {
                    unregister_service(service_id, node_id, &state).await?;
                }
                Frame::Heartbeat { snapshot } => {
                    let sequence = snapshot.sequence;
                    record_heartbeat(node_id, snapshot, &state).await?;
                    write_control_frame(
                        &mut control,
                        &Frame::HeartbeatAck {
                            sequence,
                            server_time_unix_ms: now_unix_ms(),
                        },
                    )
                    .await?;
                }
                Frame::Goodbye => break,
                _ => {
                    write_control_frame(
                        &mut control,
                        &Frame::Error {
                            message: "unsupported control frame".into(),
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }
    .await;
    let active = state
        .sessions
        .lock()
        .await
        .get(&node_id)
        .is_some_and(|current| current.id == session_id);
    if active {
        state.sessions.lock().await.remove(&node_id);
        if let Some(node) = state.nodes.lock().await.get_mut(&node_id).map(|node| {
            node.status = NodeStatus::Offline;
            node.clone()
        }) {
            state.store.put_node(&node)?;
        }
    }
    result
}

async fn register_service(
    mut service: ServiceDefinition,
    node_id: libongrok::NodeId,
    state: &AppState,
) -> Result<ServiceDefinition> {
    validate_service_name(&service.service_name)?;
    service.metadata.validate()?;
    let mut services = state.services.lock().await;
    if services.values().any(|existing| {
        existing.service_name == service.service_name && existing.node_id != node_id
    }) {
        anyhow::bail!("service name is already in use");
    }
    if let Some(previous) = services.values().find(|existing| {
        existing.node_id == node_id && existing.service_name == service.service_name
    }) {
        service.service_id = previous.service_id;
        if previous.protocol == libongrok::Protocol::Tcp
            && service.protocol == libongrok::Protocol::Tcp
        {
            service.public_port = previous.public_port;
        }
    }
    match service.protocol {
        libongrok::Protocol::Tcp => {
            let port = match service.public_port {
                Some(port) => port,
                None => first_available_tcp_port(&services, state)
                    .context("no TCP relay ports are available")?,
            };
            if port < state.tcp_port_start || port > state.tcp_port_end {
                anyhow::bail!("TCP port {port} is outside the configured relay range");
            }
            if services.values().any(|existing| {
                existing.service_id != service.service_id
                    && existing.protocol == libongrok::Protocol::Tcp
                    && existing.public_port == Some(port)
            }) {
                anyhow::bail!("TCP port {port} is already leased");
            }
            service.public_port = Some(port);
            service.public_host = Some(state.public_host.to_string());
        }
        libongrok::Protocol::Http | libongrok::Protocol::Https => {
            let domain = state
                .http_domain
                .as_deref()
                .context("HTTP/HTTPS ingress is not configured on this relay")?;
            service.public_host = Some(format!("{}.{}", service.service_name, domain));
            service.public_port = None;
        }
    }
    let service_id = service.service_id;
    services.insert(service_id, service.clone());
    drop(services);
    state.store.put(&service)?;
    if service.protocol == libongrok::Protocol::Tcp
        && let Err(error) = ensure_tcp_ingress(state.clone(), &service).await
    {
        state.services.lock().await.remove(&service_id);
        state.store.delete(service_id)?;
        return Err(error).context("failed to activate TCP relay port");
    }
    Ok(service)
}

async fn unregister_service(
    service_id: ServiceId,
    node_id: libongrok::NodeId,
    state: &AppState,
) -> Result<()> {
    let mut services = state.services.lock().await;
    if services
        .get(&service_id)
        .is_some_and(|service| service.node_id == node_id)
    {
        services.remove(&service_id);
        drop(services);
        state.store.delete(service_id)?;
        if let Some(task) = state.tcp_ingress_tasks.lock().await.remove(&service_id) {
            task.abort();
        }
    }
    Ok(())
}

async fn record_heartbeat(
    node_id: libongrok::NodeId,
    snapshot: libongrok::HeartbeatSnapshot,
    state: &AppState,
) -> Result<()> {
    let recorded_at_unix_ms = now_unix_ms();
    let rtt_ms = recorded_at_unix_ms
        .saturating_sub(snapshot.sent_at_unix_ms)
        .clamp(0, 3_600_000) as u32;
    let updated_node = {
        let mut nodes = state.nodes.lock().await;
        nodes.get_mut(&node_id).map(|node| {
            node.status = NodeStatus::Online;
            node.last_heartbeat_at_unix_ms = Some(recorded_at_unix_ms);
            node.rtt_ms = Some(rtt_ms);
            node.clone()
        })
    };
    if let Some(node) = updated_node {
        state.store.put_node(&node)?;
        state.store.put_metric(&NodeMetric {
            node_id,
            recorded_at_unix_ms,
            rtt_ms: Some(rtt_ms),
            snapshot,
        })?;
    }
    Ok(())
}

pub(crate) async fn handle_quic_connection(
    incoming: quinn::Incoming,
    state: AppState,
) -> Result<()> {
    let connection = incoming.await.context("QUIC handshake failed")?;
    let remote = connection.remote_address();
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("client did not open a control stream")?;
    match read_control_frame(&mut recv).await? {
        Frame::Hello { version } if version == PROTOCOL_VERSION => {
            write_control_frame(
                &mut send,
                &Frame::Hello {
                    version: PROTOCOL_VERSION,
                },
            )
            .await?
        }
        Frame::Hello { .. } => {
            write_control_frame(
                &mut send,
                &Frame::Error {
                    message: "unsupported protocol version".into(),
                },
            )
            .await?;
            anyhow::bail!("unsupported protocol version")
        }
        _ => anyhow::bail!("expected Hello as first control frame"),
    }
    let (kind, node_id) = match read_control_frame(&mut recv).await? {
        Frame::Auth { token, node_id } => match authenticate_token(&token, &state) {
            Some(kind) => (kind, node_id),
            None => {
                write_control_frame(&mut send, &Frame::AuthRejected).await?;
                anyhow::bail!("client authentication rejected")
            }
        },
        _ => anyhow::bail!("expected Auth after Hello"),
    };
    write_control_frame(&mut send, &Frame::AuthAccepted { kind }).await?;
    let session_id = TunnelId::new();
    let client_session = ClientSession::Quic(connection.clone());
    let mut registered_node = false;
    loop {
        match read_control_frame(&mut recv).await? {
            Frame::RegisterNode {
                metadata,
                public_key,
            } => {
                metadata.metadata.validate()?;
                validate_node_identity(node_id, public_key, &state).await?;
                activate_session(node_id, session_id, kind, client_session.clone(), &state).await?;
                let node = NodeRecord {
                    node_id,
                    public_key: Some(public_key),
                    metadata,
                    public_ip: remote.ip().to_string(),
                    source_port: remote.port(),
                    transport: TransportKind::Quic,
                    status: NodeStatus::Online,
                    connected_at_unix_ms: now_unix_ms(),
                    last_heartbeat_at_unix_ms: None,
                    rtt_ms: None,
                };
                state.nodes.lock().await.insert(node_id, node.clone());
                state.store.put_node(&node)?;
                registered_node = true;
                info!(%remote, ?kind, "QUIC node registered");
            }
            Frame::RegisterService { service } => {
                let response = if !registered_node || service.node_id != node_id {
                    Err(anyhow::anyhow!(
                        "node must be registered before publishing services"
                    ))
                } else {
                    register_service(service, node_id, &state).await
                };
                match response {
                    Ok(service) => {
                        write_control_frame(&mut send, &Frame::RegisterServiceAccepted { service })
                            .await?;
                    }
                    Err(error) => {
                        write_control_frame(
                            &mut send,
                            &Frame::Error {
                                message: error.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            Frame::UnregisterService { service_id } => {
                let mut services = state.services.lock().await;
                if services
                    .get(&service_id)
                    .is_some_and(|service| service.node_id == node_id)
                {
                    services.remove(&service_id);
                    drop(services);
                    state.store.delete(service_id)?;
                    if let Some(task) = state.tcp_ingress_tasks.lock().await.remove(&service_id) {
                        task.abort();
                    }
                }
            }
            Frame::Heartbeat { snapshot } => {
                let recorded_at_unix_ms = now_unix_ms();
                let rtt_ms = recorded_at_unix_ms
                    .saturating_sub(snapshot.sent_at_unix_ms)
                    .clamp(0, 3_600_000) as u32;
                let updated_node = {
                    let mut nodes = state.nodes.lock().await;
                    nodes.get_mut(&node_id).map(|node| {
                        node.status = NodeStatus::Online;
                        node.last_heartbeat_at_unix_ms = Some(recorded_at_unix_ms);
                        node.rtt_ms = Some(rtt_ms);
                        node.clone()
                    })
                };
                if let Some(node) = updated_node {
                    state.store.put_node(&node)?;
                    state.store.put_metric(&NodeMetric {
                        node_id,
                        recorded_at_unix_ms,
                        rtt_ms: Some(rtt_ms),
                        snapshot: snapshot.clone(),
                    })?;
                }
                write_control_frame(
                    &mut send,
                    &Frame::HeartbeatAck {
                        sequence: snapshot.sequence,
                        server_time_unix_ms: now_unix_ms(),
                    },
                )
                .await?
            }
            Frame::Goodbye => break,
            _ => {
                write_control_frame(
                    &mut send,
                    &Frame::Error {
                        message: "unsupported control frame".into(),
                    },
                )
                .await?
            }
        }
    }
    let active = state
        .sessions
        .lock()
        .await
        .get(&node_id)
        .is_some_and(|current| current.id == session_id);
    if active {
        state.sessions.lock().await.remove(&node_id);
        let offline_node = {
            let mut nodes = state.nodes.lock().await;
            nodes.get_mut(&node_id).map(|node| {
                node.status = NodeStatus::Offline;
                node.clone()
            })
        };
        if let Some(node) = offline_node {
            state.store.put_node(&node)?;
        }
    }
    Ok(())
}

pub(crate) async fn open_client_stream(
    state: &AppState,
    service_id: ServiceId,
) -> Result<(BoxedIo, TunnelId)> {
    let service = state
        .services
        .lock()
        .await
        .get(&service_id)
        .cloned()
        .context("service is no longer registered")?;
    let session = state
        .sessions
        .lock()
        .await
        .get(&service.node_id)
        .cloned()
        .context("service node is offline")?;
    let tunnel_id = TunnelId::new();
    let mut stream: BoxedIo = match session.connection {
        ClientSession::Quic(connection) => {
            let (send, recv) = timeout(Duration::from_secs(10), connection.open_bi())
                .await
                .context("timed out while opening QUIC client data stream")?
                .context("failed to open QUIC client data stream")?;
            Box::new(QuicIo { send, recv })
        }
        ClientSession::Yamux(connection) => Box::new(
            timeout(Duration::from_secs(10), connection.open_stream())
                .await
                .context("timed out while opening Yamux client data stream")?
                .context("failed to open Yamux client data stream")?,
        ),
    };
    write_control_frame(
        &mut stream,
        &Frame::OpenStream {
            tunnel_id,
            service_id,
        },
    )
    .await?;
    match timeout(Duration::from_secs(10), read_control_frame(&mut stream))
        .await
        .context("timed out waiting for local target")??
    {
        Frame::OpenStreamAck { tunnel_id: ack } if ack == tunnel_id => Ok((stream, tunnel_id)),
        Frame::Error { message } => anyhow::bail!("client rejected local target: {message}"),
        _ => anyhow::bail!("client returned an invalid data stream acknowledgement"),
    }
}

#[cfg(any())]
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

#[cfg(any())]
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

#[cfg(any())]
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

async fn api_handler(
    request: Request<Incoming>,
    state: Arc<AppState>,
) -> Result<Response<ApiBody>, Infallible> {
    let path = request.uri().path().to_owned();
    if request.method() == Method::POST
        && (path == "/v1/admin/tokens/rotate" || path == "/v1/admin/tokens/revoke")
    {
        if authenticate(request.headers(), &state) != Some(TokenKind::Admin) {
            return Ok(json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "admin bearer token required",
                },
            ));
        }
        let mutation = match token_mutation_request(request).await {
            Ok(mutation) => mutation,
            Err(error) => {
                warn!(%error, "invalid token mutation request");
                return Ok(json(
                    StatusCode::BAD_REQUEST,
                    &ErrorResponse {
                        error: "invalid token mutation request",
                    },
                ));
            }
        };
        if path == "/v1/admin/tokens/rotate" {
            let (token, hash) = generate_token(mutation.kind);
            if let Err(error) = replace_token(&state, mutation.kind, Some(hash)).await {
                error!(%error, "failed to rotate token");
                return Ok(json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorResponse {
                        error: "failed to rotate token",
                    },
                ));
            }
            return Ok(json(
                StatusCode::OK,
                &TokenRotationResponse {
                    kind: token_kind_name(mutation.kind),
                    token,
                },
            ));
        }
        if let Err(error) = replace_token(&state, mutation.kind, None).await {
            error!(%error, "failed to revoke token");
            return Ok(json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &ErrorResponse {
                    error: "failed to revoke token",
                },
            ));
        }
        return Ok(json(
            StatusCode::OK,
            &TokenRevocationResponse {
                kind: token_kind_name(mutation.kind),
                revoked: true,
            },
        ));
    }
    if request.method() == Method::GET && path == "/v1/nodes" {
        return Ok(match authenticate(request.headers(), &state) {
            Some(_) => {
                let nodes = state.nodes.lock().await;
                json(StatusCode::OK, &nodes.values().cloned().collect::<Vec<_>>())
            }
            None => json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ),
        });
    }
    if request.method() == Method::GET && path.starts_with("/v1/nodes/") {
        let suffix = &path["/v1/nodes/".len()..];
        let (node_key, metrics) = suffix
            .strip_suffix("/metrics")
            .map_or((suffix, false), |node_key| (node_key, true));
        return Ok(match authenticate(request.headers(), &state) {
            Some(_) => {
                let node = state
                    .nodes
                    .lock()
                    .await
                    .values()
                    .find(|node| node.node_id.0.to_string() == node_key)
                    .cloned();
                match node {
                    Some(node) if metrics => match state.store.metrics_for_node(node.node_id) {
                        Ok(metrics) => json(StatusCode::OK, &metrics),
                        Err(error) => {
                            error!(%error, "failed to load node metrics");
                            json(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                &ErrorResponse {
                                    error: "failed to load node metrics",
                                },
                            )
                        }
                    },
                    Some(node) => json(StatusCode::OK, &node),
                    None => json(
                        StatusCode::NOT_FOUND,
                        &ErrorResponse {
                            error: "node not found",
                        },
                    ),
                }
            }
            None => json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ),
        });
    }
    if request.method() == Method::POST && path == "/v1/services" {
        if authenticate(request.headers(), &state).is_none() {
            return Ok(json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ));
        }
        let bytes = match request.into_body().collect().await {
            Ok(body) => body.to_bytes(),
            Err(error) => {
                warn!(%error, "failed to read service create request");
                return Ok(json(
                    StatusCode::BAD_REQUEST,
                    &ErrorResponse {
                        error: "invalid service request",
                    },
                ));
            }
        };
        if bytes.len() > MAX_API_BODY_BYTES {
            return Ok(json(
                StatusCode::PAYLOAD_TOO_LARGE,
                &ErrorResponse {
                    error: "service request is too large",
                },
            ));
        }
        let request: ServiceCreateRequest = match serde_json::from_slice(&bytes) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "failed to decode service create request");
                return Ok(json(
                    StatusCode::BAD_REQUEST,
                    &ErrorResponse {
                        error: "invalid service request",
                    },
                ));
            }
        };
        let node_status = state
            .nodes
            .lock()
            .await
            .get(&request.node_id)
            .map(|node| node.status);
        let Some(node_status) = node_status else {
            return Ok(json(
                StatusCode::NOT_FOUND,
                &ErrorResponse {
                    error: "node not found",
                },
            ));
        };
        if node_status != NodeStatus::Online {
            return Ok(json(
                StatusCode::CONFLICT,
                &ErrorResponse {
                    error: "node is offline",
                },
            ));
        }
        let node_id = request.node_id;
        return Ok(
            match register_service(request.into_definition(), node_id, &state).await {
                Ok(service) => json(StatusCode::CREATED, &service),
                Err(error) => {
                    warn!(%error, "service create request rejected");
                    json(
                        StatusCode::BAD_REQUEST,
                        &ErrorResponse {
                            error: "service could not be created",
                        },
                    )
                }
            },
        );
    }
    if let Some(service_key) = path.strip_prefix("/v1/services/") {
        let service_id = match uuid::Uuid::parse_str(service_key) {
            Ok(uuid) => ServiceId(uuid),
            Err(_) => {
                return Ok(json(
                    StatusCode::BAD_REQUEST,
                    &ErrorResponse {
                        error: "invalid service id",
                    },
                ));
            }
        };
        let Some(_kind) = authenticate(request.headers(), &state) else {
            return Ok(json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ));
        };
        match (request.method(), service_key) {
            (&Method::GET, _) => {
                let service = state.services.lock().await.get(&service_id).cloned();
                return Ok(match service {
                    Some(service) => {
                        let node = state.nodes.lock().await.get(&service.node_id).cloned();
                        json(
                            StatusCode::OK,
                            &ServiceView {
                                status: if node
                                    .as_ref()
                                    .is_some_and(|n| n.status == NodeStatus::Online)
                                {
                                    ServiceStatus::Online
                                } else {
                                    ServiceStatus::Offline
                                },
                                transport: node.as_ref().map(|n| n.transport),
                                last_heartbeat_at_unix_ms: node
                                    .as_ref()
                                    .and_then(|n| n.last_heartbeat_at_unix_ms),
                                rtt_ms: node.as_ref().and_then(|n| n.rtt_ms),
                                service,
                            },
                        )
                    }
                    None => json(
                        StatusCode::NOT_FOUND,
                        &ErrorResponse {
                            error: "service not found",
                        },
                    ),
                });
            }
            (&Method::DELETE, _) => {
                let removed = state.services.lock().await.remove(&service_id);
                return Ok(match removed {
                    Some(_) => {
                        if let Some(task) = state.tcp_ingress_tasks.lock().await.remove(&service_id)
                        {
                            task.abort();
                        }
                        match state.store.delete(service_id) {
                            Ok(()) => json(
                                StatusCode::OK,
                                &serde_json::json!({"deleted": true, "service_id": service_id }),
                            ),
                            Err(error) => {
                                error!(%error, "failed to delete service from store");
                                json(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    &ErrorResponse {
                                        error: "failed to delete service",
                                    },
                                )
                            }
                        }
                    }
                    None => json(
                        StatusCode::NOT_FOUND,
                        &ErrorResponse {
                            error: "service not found",
                        },
                    ),
                });
            }
            _ => {}
        }
    }
    let response = match (request.method(), path.as_str()) {
        (&Method::GET, "/healthz") | (&Method::GET, "/readyz") => {
            json(StatusCode::OK, &HealthResponse { status: "ok" })
        }
        (&Method::POST, "/v1/auth/validate") => match authenticate(request.headers(), &state) {
            Some(TokenKind::Admin) => json(
                StatusCode::OK,
                &AuthResponse {
                    kind: "admin",
                    capabilities: vec![
                        "services:read",
                        "services:write",
                        "nodes:read",
                        "tokens:write",
                    ],
                    server: "ongrok",
                },
            ),
            Some(TokenKind::User) => json(
                StatusCode::OK,
                &AuthResponse {
                    kind: "user",
                    capabilities: vec!["services:read", "services:write", "nodes:read"],
                    server: "ongrok",
                },
            ),
            None => json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ),
        },
        (&Method::GET, "/v1/services") => match authenticate(request.headers(), &state) {
            Some(_) => {
                let services = state.services.lock().await;
                let nodes = state.nodes.lock().await;
                let services = services
                    .values()
                    .cloned()
                    .map(|service| {
                        let node = nodes.get(&service.node_id);
                        ServiceView {
                            status: if node.is_some_and(|node| node.status == NodeStatus::Online) {
                                ServiceStatus::Online
                            } else {
                                ServiceStatus::Offline
                            },
                            transport: node.map(|node| node.transport),
                            last_heartbeat_at_unix_ms: node
                                .and_then(|node| node.last_heartbeat_at_unix_ms),
                            rtt_ms: node.and_then(|node| node.rtt_ms),
                            service,
                        }
                    })
                    .collect::<Vec<_>>();
                json(StatusCode::OK, &services)
            }
            None => json(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse {
                    error: "invalid bearer token",
                },
            ),
        },
        _ => json(
            StatusCode::NOT_FOUND,
            &ErrorResponse {
                error: "route not found",
            },
        ),
    };
    Ok(response)
}

async fn token_mutation_request(request: Request<Incoming>) -> Result<TokenMutationRequest> {
    let bytes = request
        .into_body()
        .collect()
        .await
        .context("failed to read token mutation request")?
        .to_bytes();
    if bytes.len() > MAX_API_BODY_BYTES {
        anyhow::bail!("token mutation request is too large");
    }
    serde_json::from_slice(&bytes).context("failed to decode token mutation request")
}

async fn replace_token(state: &AppState, kind: TokenKind, hash: Option<[u8; 32]>) -> Result<()> {
    {
        let mut tokens = state
            .tokens
            .write()
            .map_err(|_| anyhow::anyhow!("token state lock is poisoned"))?;
        let mut updated = tokens.clone();
        updated.set(kind, hash);
        state.store.put_tokens(&updated)?;
        *tokens = updated;
    }

    let disconnected = {
        let mut sessions = state.sessions.lock().await;
        let node_ids = sessions
            .iter()
            .filter_map(|(node_id, session)| (session.token_kind == kind).then_some(*node_id))
            .collect::<Vec<_>>();
        node_ids
            .into_iter()
            .filter_map(|node_id| sessions.remove(&node_id).map(|session| (node_id, session)))
            .collect::<Vec<_>>()
    };
    for (_, session) in &disconnected {
        session.connection.close();
    }
    let offline_nodes = {
        let mut nodes = state.nodes.lock().await;
        disconnected
            .iter()
            .filter_map(|(node_id, _)| {
                nodes.get_mut(node_id).map(|node| {
                    node.status = NodeStatus::Offline;
                    node.clone()
                })
            })
            .collect::<Vec<_>>()
    };
    for node in offline_nodes {
        state.store.put_node(&node)?;
    }
    Ok(())
}

fn token_kind_name(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Admin => "admin",
        TokenKind::User => "user",
    }
}

fn authenticate(headers: &header::HeaderMap, state: &AppState) -> Option<TokenKind> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ")?;
    authenticate_token(token, state)
}

fn authenticate_token(token: &str, state: &AppState) -> Option<TokenKind> {
    let hash = hash_token(token);
    state.tokens.read().ok()?.authenticate(hash)
}

fn json<T: Serialize>(status: StatusCode, value: &T) -> Response<ApiBody> {
    match serde_json::to_vec(value) {
        Ok(bytes) => raw_json(status, &String::from_utf8_lossy(&bytes)),
        Err(error) => {
            error!(%error, "failed to encode JSON response");
            raw_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "{\"error\":\"internal error\"}",
            )
        }
    }
}
fn raw_json(status: StatusCode, value: &str) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::copy_from_slice(value.as_bytes())))
        .expect("response builder must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TokenState;

    fn test_state() -> AppState {
        let path = std::env::temp_dir().join(format!("ongrok-test-{}.redb", ServiceId::new().0));
        AppState {
            tokens: Arc::new(RwLock::new(TokenState {
                admin_hash: Some(hash_token("admin")),
                user_hash: Some(hash_token("user")),
            })),
            services: Arc::new(Mutex::new(BTreeMap::new())),
            nodes: Arc::new(Mutex::new(BTreeMap::new())),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            tcp_ingress_tasks: Arc::new(Mutex::new(BTreeMap::new())),
            public_host: Arc::from("localhost"),
            http_domain: None,
            tcp_port_start: 20_000,
            tcp_port_end: 30_000,
            store: Arc::new(ServiceStore::open(&path).unwrap()),
        }
    }

    fn node_record(node_id: libongrok::NodeId, public_key: Option<[u8; 32]>) -> NodeRecord {
        NodeRecord {
            node_id,
            public_key,
            metadata: libongrok::NodeMetadata {
                hostname: "test".into(),
                os: "test".into(),
                arch: "test".into(),
                client_version: "test".into(),
                metadata: Default::default(),
            },
            public_ip: "127.0.0.1".into(),
            source_port: 1234,
            transport: TransportKind::Quic,
            status: NodeStatus::Offline,
            connected_at_unix_ms: 0,
            last_heartbeat_at_unix_ms: None,
            rtt_ms: None,
        }
    }

    #[tokio::test]
    async fn node_identity_validation_allows_new_same_and_legacy_but_rejects_mismatch() {
        let state = test_state();
        let node_id = libongrok::NodeId::new();
        let first_key = [7_u8; 32];
        let other_key = [8_u8; 32];

        assert!(
            validate_node_identity(node_id, first_key, &state)
                .await
                .is_ok()
        );

        state
            .nodes
            .lock()
            .await
            .insert(node_id, node_record(node_id, Some(first_key)));
        assert!(
            validate_node_identity(node_id, first_key, &state)
                .await
                .is_ok()
        );
        let error = validate_node_identity(node_id, other_key, &state)
            .await
            .expect_err("a persisted identity mismatch must be rejected");
        assert!(
            error
                .to_string()
                .contains("does not match persisted identity")
        );

        state
            .nodes
            .lock()
            .await
            .insert(node_id, node_record(node_id, None));
        assert!(
            validate_node_identity(node_id, other_key, &state)
                .await
                .is_ok()
        );
    }

    #[test]
    fn bearer_authentication_distinguishes_kinds() {
        let state = test_state();
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer user".parse().unwrap());
        assert_eq!(authenticate(&headers, &state), Some(TokenKind::User));
        headers.insert(header::AUTHORIZATION, "Bearer invalid".parse().unwrap());
        assert_eq!(authenticate(&headers, &state), None);
    }

    #[test]
    fn token_state_persists_rotation_and_revocation() {
        let path = std::env::temp_dir().join(format!("ongrok-tokens-{}.redb", ServiceId::new().0));
        let store = ServiceStore::open(&path).unwrap();
        let initial = store
            .load_or_initialize_tokens(hash_token("admin"), hash_token("user"))
            .unwrap();
        assert_eq!(
            initial.authenticate(hash_token("admin")),
            Some(TokenKind::Admin)
        );
        let rotated = TokenState {
            admin_hash: Some(hash_token("admin-2")),
            user_hash: None,
        };
        store.put_tokens(&rotated).unwrap();
        let reloaded = store
            .load_or_initialize_tokens(hash_token("ignored"), hash_token("ignored"))
            .unwrap();
        assert_eq!(
            reloaded.authenticate(hash_token("admin-2")),
            Some(TokenKind::Admin)
        );
        assert_eq!(reloaded.authenticate(hash_token("user")), None);
    }

    #[test]
    fn service_store_round_trips_records() {
        let path = std::env::temp_dir().join(format!("ongrok-store-{}.redb", ServiceId::new().0));
        let store = ServiceStore::open(&path).unwrap();
        let service = ServiceDefinition {
            service_id: ServiceId::new(),
            service_name: "ssh".into(),
            node_id: libongrok::NodeId::new(),
            protocol: libongrok::Protocol::Tcp,
            local_address: "127.0.0.1:22".into(),
            public_host: Some("gateway.example.test".into()),
            public_port: Some(22001),
            metadata: Default::default(),
        };
        store.put(&service).unwrap();
        let loaded = store.load_services().unwrap();
        assert_eq!(loaded.get(&service.service_id), Some(&service));
        store.delete(service.service_id).unwrap();
        assert!(store.load_services().unwrap().is_empty());
    }
}
