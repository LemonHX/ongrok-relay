//! TCP ingress lease activation and byte forwarding.

use crate::{server::open_client_stream, state::AppState};
use anyhow::{Context, Result};
use libongrok::{ServiceDefinition, ServiceId};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::{
    io::{AsyncWriteExt, copy},
    net::{TcpListener, TcpStream},
};
use tracing::{info, warn};

pub(crate) fn first_available_tcp_port(
    services: &std::collections::BTreeMap<ServiceId, ServiceDefinition>,
    state: &AppState,
) -> Option<u16> {
    (state.tcp_port_start..=state.tcp_port_end).find(|port| {
        !services.values().any(|service| {
            service.protocol == libongrok::Protocol::Tcp && service.public_port == Some(*port)
        })
    })
}

pub(crate) async fn ensure_tcp_ingress(state: AppState, service: &ServiceDefinition) -> Result<()> {
    let port = service
        .public_port
        .context("TCP service has no assigned public port")?;
    if state
        .tcp_ingress_tasks
        .lock()
        .await
        .contains_key(&service.service_id)
    {
        return Ok(());
    }
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind TCP ingress at {address}"))?;
    let service_id = service.service_id;
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        info!(%address, service_id = %service_id.0, "TCP ingress listening");
        loop {
            match listener.accept().await {
                Ok((visitor, peer)) => {
                    let state = task_state.clone();
                    tokio::spawn(async move {
                        if let Err(error) = forward_tcp_connection(visitor, state, service_id).await
                        {
                            warn!(%peer, service_id = %service_id.0, %error, "TCP ingress connection failed");
                        }
                    });
                }
                Err(error) => {
                    warn!(%address, service_id = %service_id.0, %error, "TCP ingress accept failed");
                    break;
                }
            }
        }
    });
    if let Some(previous) = state
        .tcp_ingress_tasks
        .lock()
        .await
        .insert(service_id, task)
    {
        previous.abort();
    }
    Ok(())
}

pub(crate) async fn forward_tcp_connection(
    visitor: TcpStream,
    state: AppState,
    service_id: ServiceId,
) -> Result<()> {
    let (tunnel, _) = open_client_stream(&state, service_id).await?;
    let (mut recv, mut send) = tokio::io::split(tunnel);
    let (mut visitor_read, mut visitor_write) = visitor.into_split();
    let visitor_to_client = async {
        let copied = copy(&mut visitor_read, &mut send).await?;
        send.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let client_to_visitor = async {
        let copied = copy(&mut recv, &mut visitor_write).await?;
        visitor_write.shutdown().await?;
        Ok::<u64, std::io::Error>(copied)
    };
    let (from_visitor, from_client) =
        tokio::try_join!(visitor_to_client, client_to_visitor).context("TCP relay copy failed")?;
    info!(service_id = %service_id.0, bytes_from_visitor = from_visitor, bytes_from_client = from_client, "TCP ingress connection completed");
    Ok(())
}
