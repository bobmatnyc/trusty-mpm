//! trusty-mpm daemon entry point.
//!
//! Why: claude-mpm spawns a fresh Python process per hook invocation; a single
//! long-lived daemon removes that per-call cost and enables shared state.
//! What: boots tracing, parses CLI flags, builds the shared [`DaemonState`],
//! and runs in one of two modes — `http` (the resident API + hook relay) or
//! `mcp` (an MCP server over stdio so a Claude Code session can speak directly
//! to the orchestrator).
//! Test: `cargo run -p trusty-mpm-daemon` logs "trusty-mpm daemon starting" and
//! `curl localhost:7880/health` returns `ok`; unit tests live in the submodules.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::info;

use trusty_mpm_daemon::{api, discover, mcp_backend, state, tmux, watcher};

use mcp_backend::StateBackend;
use state::DaemonState;

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

/// Run the resident HTTP daemon: API, hook relay, dashboard feed.
async fn run_http(state: Arc<DaemonState>, addr: SocketAddr) -> anyhow::Result<()> {
    info!("trusty-mpm daemon starting on {addr}");
    if tmux::TmuxDriver::is_available() {
        info!("tmux control model available");
    } else {
        info!("tmux not found — sessions will need the PTY or SDK control model");
    }

    // Discover the trusty sidecar addresses and record them in shared state.
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let addrs = discover::discover_all(&home).await;
    info!(
        "trusty-memory at {}, trusty-search at {}",
        addrs.memory, addrs.search
    );
    state.set_trusty_addrs(addrs);

    // Spawn the multi-session file watcher as a background task.
    let fw = watcher::FileWatcher::new(Arc::clone(&state));
    tokio::spawn(fw.spawn());

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("daemon listening; press Ctrl-C to stop");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Run the MCP server over stdio so a Claude Code session can call the
/// orchestration tools (`session_list`, `agent_delegate`, ...).
async fn run_mcp(state: Arc<DaemonState>) -> anyhow::Result<()> {
    info!("trusty-mpm MCP server starting on stdio");
    let backend = StateBackend::new(state);
    trusty_mcp_core::run_stdio_loop(move |req| {
        let backend = backend.clone();
        async move { trusty_mpm_mcp::dispatch(&backend, req).await }
    })
    .await
}
