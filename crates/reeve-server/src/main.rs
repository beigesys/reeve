//! Thin binary entrypoint; all logic lives in the reeve-server library so
//! integration tests exercise the same code paths.
//!
//! Invocations:
//! - `reeve-server`                        — run (normal startup)
//! - `reeve-server --restore-from-target`  — DR: with NO local DB and a
//!   configured durability target, restore the latest generation first
//!   (spec/reeve/07-durability.md §9.5; needs the keyfile in place too)
//! - `reeve-server verify-restore`         — one §9.4 verify-restore
//!   pass; prints the outcome as JSON, exit 0 iff the chain verified

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Structured logs to stdout (operational contract, CLAUDE.md).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = reeve_server::config::Config::from_env()?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("verify-restore") => {
            let outcome = reeve_server::durability::verify_restore_cli(cfg).await?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            if !outcome.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        _ => {
            let mut opts = reeve_server::RunOptions::default();
            for arg in &args {
                match arg.as_str() {
                    "--restore-from-target" => opts.restore_from_target = true,
                    other => anyhow::bail!(
                        "unknown argument {other:?} (expected `verify-restore` or \
                         `--restore-from-target`)"
                    ),
                }
            }
            reeve_server::run_with_options(cfg, opts).await
        }
    }
}
