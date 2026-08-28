//! ongrok relay server binary entry point.

mod api_models;
mod config;
mod ingress;
mod relay;
mod server;
mod state;
mod store;
mod transport;
mod wire;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load optional local development/deployment values before clap resolves
    // its `env` arguments. Explicitly exported variables still take priority.
    let _ = dotenvy::dotenv();
    server::run_cli().await
}
