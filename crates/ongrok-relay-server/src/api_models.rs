//! Stable JSON shapes shared by the control API handlers.

use libongrok::{
    Metadata, NodeId, Protocol, ServiceDefinition, ServiceId, ServiceStatus, TokenKind,
    TransportKind,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(crate) struct ErrorResponse<'a> {
    pub(crate) error: &'a str,
}

#[derive(Serialize)]
pub(crate) struct AuthResponse {
    pub(crate) kind: &'static str,
    pub(crate) capabilities: Vec<&'static str>,
    pub(crate) server: &'static str,
}

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    pub(crate) status: &'static str,
}

#[derive(Deserialize)]
pub(crate) struct TokenMutationRequest {
    pub(crate) kind: TokenKind,
}

#[derive(Serialize)]
pub(crate) struct TokenRotationResponse {
    pub(crate) kind: &'static str,
    pub(crate) token: String,
}

#[derive(Serialize)]
pub(crate) struct TokenRevocationResponse {
    pub(crate) kind: &'static str,
    pub(crate) revoked: bool,
}

#[derive(Serialize)]
pub(crate) struct ServiceView {
    #[serde(flatten)]
    pub(crate) service: ServiceDefinition,
    pub(crate) status: ServiceStatus,
    pub(crate) transport: Option<TransportKind>,
    pub(crate) last_heartbeat_at_unix_ms: Option<i64>,
    pub(crate) rtt_ms: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct ServiceCreateRequest {
    pub(crate) service_name: String,
    pub(crate) node_id: NodeId,
    pub(crate) protocol: Protocol,
    pub(crate) local_address: String,
    #[serde(default)]
    pub(crate) public_port: Option<u16>,
    #[serde(default)]
    pub(crate) metadata: Metadata,
}

#[derive(Deserialize)]
pub(crate) struct ServicePatchRequest {
    #[serde(default)]
    pub(crate) metadata: Option<Metadata>,
}

impl ServiceCreateRequest {
    pub(crate) fn into_definition(self) -> ServiceDefinition {
        ServiceDefinition {
            service_id: ServiceId::new(),
            service_name: self.service_name,
            node_id: self.node_id,
            protocol: self.protocol,
            local_address: self.local_address,
            public_host: None,
            public_port: self.public_port,
            metadata: self.metadata,
        }
    }
}
