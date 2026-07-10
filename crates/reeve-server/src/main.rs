//! Thin binary entrypoint; all logic lives in the reeve-server library so
//! integration tests exercise the same code paths.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logs to stdout (operational contract, CLAUDE.md).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = reeve_server::config::Config::from_env()?;
    reeve_server::run(cfg).await
}
