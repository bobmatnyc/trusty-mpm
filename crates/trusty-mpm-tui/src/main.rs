//! trusty-mpm TUI dashboard.
//!
//! Why: operators need an at-a-glance, multi-session view — every active
//! Claude Code session, its agents, memory pressure, and a live hook-event
//! feed — without parsing daemon logs. This is a superset of the claude-mpm
//! dashboard: the same metrics, but across N sessions.
//! What: a ratatui app that polls the daemon HTTP API on a timer and renders
//! the [`dashboard`] panels; `q`/`Esc` quits. Rendering and HTTP are split into
//! the [`dashboard`] and [`client`] modules so the logic is unit-testable.
//! Test: `cargo test -p trusty-mpm-tui` covers row formatting and the client;
//! `cargo run -p trusty-mpm-tui` launches the live dashboard.

mod client;
mod dashboard;

use std::time::Duration;

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use client::DaemonClient;
use dashboard::DashboardState;

/// trusty-mpm TUI command-line options.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpm-tui", version, about = "trusty-mpm dashboard")]
struct Args {
    /// Base URL of the trusty-mpm daemon.
    #[arg(long, env = "TRUSTY_MPM_URL", default_value = "http://127.0.0.1:7880")]
    url: String,

    /// Poll interval in milliseconds.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = DaemonClient::new(args.url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &client, args.interval_ms).await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// The dashboard event loop: poll the daemon, render, handle input.
///
/// Why: kept separate from `main` so terminal setup/teardown wraps it cleanly.
/// What: each tick refreshes [`DashboardState`] from the daemon, redraws, and
/// checks for a quit key (`q` / `Esc`).
/// Test: the pure parts (rendering, client) are unit-tested; this loop is the
/// thin glue exercised by `cargo run`.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: &DaemonClient,
    interval_ms: u64,
) -> anyhow::Result<()> {
    let mut state = DashboardState::default();
    loop {
        // Refresh from the daemon: probe health first, then pull every panel's
        // data — sessions, the hook-event feed, and circuit breakers.
        state.daemon_reachable = client.is_healthy().await;
        if state.daemon_reachable {
            match client.sessions().await {
                Ok(sessions) => state.sessions = sessions,
                Err(_) => state.daemon_reachable = false,
            }
            match client.events().await {
                Ok(events) => state.events = events,
                Err(_) => state.daemon_reachable = false,
            }
            match client.breakers().await {
                Ok(breakers) => state.breakers = breakers,
                Err(_) => state.daemon_reachable = false,
            }
        } else {
            state.sessions.clear();
            state.events.clear();
            state.breakers.clear();
        }
        terminal.draw(|f| dashboard::render(f, &state))?;

        // Wait for input up to the poll interval; quit on q/Esc.
        if event::poll(Duration::from_millis(interval_ms))?
            && let Event::Key(key) = event::read()?
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}
