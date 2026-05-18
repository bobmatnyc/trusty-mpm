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
pub mod iterm2;

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
    run_focused(url, interval_ms, None).await
}

/// Re-resolve the daemon URL from the lock file when the daemon is unreachable.
///
/// Why: `DaemonClient` is built once at startup from a URL resolved at that
/// instant. If the TUI launched before the daemon wrote its lock file, or the
/// daemon later restarted onto a fresh ephemeral port, the client would stay
/// pinned to a stale address and report "daemon unreachable" forever even
/// though a daemon is live. Re-resolving on every failed poll lets the TUI
/// self-heal and follow the daemon to its current address.
/// What: when `reachable` is `false`, calls [`trusty_mpm_core::resolve_daemon_url`]
/// (lock file → default) and, if it yields a URL different from the client's
/// current base, re-points the client and returns `true` so the caller can
/// re-poll immediately. A no-op (returns `false`) while the daemon is reachable.
/// Test: `rediscover_repoints_when_lockfile_changes`.
fn rediscover_daemon(client: &mut DaemonClient, reachable: bool) -> bool {
    if reachable {
        return false;
    }
    let resolved = trusty_mpm_core::resolve_daemon_url(None);
    if resolved != client.base_url() {
        client.set_base_url(resolved);
        true
    } else {
        false
    }
}

/// Run the dashboard pre-focused on a specific session.
///
/// Why: `tm connect <target>` resolves a fuzzy target to a definitive session
/// id and wants the TUI to open with that session already highlighted, so the
/// operator lands directly on the right row.
/// What: same terminal setup/teardown as [`run`], but threads `focus_id` into
/// [`run_loop`], which selects the matching session right after the priming
/// poll. A `None` focus behaves exactly like the plain `tui` subcommand.
/// Test: focus selection is unit-tested via [`dashboard::DashboardState::focus_on`];
/// the terminal glue is exercised by launching the dashboard.
pub async fn run_focused(
    url: String,
    interval_ms: u64,
    focus_id: Option<String>,
) -> anyhow::Result<()> {
    let mut client = DaemonClient::new(url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut client, interval_ms, focus_id).await;

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
/// session selection so a shrunken list never leaves a stale index. When the
/// daemon is unreachable it re-resolves the daemon URL from the lock file via
/// [`rediscover_daemon`] and retries one health probe, so the TUI follows the
/// daemon to a new ephemeral port after a restart instead of being stuck.
/// Test: the pure pieces (rendering, client, clamping, rediscovery) are
/// unit-tested.
async fn poll_daemon(state: &mut DashboardState, client: &mut DaemonClient) {
    state.daemon_reachable = client.is_healthy().await;
    // Self-heal: if the daemon looks unreachable, the lock file may now point
    // at a different (ephemeral) port — re-resolve and retry one probe.
    if rediscover_daemon(client, state.daemon_reachable) {
        state.daemon_reachable = client.is_healthy().await;
    }
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
    // Refresh the focused session's detail panel from its tmux pane snapshot;
    // a `None` result (no tmux / unknown session) leaves `session_output` empty
    // so the panel falls back to the session's recent hook events.
    refresh_session_output(state, client).await;
}

/// Refresh the focused session's detail-panel output from its tmux snapshot.
///
/// Why: selecting a session opens a "Session Output / History" panel that must
/// refresh every poll; tmux-origin sessions show a live pane snapshot.
/// What: when a session is focused and reachable, captures its tmux pane via
/// `GET /tmux/sessions/{name}/snapshot` and stores the lines in
/// `session_output`; clears `session_output` when no session is focused or the
/// snapshot is unavailable (so the panel falls back to hook events).
/// Test: the panel formatting is covered by `session_output_panel_lines_*`;
/// this glue is exercised by launching the dashboard.
async fn refresh_session_output(state: &mut DashboardState, client: &DaemonClient) {
    let Some(focused) = state.active_session.clone() else {
        state.session_output.clear();
        return;
    };
    if !state.daemon_reachable {
        state.session_output.clear();
        return;
    }
    match client.snapshot_tmux_session(&focused).await {
        Ok(Some(snapshot)) => {
            state.session_output = snapshot.lines().map(str::to_string).collect();
        }
        // No tmux pane (native-origin session) or transport failure: clear so
        // the panel falls back to the session's recent hook events.
        Ok(None) | Err(_) => state.session_output.clear(),
    }
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
    client: &mut DaemonClient,
    interval_ms: u64,
    focus_id: Option<String>,
) -> anyhow::Result<()> {
    // Why: iTerm2 detection is a stable environment property — probe it once at
    // startup rather than every key press.
    // What: store the result so the `o` key handler can branch on it.
    // Test: detection itself is covered by `iterm2` unit tests.
    let mut state = DashboardState {
        iterm2_mode: iterm2::is_iterm2(),
        ..DashboardState::default()
    };
    let mut table_state = TableState::default();

    // Prime the dashboard with one poll before the first render.
    poll_daemon(&mut state, client).await;
    // Why: `tm connect` resolves a session before the TUI opens; apply the
    // requested focus only after the priming poll has populated the list.
    // What: select the matching row and note it in the status bar.
    // Test: `dashboard::DashboardState::focus_on` covers the selection logic.
    if let Some(id) = focus_id.as_deref()
        && state.focus_on(id)
    {
        state.last_action = Some(format!("Connected to {id}"));
    }
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
            // Why: while the command bar is active it owns every keystroke, so
            // typing a command never triggers navigation or action keys.
            // What: Enter dispatches the typed command, Esc deactivates the bar,
            // Tab autocompletes, ↑/↓ recall history, Backspace edits, and
            // printable characters append to the buffer.
            // Test: `dispatch_command` covers command dispatch; `CommandBar`
            // unit tests cover editing, autocomplete, and history.
            if state.command_bar.active {
                match key.code {
                    KeyCode::Esc => state.command_bar.deactivate(),
                    KeyCode::Enter => {
                        let typed = state.command_bar.take_for_execution();
                        dispatch_command(&mut state, client, &typed).await;
                        // `/exit` and `/quit` raise `should_exit` — honour it
                        // here, exactly like pressing `q`.
                        if state.should_exit {
                            return Ok(());
                        }
                        poll_daemon(&mut state, client).await;
                        last_poll = Instant::now();
                    }
                    KeyCode::Tab => state.command_bar.autocomplete(),
                    KeyCode::Up => state.command_bar.history_prev(),
                    KeyCode::Down => state.command_bar.history_next(),
                    KeyCode::Backspace => state.command_bar.backspace(),
                    KeyCode::Char(c) => state.command_bar.push(c),
                    _ => {}
                }
                continue;
            }
            match key.code {
                // `q` always quits. `Esc` first closes the help overlay, then
                // deselects a focused session, and only quits when neither
                // applies — so it never quits out from under an open panel.
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Esc if state.show_help => state.show_help = false,
                KeyCode::Esc if state.active_session.is_some() => {
                    state.clear_active_session();
                    state.last_action = Some("session deselected".to_string());
                }
                KeyCode::Esc => return Ok(()),
                KeyCode::Char('?') => state.show_help = !state.show_help,
                // `:` and `/` both activate the persistent command bar.
                KeyCode::Char(':') | KeyCode::Char('/') => state.command_bar.activate(),
                KeyCode::Up | KeyCode::Char('k') => state.select_up(),
                KeyCode::Down | KeyCode::Char('j') => state.select_down(),
                KeyCode::Enter => {
                    // Enter on a highlighted session row focuses it for the
                    // command bar's summarized-chat mode and opens its
                    // detail panel — populated immediately, not next tick.
                    match state.set_active_session() {
                        Some(name) => {
                            state.last_action = Some(format!("Focused session: {name}"));
                            refresh_session_output(&mut state, client).await;
                        }
                        None => state.last_action = Some("no sessions to focus".to_string()),
                    }
                }
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
                    // Why: `o` opens the selected session in a new iTerm2 tab —
                    // ergonomic only inside iTerm2, so branch on the startup probe.
                    // What: in iTerm2 mode, resolve the selected target and run the
                    // AppleScript launcher; otherwise show the fallback hint.
                    // Test: detection is covered by `iterm2` unit tests; the
                    // branch/message logic by `handle_open_in_iterm2` tests.
                    handle_open_in_iterm2(&mut state);
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
}

/// Open the selected session in a new iTerm2 tab (the `o` key).
///
/// Why: opening a session as a sibling iTerm2 tab lets the operator keep the
/// dashboard visible while the session runs — but that only works inside
/// iTerm2, so the handler branches on the startup detection result.
/// What: in iTerm2 mode, resolves the selected `tmux_name` and runs the
/// AppleScript launcher, recording a success or `"iTerm2 error: <msg>"` line;
/// outside iTerm2 (or with no sessions) it records the appropriate hint.
/// Test: `open_in_iterm2_*` cases below cover every branch (no live iTerm2
/// needed since `open_session_tab` is only reached in iTerm2 mode).
fn handle_open_in_iterm2(state: &mut DashboardState) {
    if !state.iterm2_mode {
        state.last_action = Some("iTerm2 not detected — use your terminal to attach".to_string());
        return;
    }
    let Some(target) = state.selected_target() else {
        state.last_action = Some("no sessions".to_string());
        return;
    };
    state.last_action = Some(match iterm2::open_session_tab(&target) {
        Ok(()) => "Opening in iTerm2 tab…".to_string(),
        Err(e) => format!("iTerm2 error: {e}"),
    });
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
    };
    state.last_action = Some(match result {
        Ok(msg) => msg,
        Err(e) => format!("error: {e}"),
    });
}

/// Dispatch a slash command typed into the persistent command bar.
///
/// Why: the command bar is the single surface for every slash command; this is
/// the one place that maps a typed verb to a daemon call so adding a command
/// later is one match arm.
/// What: splits `typed` into a verb (normalized — leading `/` stripped, trimmed,
/// lowercased) and an argument tail, runs the matching daemon call, and writes
/// the result lines into the command bar's output panel. An empty command is a
/// no-op; an unknown command writes an error line.
/// Test: `cargo test -p trusty-mpm-tui` covers normalization, the empty/unknown
/// arms, and the output-panel writes; live HTTP is covered by the daemon tests.
async fn dispatch_command(state: &mut DashboardState, client: &DaemonClient, typed: &str) {
    let trimmed = typed.trim();
    if trimmed.is_empty() {
        return;
    }
    // Plain text (no leading `/`) is not a command — it routes to the focused
    // session's summarized-chat mode rather than the slash-command dispatch.
    if !trimmed.starts_with('/') {
        let lines = session_chat(state, client, trimmed).await;
        state.command_bar.set_output(lines);
        return;
    }
    // Split into verb + argument tail; normalize only the verb.
    let no_slash = trimmed.trim_start_matches('/').trim_start();
    let (verb, arg) = match no_slash.split_once(char::is_whitespace) {
        Some((v, rest)) => (v.trim().to_lowercase(), rest.trim().to_string()),
        None => (no_slash.trim().to_lowercase(), String::new()),
    };

    let lines: Vec<String> = match verb.as_str() {
        "" => return,
        "help" => dashboard::command_help_lines(),
        "exit" | "quit" => {
            // `/exit` and `/quit` leave the dashboard, exactly like the `q`
            // key; the dispatcher can't return from the loop, so it raises a
            // flag the event loop checks after dispatch.
            state.should_exit = true;
            vec!["Exiting…".to_string()]
        }
        "discover" => match client.discover_sessions().await {
            Ok(0) => vec!["discover: no new Claude Code sessions found".to_string()],
            Ok(n) => vec![format!(
                "discover: adopted {n} tmux session(s) running Claude Code"
            )],
            Err(e) => vec![format!("discover: daemon error: {e}")],
        },
        "pair" => match client.pair_request().await {
            Ok(req) => {
                let minutes = req.expires_in_seconds / 60;
                vec![
                    format!(
                        "Telegram pairing code: {}  (expires in {minutes} min)",
                        req.code
                    ),
                    format!("Send /pair {} to your Telegram bot", req.code),
                ]
            }
            Err(e) => vec![format!("pair: daemon error: {e}")],
        },
        "projects" => match client.discover_projects().await {
            Ok(projects) if projects.is_empty() => vec!["projects: none discovered".to_string()],
            Ok(projects) => std::iter::once("Discovered projects:".to_string())
                .chain(
                    projects
                        .iter()
                        .map(|p| format!("  {}  ({} session(s))", p.path, p.session_count)),
                )
                .collect(),
            Err(e) => vec![format!("projects: daemon error: {e}")],
        },
        "sessions" => match client.sessions().await {
            Ok(sessions) if sessions.is_empty() => vec!["sessions: none active".to_string()],
            Ok(sessions) => std::iter::once("Daemon sessions:".to_string())
                .chain(sessions.iter().map(|s| {
                    format!(
                        "  {}  {}  {:?}",
                        if s.tmux_name.is_empty() {
                            dashboard::short_session(&s.id)
                        } else {
                            s.tmux_name.clone()
                        },
                        s.workdir,
                        s.status,
                    )
                }))
                .collect(),
            Err(e) => vec![format!("sessions: daemon error: {e}")],
        },
        "tmux" => match client.tmux_sessions().await {
            Ok(rows) if rows.is_empty() => vec!["tmux: no sessions".to_string()],
            Ok(rows) => std::iter::once("tmux sessions:".to_string())
                .chain(rows.iter().map(|r| {
                    let tag = if r.managed { "managed" } else { "external" };
                    format!("  {}  [{tag}]", r.name)
                }))
                .collect(),
            Err(e) => vec![format!("tmux: daemon error: {e}")],
        },
        "status" => {
            if client.is_healthy().await {
                match client.sessions().await {
                    Ok(sessions) => vec![format!("daemon: ok  ({} session(s))", sessions.len())],
                    Err(e) => vec![format!("daemon: ok, but sessions query failed: {e}")],
                }
            } else {
                vec!["daemon: unreachable".to_string()]
            }
        }
        "adopt" => {
            if arg.is_empty() {
                vec!["adopt: usage: /adopt <tmux-session-name>".to_string()]
            } else {
                match client.adopt_tmux_session(&arg).await {
                    Ok(true) => vec![format!("adopted tmux session: {arg}")],
                    Ok(false) => vec![format!("adopt: session not found: {arg}")],
                    Err(e) => vec![format!("adopt: daemon error: {e}")],
                }
            }
        }
        "connect" => {
            if arg.is_empty() {
                vec!["connect: usage: /connect <id|dir>".to_string()]
            } else {
                match state.connect_action(&arg) {
                    dashboard::ConnectAction::Resolved(msg) => vec![msg],
                    // A directory with no existing session — launch one. The
                    // daemon registers state; this client owns the tmux launch.
                    dashboard::ConnectAction::Launch(dir) => {
                        match client.launch_session(&dir).await {
                            Ok(name) => {
                                vec![format!("Launched claude-mpm session {name} in {dir}")]
                            }
                            Err(e) => vec![format!("connect: launch failed: {e}")],
                        }
                    }
                }
            }
        }
        "chat" => {
            if arg.is_empty() {
                vec!["chat: usage: /chat <message>".to_string()]
            } else {
                match client.llm_chat(&arg, &state.chat_history).await {
                    Ok(Some(outcome)) => {
                        state.chat_history = outcome.history;
                        outcome.reply.lines().map(str::to_string).collect()
                    }
                    Ok(None) => vec![
                        "chat: LLM not configured — set OPENROUTER_API_KEY and enable the overseer"
                            .to_string(),
                    ],
                    Err(e) => vec![format!("chat: daemon error: {e}")],
                }
            }
        }
        "send" => {
            // `/send <session> <prompt>`: first token is the session, the
            // remainder is the (possibly multi-word) prompt.
            match arg.split_once(char::is_whitespace) {
                Some((session, prompt)) if !prompt.trim().is_empty() => {
                    match client.sessions().await {
                        Ok(rows) => {
                            match rows.iter().find(|r| {
                                r.id.0.to_string() == session
                                    || r.tmux_name == session
                                    || (!r.tmux_name.is_empty() && r.tmux_name.starts_with(session))
                            }) {
                                Some(row) => {
                                    let target = if row.tmux_name.is_empty() {
                                        row.id.0.to_string()
                                    } else {
                                        row.tmux_name.clone()
                                    };
                                    match client.send_session_command(&target, prompt.trim()).await
                                    {
                                        Ok(Some(output)) => {
                                            std::iter::once(format!("sent to {target}:"))
                                                .chain(output.lines().map(str::to_string))
                                                .collect()
                                        }
                                        Ok(None) => {
                                            vec![format!("send: session {session} not found")]
                                        }
                                        Err(e) => vec![format!("send: daemon error: {e}")],
                                    }
                                }
                                None => vec![format!("send: session {session} not found")],
                            }
                        }
                        Err(e) => vec![format!("send: daemon error: {e}")],
                    }
                }
                _ => vec!["send: usage: /send <session> <prompt>".to_string()],
            }
        }
        other => vec![format!("unknown command: /{other}  (try /help)")],
    };
    state.command_bar.set_output(lines);
}

/// The prompt prefix used to summarize Claude Code session output.
///
/// Why: the summarized-chat mode asks the LLM for a tight 2-3 sentence digest
/// of the pane output; keeping the instruction in one constant means the chat
/// and the test assert against the same text.
const SUMMARY_PROMPT: &str = "Summarize this Claude Code session output in 2-3 sentences, focusing on what \
     was accomplished or any errors:\n\n";

/// Route plain CMD-bar text to the focused session, then summarize the output.
///
/// Why: the summarized-chat mode lets the operator drive the focused Claude
/// Code session with plain text — the message is sent to the session and the
/// raw pane output is condensed by the LLM so the operator reads a digest, not
/// a wall of text.
/// What: requires a focused session (else a hint to `/connect`); sends `message`
/// via `POST /sessions/{id}/command`; if an LLM is configured, asks
/// `POST /llm/chat` to summarize the captured output with [`SUMMARY_PROMPT`] and
/// shows the summary, otherwise shows the raw output. Every daemon failure
/// becomes a renderable line, never a panic.
/// Test: `session_chat_without_focus_hints_connect`,
/// `session_chat_without_daemon_reports_error`.
async fn session_chat(
    state: &mut DashboardState,
    client: &DaemonClient,
    message: &str,
) -> Vec<String> {
    let Some(session) = state.active_session.clone() else {
        return vec!["No session selected — use /connect <id> or select a session".to_string()];
    };
    let output = match client.send_session_command(&session, message).await {
        Ok(Some(output)) => output,
        Ok(None) => return vec![format!("session {session} not found")],
        Err(e) => return vec![format!("session chat: daemon error: {e}")],
    };

    // Summarize the pane output via the LLM overseer when one is configured;
    // otherwise fall back to showing the raw captured output.
    let prompt = format!("{SUMMARY_PROMPT}{output}");
    match client.llm_chat(&prompt, &[]).await {
        Ok(Some(outcome)) => std::iter::once(format!("[session: {session}] summary:"))
            .chain(outcome.reply.lines().map(str::to_string))
            .collect(),
        Ok(None) => std::iter::once(format!(
            "[session: {session}] (LLM not configured — raw output):"
        ))
        .chain(output.lines().map(str::to_string))
        .collect(),
        Err(e) => std::iter::once(format!("session chat: summary failed: {e}"))
            .chain(output.lines().map(str::to_string))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::SessionRow;

    /// Build a `DashboardState` carrying a single session for `o`-key tests.
    fn state_with_session(iterm2_mode: bool) -> DashboardState {
        DashboardState {
            iterm2_mode,
            sessions: vec![SessionRow {
                id: trusty_mpm_core::session::SessionId(uuid::Uuid::nil()),
                workdir: "/tmp/proj".into(),
                status: trusty_mpm_core::session::SessionStatus::Active,
                active_delegations: 0,
                tmux_name: "tmpm-quiet-falcon".into(),
                last_seen: Default::default(),
            }],
            ..DashboardState::default()
        }
    }

    #[test]
    fn open_in_iterm2_without_iterm2_shows_fallback_hint() {
        // Why: non-iTerm2 terminals must get a clear instruction, never a launch.
        // What: with `iterm2_mode = false`, the handler records the fallback hint.
        // Test: assert the exact fallback message regardless of session count.
        let mut state = state_with_session(false);
        handle_open_in_iterm2(&mut state);
        assert_eq!(
            state.last_action.as_deref(),
            Some("iTerm2 not detected — use your terminal to attach"),
        );
    }

    #[test]
    fn open_in_iterm2_with_no_sessions_reports_no_sessions() {
        // Why: pressing `o` with an empty session list must not call osascript.
        // What: iTerm2 mode but zero sessions records `"no sessions"`.
        // Test: empty `sessions` → the `"no sessions"` status line.
        let mut state = DashboardState {
            iterm2_mode: true,
            ..DashboardState::default()
        };
        handle_open_in_iterm2(&mut state);
        assert_eq!(state.last_action.as_deref(), Some("no sessions"));
    }

    #[test]
    fn rediscover_is_noop_when_daemon_reachable() {
        // Why: a reachable daemon must never trigger a URL re-resolution — that
        // would needlessly thrash the client base URL while everything works.
        let mut client = DaemonClient::new("http://127.0.0.1:7880");
        assert!(!rediscover_daemon(&mut client, true));
        assert_eq!(client.base_url(), "http://127.0.0.1:7880");
    }

    #[test]
    fn rediscover_is_noop_when_resolved_url_unchanged() {
        // Why: when the daemon is unreachable but the lock file resolves to the
        // same URL the client already targets, re-pointing is pointless — the
        // function must report "no change" so the caller does not re-poll.
        // What: with no lock file present, `resolve_daemon_url(None)` returns the
        // default; a client already on the default sees no change.
        let mut client = DaemonClient::new(trusty_mpm_core::DEFAULT_DAEMON_URL);
        // If a stale lock file exists on this machine it could resolve elsewhere;
        // either way the function must not panic and must return a bool.
        let changed = rediscover_daemon(&mut client, false);
        if !changed {
            assert_eq!(client.base_url(), trusty_mpm_core::DEFAULT_DAEMON_URL);
        }
    }

    #[tokio::test]
    async fn dispatch_empty_command_is_noop() {
        // Why: pressing Enter on an empty command bar must do nothing.
        // What: an empty (or whitespace-only) command leaves the output panel
        // untouched.
        // Test: no output lines are recorded.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "  ").await;
        assert!(state.command_bar.output.is_empty());
    }

    #[tokio::test]
    async fn dispatch_unknown_command_writes_output() {
        // Why: an unrecognized command must give the operator feedback rather
        // than failing silently.
        // What: `/bogus` writes an `unknown command` line into the output panel.
        // Test: assert the output panel holds the error line.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/bogus").await;
        assert_eq!(state.command_bar.output.len(), 1);
        assert!(state.command_bar.output[0].contains("unknown command: /bogus"));
    }

    #[tokio::test]
    async fn dispatch_help_lists_commands_without_daemon() {
        // Why: `/help` is purely local and must work even with no daemon.
        // What: `/help` writes the command reference into the output panel.
        // Test: assert the output contains every documented command.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/help").await;
        let text = state.command_bar.output.join("\n");
        assert!(text.contains("/pair"));
        assert!(text.contains("/projects"));
        assert!(text.contains("/adopt"));
    }

    #[tokio::test]
    async fn dispatch_pair_command_writes_error_when_daemon_down() {
        // Why: `/pair` against an unreachable daemon must surface the failure in
        // the output panel, not crash or do nothing.
        // What: a port-0 base URL never connects, so `pair_request` errors and
        // `dispatch_command` writes an error line.
        // Test: assert the output panel holds a `pair: daemon error` line.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/pair").await;
        assert!(
            state
                .command_bar
                .output
                .iter()
                .any(|l| l.contains("pair: daemon error"))
        );
    }

    #[tokio::test]
    async fn dispatch_adopt_without_arg_shows_usage() {
        // Why: `/adopt` with no session name must explain its usage, not call
        // the daemon with an empty name.
        // What: `/adopt` writes a usage line into the output panel.
        // Test: assert the usage line is present.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/adopt").await;
        assert!(state.command_bar.output[0].contains("usage"));
    }

    #[tokio::test]
    async fn dispatch_chat_without_arg_shows_usage() {
        // `/chat` with no message must explain its usage, not call the daemon.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/chat").await;
        assert!(state.command_bar.output[0].contains("usage"));
    }

    #[tokio::test]
    async fn dispatch_chat_writes_error_when_daemon_down() {
        // `/chat <msg>` against an unreachable daemon surfaces the failure.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/chat hello there").await;
        assert!(
            state
                .command_bar
                .output
                .iter()
                .any(|l| l.contains("chat: daemon error"))
        );
    }

    #[tokio::test]
    async fn dispatch_send_without_prompt_shows_usage() {
        // `/send <session>` with no prompt must explain its usage.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/send frontend").await;
        assert!(state.command_bar.output[0].contains("usage"));
    }

    #[tokio::test]
    async fn dispatch_exit_sets_should_exit() {
        // `/exit` and `/quit` must raise the loop's exit flag.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/exit").await;
        assert!(state.should_exit);

        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/quit").await;
        assert!(state.should_exit);
    }

    #[tokio::test]
    async fn dispatch_discover_writes_error_when_daemon_down() {
        // `/discover` against an unreachable daemon surfaces the failure.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "/discover").await;
        assert!(
            state
                .command_bar
                .output
                .iter()
                .any(|l| l.contains("discover: daemon error"))
        );
    }

    #[tokio::test]
    async fn session_chat_without_focus_hints_connect() {
        // Plain text with no focused session must hint at /connect.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        dispatch_command(&mut state, &client, "hello there").await;
        assert!(
            state.command_bar.output[0].contains("No session selected"),
            "expected a connect hint, got {:?}",
            state.command_bar.output
        );
    }

    #[tokio::test]
    async fn session_chat_without_daemon_reports_error() {
        // Plain text with a focused session but a dead daemon reports an error.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState {
            active_session: Some("frontend".to_string()),
            ..DashboardState::default()
        };
        dispatch_command(&mut state, &client, "run the tests").await;
        assert!(
            state
                .command_bar
                .output
                .iter()
                .any(|l| l.contains("daemon error")),
            "expected a daemon error, got {:?}",
            state.command_bar.output
        );
    }

    #[tokio::test]
    async fn dispatch_connect_resolves_session() {
        // Why: `/connect <name>` must focus a matching session purely locally.
        // What: with one session named `frontend`, `/connect front` focuses it
        // and writes a `Connected to` line.
        // Test: assert the selection and the output line.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState {
            sessions: vec![SessionRow {
                id: trusty_mpm_core::session::SessionId(uuid::Uuid::nil()),
                workdir: "/tmp/proj".into(),
                status: trusty_mpm_core::session::SessionStatus::Active,
                active_delegations: 0,
                tmux_name: "frontend".into(),
                last_seen: Default::default(),
            }],
            ..DashboardState::default()
        };
        dispatch_command(&mut state, &client, "/connect front").await;
        assert_eq!(state.selected_session, 0);
        assert!(state.command_bar.output[0].contains("Connected to"));
    }
}
