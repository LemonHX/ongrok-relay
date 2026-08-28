//! In-memory control-plane state and authenticated client session ownership.

use crate::store::ServiceStore;
use anyhow::Result;
use libongrok::{
    NodeId, NodeRecord, ServiceDefinition, ServiceId, TokenKind, TunnelId, YamuxSession,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};
use tokio::{sync::Mutex, task::JoinHandle};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) tokens: Arc<RwLock<TokenState>>,
    pub(crate) services: Arc<Mutex<BTreeMap<ServiceId, ServiceDefinition>>>,
    pub(crate) nodes: Arc<Mutex<BTreeMap<NodeId, NodeRecord>>>,
    pub(crate) sessions: Arc<Mutex<BTreeMap<NodeId, ActiveSession>>>,
    pub(crate) tcp_ingress_tasks: Arc<Mutex<BTreeMap<ServiceId, JoinHandle<()>>>>,
    pub(crate) public_host: Arc<str>,
    pub(crate) http_domain: Option<Arc<str>>,
    pub(crate) tcp_port_start: u16,
    pub(crate) tcp_port_end: u16,
    pub(crate) store: Arc<ServiceStore>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct TokenState {
    pub(crate) admin_hash: Option<[u8; 32]>,
    pub(crate) user_hash: Option<[u8; 32]>,
}

impl TokenState {
    pub(crate) fn authenticate(&self, token_hash: [u8; 32]) -> Option<TokenKind> {
        if self.admin_hash == Some(token_hash) {
            Some(TokenKind::Admin)
        } else if self.user_hash == Some(token_hash) {
            Some(TokenKind::User)
        } else {
            None
        }
    }

    pub(crate) fn set(&mut self, kind: TokenKind, token_hash: Option<[u8; 32]>) {
        match kind {
            TokenKind::Admin => self.admin_hash = token_hash,
            TokenKind::User => self.user_hash = token_hash,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActiveSession {
    pub(crate) id: TunnelId,
    pub(crate) token_kind: TokenKind,
    pub(crate) connection: ClientSession,
}

#[derive(Clone)]
pub(crate) enum ClientSession {
    Quic(quinn::Connection),
    Yamux(YamuxSession),
}

impl ClientSession {
    pub(crate) fn close(&self) {
        match self {
            Self::Quic(connection) => connection.close(0_u32.into(), b"token revoked"),
            Self::Yamux(session) => session.close(),
        }
    }
}

pub(crate) async fn activate_session(
    node_id: NodeId,
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

/// Reject an impostor while allowing persisted records created before node keys existed.
pub(crate) async fn validate_node_identity(
    node_id: NodeId,
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
