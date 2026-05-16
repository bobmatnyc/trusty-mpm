//! trusty-mpm daemon library.
//!
//! Why: the daemon's HTTP API and shared state are useful beyond the `trusty-mpmd`
//! binary — sibling crates (e.g. the Telegram bot's test suite) reuse the real
//! `api::router` and `DaemonState` to drive in-process integration tests without
//! a live daemon. Exposing the modules as a library makes that possible.
//! What: re-exports the daemon's modules as `pub` so both `main.rs` and external
//! consumers can build against them.
//! Test: the modules carry their own `#[cfg(test)]` suites; `cargo test
//! -p trusty-mpm-daemon` exercises them.

pub mod api;
pub mod audit;
pub mod discover;
pub mod mcp_backend;
pub mod optimizer;
pub mod state;
pub mod tmux;
pub mod watcher;

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::info;

pub use state::DaemonState;

/// Run the resident HTTP daemon: API, hook relay, dashboard feed.
///
/// Why: the HTTP boot sequence is shared by both the standalone `trusty-mpmd`
/// shim and the unified `trusty-mpm daemon` subcommand; living in the library
/// keeps a single source of truth.
/// What: announces tmux availability, discovers the trusty sidecars, spawns the
/// file watcher, then serves the axum router until the socket closes.
/// Test: `cargo run -p trusty-mpm-cli -- daemon` logs "trusty-mpm daemon
/// starting" and `curl localhost:7880/health` returns `ok`.
pub async fn run_http(state: Arc<DaemonState>, addr: SocketAddr) -> anyhow::Result<()> {
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

    // Spawn the periodic dead-session reaper so registry entries for tmux
    // sessions that have exited do not accumulate forever.
    tokio::spawn(reap_loop(Arc::clone(&state)));

    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("daemon listening; press Ctrl-C to stop");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Interval between dead-session reap sweeps.
const REAP_INTERVAL_SECS: u64 = 60;

/// Periodically prune registry entries whose tmux session has exited.
///
/// Why: without housekeeping, dead sessions accumulate in `DaemonState`
/// forever; a slow background sweep keeps the registry honest.
/// What: every [`REAP_INTERVAL_SECS`] seconds, discovers tmux and calls
/// [`DaemonState::reap_dead_sessions`]; logs how many entries were reaped.
/// Test: the reaping rule is unit-tested via `DaemonState::reap_against`.
async fn reap_loop(state: Arc<DaemonState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(REAP_INTERVAL_SECS));
    loop {
        tick.tick().await;
        if let Ok(driver) = tmux::TmuxDriver::discover() {
            let removed = state.reap_dead_sessions(&driver);
            if removed > 0 {
                info!("reaped {removed} dead session(s)");
            }
        }
    }
}

/// Run the MCP server over stdio so a Claude Code session can call the
/// orchestration tools (`session_list`, `agent_delegate`, ...).
///
/// Why: shared by `trusty-mpmd mcp` and `trusty-mpm daemon --mcp`.
/// What: wraps [`DaemonState`] in a [`mcp_backend::StateBackend`] and pumps the
/// trusty-mcp-core stdio JSON-RPC loop.
/// Test: pipe a JSON-RPC `initialize` request to the process and observe a
/// well-formed response on stdout.
pub async fn run_mcp(state: Arc<DaemonState>) -> anyhow::Result<()> {
    info!("trusty-mpm MCP server starting on stdio");
    let backend = mcp_backend::StateBackend::new(state);
    trusty_mcp_core::run_stdio_loop(move |req| {
        let backend = backend.clone();
        async move { trusty_mpm_mcp::dispatch(&backend, req).await }
    })
    .await
}
