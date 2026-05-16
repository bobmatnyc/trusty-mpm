//! trusty-mpm daemon entry point.
//!
//! Why: claude-mpm spawns a fresh Python process per hook invocation; a single
//! long-lived daemon removes that per-call cost and enables shared state.
//! What: Boots tracing, parses CLI flags, and (in this scaffold) logs startup
//! then serves a minimal HTTP health endpoint.
//! Test: `cargo run -p trusty-mpm-daemon` should log "trusty-mpm daemon starting"
//! and `curl localhost:7880/health` should return `ok`.

use std::net::SocketAddr;

use axum::{routing::get, Router};
use clap::Parser;
use tracing::info;

/// trusty-mpm daemon command-line options.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpmd", version, about = "trusty-mpm daemon")]
struct Args {
    /// Address the daemon HTTP API binds to.
    #[arg(long, env = "TRUSTY_MPM_ADDR", default_value = "127.0.0.1:7880")]
    addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    info!("trusty-mpm daemon starting on {}", args.addr);

    let app = Router::new().route("/health", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind(args.addr).await?;
    info!("daemon listening; press Ctrl-C to stop");
    axum::serve(listener, app).await?;

    Ok(())
}
