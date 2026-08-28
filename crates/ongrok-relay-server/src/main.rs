//! ongrok relay server binary entry point.

mod config;
mod server;
mod state;
mod store;
mod wire;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server::run_cli().await
}
