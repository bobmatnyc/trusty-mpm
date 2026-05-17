//! trusty-mpm TUI dashboard library.
//!
//! Why: operators need an at-a-glance, multi-session view — every active
//! Claude Code session, its agents, memory pressure, and a live hook-event
//! feed — without parsing daemon logs. Exposing the dashboard as a library lets
//! the unified `trusty-mpm tui` subcommand reuse it without a separate binary.
//! What: a ratatui app that polls the daemon HTTP API on a timer and renders
//! the [`dashboard`] panels; `q`/`Esc` quits. Rendering and HTTP are split into
//! the [`dashboard`] and [`client`] modules so the logic is unit-testable.
//! Test: `cargo test -p trusty-mpm-tui` covers row formatting and the client;
//! `trusty-mpm tui` launches the live dashboard.

pub mod client;
pub mod dashboard;

use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, widgets::TableState};

use client::DaemonClient;
use dashboard::DashboardState;

/// Run the ratatui multi-session dashboard against `url`.
///
/// Why: shared entry point for both the `trusty-mpm tui` subcommand and the
/// backward-compatible `trusty-mpm-tui` shim binary.
/// What: sets up the alternate screen / raw mode, runs [`run_loop`], and always
/// restores the terminal afterward even on error.
/// Test: pure parts (rendering, client) are unit-tested; this is the thin glue
/// exercised by launching the dashboard.
pub async fn run(url: String, interval_ms: u64) -> anyhow::Result<()> {
    let client = DaemonClient::new(url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &client, interval_ms).await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Refresh [`DashboardState`] from one full daemon poll.
///
/// Why: keeps the poll logic out of the key-driven event loop so the loop can
/// re-poll on demand (after an action) as well as on its timer.
/// What: probes health, then pulls sessions / events / breakers and the on-disk
/// log tail; clears the panels when the daemon is unreachable. Re-clamps the
/// session selection so a shrunken list never leaves a stale index.
/// Test: the pure pieces (rendering, client, clamping) are unit-tested.
async fn poll_daemon(state: &mut DashboardState, client: &DaemonClient) {
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
    // The daemon log tail is read straight from disk, independent of the HTTP
    // poll — it stays useful even when the daemon is unreachable.
    state.log_lines = dashboard::read_log_tail(20);
    state.clamp_selection();
}

/// The dashboard event loop: poll the daemon, render, handle input.
///
/// Why: kept separate from [`run`] so terminal setup/teardown wraps it cleanly.
/// What: refreshes [`DashboardState`] from the daemon on an `interval_ms` timer
/// but polls the keyboard every 50ms so navigation and action keys feel
/// instantaneous; action keys (`p`/`r`/`x`/`o`) call the daemon inline and
/// trigger an immediate re-poll; `q`/`Esc` quits.
/// Test: the pure pieces (rendering, client, clamping) are unit-tested; this
/// loop is the thin glue exercised by launching the dashboard.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: &DaemonClient,
    interval_ms: u64,
) -> anyhow::Result<()> {
    let mut state = DashboardState::default();
    let mut table_state = TableState::default();

    // Prime the dashboard with one poll before the first render.
    poll_daemon(&mut state, client).await;
    let mut last_poll = Instant::now();

    loop {
        if !state.sessions.is_empty() {
            table_state.select(Some(state.selected_session));
        } else {
            table_state.select(None);
        }
        terminal.draw(|f| dashboard::render_with_table_state(f, &state, &mut table_state))?;

        // Poll the keyboard on a tight 50ms cadence so input feels snappy even
        // with a slow data-refresh interval.
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc if !state.show_help => return Ok(()),
                KeyCode::Esc => state.show_help = false,
                KeyCode::Char('?') => state.show_help = !state.show_help,
                KeyCode::Up | KeyCode::Char('k') => state.select_up(),
                KeyCode::Down | KeyCode::Char('j') => state.select_down(),
                KeyCode::Char('p') => {
                    handle_action(&mut state, client, Action::Pause).await;
                    poll_daemon(&mut state, client).await;
                    last_poll = Instant::now();
                }
                KeyCode::Char('r') => {
                    handle_action(&mut state, client, Action::Resume).await;
                    poll_daemon(&mut state, client).await;
                    last_poll = Instant::now();
                }
                KeyCode::Char('x') => {
                    handle_action(&mut state, client, Action::Stop).await;
                    poll_daemon(&mut state, client).await;
                    last_poll = Instant::now();
                }
                KeyCode::Char('o') => {
                    handle_action(&mut state, client, Action::Output).await;
                }
                _ => {}
            }
        }

        // Throttle the data refresh: only re-poll the daemon every interval_ms.
        if last_poll.elapsed() >= Duration::from_millis(interval_ms) {
            poll_daemon(&mut state, client).await;
            last_poll = Instant::now();
        }
    }
}

/// The session action a key press maps to.
#[derive(Debug, Clone, Copy)]
enum Action {
    /// Pause the selected session (`p`).
    Pause,
    /// Resume the selected session (`r`).
    Resume,
    /// Stop the selected session (`x`).
    Stop,
    /// Capture the selected session's output (`o`).
    Output,
}

/// Run a session [`Action`] against the selected session.
///
/// Why: the four action keys share the same shape — resolve the selected
/// session's `tmux_name`, call the daemon, and record a status-bar message.
/// What: skips the HTTP call with `"no sessions"` when the list is empty;
/// stores either a success line or `"error: {e}"` in `last_action`.
/// Test: the underlying client methods are unit-tested for construction;
/// selection/empty handling is covered by `dashboard` tests.
async fn handle_action(state: &mut DashboardState, client: &DaemonClient, action: Action) {
    let Some(target) = state.selected_target() else {
        state.last_action = Some("no sessions".to_string());
        return;
    };
    let result = match action {
        Action::Pause => client
            .pause_session(&target)
            .await
            .map(|summary| format!("[p] paused {target}: {summary}")),
        Action::Resume => client
            .resume_session(&target)
            .await
            .map(|()| format!("[r] resumed {target}")),
        Action::Stop => client
            .stop_session(&target)
            .await
            .map(|()| format!("[x] stopped {target}")),
        Action::Output => client
            .session_output(&target, 30)
            .await
            .map(|out| format!("[o] {} lines captured", out.lines().count())),
    };
    state.last_action = Some(match result {
        Ok(msg) => msg,
        Err(e) => format!("error: {e}"),
    });
}
