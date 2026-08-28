//! Public HTTP and HTTPS ingress forwarding.

use crate::{server::open_client_stream, state::AppState};
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::header::HeaderName;
use hyper::{
    HeaderMap, Request, Response, StatusCode, body::Incoming, client::conn::http1, header,
    service::service_fn,
};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use libongrok::Protocol;
use rustls::ServerConfig;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::{net::TcpListener, task::JoinSet, time::timeout};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

type ProxyError = Box<dyn std::error::Error + Send + Sync>;
type IngressBody = BoxBody<Bytes, ProxyError>;
const MAX_HTTP_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) async fn run_http_ingress(address: std::net::SocketAddr, state: AppState) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind HTTP ingress at {address}"))?;
    info!(%address, "HTTP ingress listening");
    let state = Arc::new(state);
    let mut tasks = JoinSet::new();
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("HTTP ingress accept failed")?;
        let state = Arc::clone(&state);
        tasks.spawn(async move {
            let service = service_fn(move |request| {
                http_ingress_handler(request, Arc::clone(&state), Protocol::Http)
            });
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder.http1().max_buf_size(MAX_HTTP_BUFFER_BYTES);
            if let Err(error) = builder
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                warn!(%peer, %error, "HTTP ingress connection failed");
            }
        });
    }
}

pub(crate) async fn run_https_ingress(
    address: std::net::SocketAddr,
    tls: Arc<ServerConfig>,
    state: AppState,
) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind HTTPS ingress at {address}"))?;
    let mut config = (*tls).clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    info!(%address, "HTTPS ingress listening");
    let state = Arc::new(state);
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .context("HTTPS ingress accept failed")?;
        let acceptor = acceptor.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let result = async {
                let stream = timeout(Duration::from_secs(15), acceptor.accept(stream))
                    .await
                    .context("HTTPS ingress handshake timed out")??;
                let service = service_fn(move |request| {
                    http_ingress_handler(request, Arc::clone(&state), Protocol::Https)
                });
                let mut builder = auto::Builder::new(TokioExecutor::new());
                builder.http1().max_buf_size(MAX_HTTP_BUFFER_BYTES);
                builder
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .map_err(|error| anyhow::anyhow!("HTTPS ingress connection failed: {error}"))
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "HTTPS ingress connection failed");
            }
        });
    }
}

async fn http_ingress_handler(
    request: Request<Incoming>,
    state: Arc<AppState>,
    protocol: Protocol,
) -> Result<Response<IngressBody>, Infallible> {
    let response = match forward_http_request(request, state, protocol).await {
        Ok(response) => response,
        Err(error) => {
            warn!(%error, "HTTP ingress request failed");
            ingress_error(StatusCode::BAD_GATEWAY, "service is unavailable")
        }
    };
    Ok(response)
}

async fn forward_http_request(
    mut request: Request<Incoming>,
    state: Arc<AppState>,
    protocol: Protocol,
) -> Result<Response<IngressBody>> {
    let host = request_host(&request).context("HTTP request is missing a Host header")?;
    let service = state
        .services
        .lock()
        .await
        .values()
        .find(|service| {
            service.protocol == protocol && service.public_host.as_deref() == Some(host.as_str())
        })
        .cloned()
        .context("no HTTP service is registered for host")?;
    let (tunnel, _) = open_client_stream(&state, service.service_id).await?;
    strip_hop_by_hop_headers(request.headers_mut());
    let (mut sender, connection) = http1::handshake(TokioIo::new(tunnel))
        .await
        .context("failed to establish HTTP tunnel connection")?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "HTTP tunnel connection ended");
        }
    });
    let response = sender
        .send_request(request)
        .await
        .context("local HTTP service request failed")?;
    let (mut parts, body) = response.into_parts();
    strip_hop_by_hop_headers(&mut parts.headers);
    Ok(Response::from_parts(
        parts,
        body.map_err(|error| -> ProxyError { Box::new(error) })
            .boxed(),
    ))
}

fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_names = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<HeaderName>().ok())
        .collect::<Vec<_>>();
    for name in connection_names {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

fn request_host(request: &Request<Incoming>) -> Option<String> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        })?;
    Some(
        host.trim_end_matches('.')
            .split(':')
            .next()?
            .to_ascii_lowercase(),
    )
}

fn ingress_error(status: StatusCode, message: &'static str) -> Response<IngressBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from_static(message.as_bytes()))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("ingress error response is valid")
}

#[cfg(test)]
mod tests {
    use super::strip_hop_by_hop_headers;
    use hyper::header::{CONNECTION, HeaderMap, HeaderName, HeaderValue};

    #[test]
    fn strips_standard_and_connection_declared_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONNECTION,
            HeaderValue::from_static("x-tunnel-hop, keep-alive"),
        );
        headers.insert("x-tunnel-hop", HeaderValue::from_static("secret"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-end-to-end", HeaderValue::from_static("kept"));
        strip_hop_by_hop_headers(&mut headers);
        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key(HeaderName::from_static("x-tunnel-hop")));
        assert!(!headers.contains_key(HeaderName::from_static("keep-alive")));
        assert_eq!(headers["x-end-to-end"], "kept");
    }
}
