//! trusty-mpm daemon entry point (`trusty-mpmd`).
//!
//! Why: claude-mpm spawns a fresh Python process per hook invocation; a single
//! long-lived daemon removes that per-call cost and enables shared state. This
//! binary is kept as a backward-compatible shim — the primary entry point is
//! now `trusty-mpm daemon`, which calls the same library functions.
//! What: boots tracing, parses CLI flags, builds the shared [`DaemonState`],
//! and delegates to [`trusty_mpm_daemon::run_http`] or
//! [`trusty_mpm_daemon::run_mcp`].
//! Test: `cargo run -p trusty-mpm-daemon` logs "trusty-mpm daemon starting" and
//! `curl localhost:7880/health` returns `ok`.

use std::net::SocketAddr;

use clap::{Parser, Subcommand};

use trusty_mpm_daemon::{DaemonState, run_http, run_mcp};

/// trusty-mpm daemon command-line options.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpmd", version, about = "trusty-mpm daemon")]
struct Args {
    /// Run mode (defaults to the resident HTTP daemon).
    #[command(subcommand)]
    mode: Option<Mode>,
}

/// Daemon run modes.
#[derive(Debug, Subcommand)]
enum Mode {
    /// Run the resident HTTP API and universal hook relay.
    Http {
        /// Address the daemon HTTP API binds to.
        #[arg(long, env = "TRUSTY_MPM_ADDR", default_value = "127.0.0.1:7880")]
        addr: SocketAddr,
    },
    /// Run as an MCP server over stdio (launched by a Claude Code session).
    Mcp,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // MCP mode speaks JSON-RPC on stdout — keep tracing on stderr.
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let state = DaemonState::shared();

    match args.mode.unwrap_or(Mode::Http {
        addr: "127.0.0.1:7880".parse().expect("valid default addr"),
    }) {
        Mode::Http { addr } => run_http(state, addr).await,
        Mode::Mcp => run_mcp(state).await,
    }
}
