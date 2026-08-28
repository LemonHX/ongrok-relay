//! ongrok relay server binary entry point.

mod api_models;
mod config;
mod ingress;
mod server;
mod state;
mod store;
mod wire;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server::run_cli().await
}
