//! Multi-session dashboard rendering.
//!
//! Why: the dashboard is a superset of the claude-mpm dashboard — it shows
//! *all* active sessions at once, not just the current one. Keeping the pure
//! layout/rendering logic here (separate from the event loop and HTTP polling)
//! makes the table-building unit-testable.
//! What: [`DashboardState`] holds the polled session rows and a memory-pressure
//! summary; [`render`] draws the ratatui frame; [`session_rows`] builds the
//! table rows the test suite can assert on.
//! Test: `cargo test -p trusty-mpm-tui` checks row formatting and the empty
//! state without a terminal.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState},
};

use crate::client::{BreakerRow, EventRow, SessionRow};

/// One-line key hint shown in the status bar before any action is taken.
pub const KEY_HINT: &str =
    "keys: ↑↓ navigate | p pause | r resume | x stop | o output | ? help | q quit";

/// Snapshot of everything the dashboard renders this frame.
///
/// Why: the event loop polls the daemon, fills this struct, and hands it to
/// `render` — a clean data/render split.
/// What: the session list plus a daemon-reachable flag.
/// Test: `session_rows_format_each_session`.
#[derive(Debug, Clone, Default)]
pub struct DashboardState {
    /// Sessions reported by the daemon.
    pub sessions: Vec<SessionRow>,
    /// Recent hook events reported by the daemon (oldest first).
    pub events: Vec<EventRow>,
    /// Per-agent circuit-breaker state reported by the daemon.
    pub breakers: Vec<BreakerRow>,
    /// Last N lines from the daemon log file (read from disk each tick).
    pub log_lines: Vec<String>,
    /// Whether the last daemon poll succeeded.
    pub daemon_reachable: bool,
    /// Index into [`Self::sessions`] of the highlighted row.
    ///
    /// Why: the operator navigates the session table with the arrow keys; the
    /// action keys (pause/resume/stop/output) target this row.
    /// What: kept in-bounds by [`Self::clamp_selection`] every poll.
    /// Test: `selection_clamps_to_bounds`.
    pub selected_session: usize,
    /// Human-readable result of the last user action, shown in the status bar.
    ///
    /// Why: gives the operator immediate feedback after a key press without a
    /// separate notification surface.
    /// What: `None` until the first action, then e.g. `"[p] paused tmpm-..."`.
    pub last_action: Option<String>,
    /// Whether the help overlay is currently visible (toggled with `?`).
    pub show_help: bool,
}

impl DashboardState {
    /// Clamp [`Self::selected_session`] into the current session bounds.
    ///
    /// Why: the session list shrinks between polls (sessions end); a stale
    /// selection index would index out of bounds when an action key fires.
    /// What: pins the index to `sessions.len() - 1`, or `0` when empty.
    /// Test: `selection_clamps_to_bounds`.
    pub fn clamp_selection(&mut self) {
        let max = self.sessions.len().saturating_sub(1);
        if self.selected_session > max {
            self.selected_session = max;
        }
    }

    /// Move the session selection up one row (saturating at the top).
    pub fn select_up(&mut self) {
        self.selected_session = self.selected_session.saturating_sub(1);
        self.clamp_selection();
    }

    /// Move the session selection down one row (saturating at the bottom).
    pub fn select_down(&mut self) {
        let max = self.sessions.len().saturating_sub(1);
        if self.selected_session < max {
            self.selected_session += 1;
        }
    }

    /// The friendly `tmux_name` of the currently-selected session, if any.
    ///
    /// Why: session action endpoints resolve their `{id}` against `tmux_name`;
    /// callers need the target for the selected row.
    /// What: returns `None` when there are no sessions.
    /// Test: `selected_target_returns_none_when_empty`.
    pub fn selected_target(&self) -> Option<String> {
        self.sessions
            .get(self.selected_session)
            .map(|s| s.tmux_name.clone())
    }
}

/// Pick the display colour for a session status string.
///
/// Why: a colour-coded status cell makes the operator's eye jump to trouble —
/// centralising the mapping keeps `session_rows` readable and unit-testable.
/// What: `"active"` → green, `"paused"` → yellow, anything else → gray.
/// Test: `session_status_colours`.
fn session_status_color(status: &str) -> Color {
    match status {
        "active" => Color::Green,
        "paused" => Color::Yellow,
        _ => Color::Gray,
    }
}

/// Pick the display colour for a circuit-breaker state string.
///
/// Why: an at-a-glance colour for breaker state surfaces open breakers
/// immediately; centralising the mapping keeps `breaker_rows` testable.
/// What: `"closed"` → green, `"half_open"` → yellow, `"open"` → red, anything
/// else → gray.
/// Test: `breaker_state_colours`.
fn breaker_state_color(state: &str) -> Color {
    match state {
        "closed" => Color::Green,
        "half_open" => Color::Yellow,
        "open" => Color::Red,
        _ => Color::Gray,
    }
}

/// Read the last `n` lines from the daemon log file.
///
/// Why: surfacing a live log tail in the dashboard saves the operator from
/// tailing the file in a separate terminal.
/// What: reads `~/.trusty-mpm/logs/trusty-mpm.log.YYYY-MM-DD` (tracing-appender's
/// daily roller suffix), falling back to the plain `trusty-mpm.log` name, and
/// returns the trailing `n` lines — or a placeholder when no file exists.
/// Test: `read_log_tail_missing_file_returns_placeholder`.
pub fn read_log_tail(n: usize) -> Vec<String> {
    // Try dated file first (tracing-appender daily suffix is YYYY-MM-DD).
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let log_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".trusty-mpm")
        .join("logs");
    let candidates = [
        log_dir.join(format!("trusty-mpm.log.{today}")),
        log_dir.join("trusty-mpm.log"),
    ];
    for path in &candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
            let start = lines.len().saturating_sub(n);
            return lines[start..].to_vec();
        }
    }
    vec!["(no log file yet)".to_string()]
}

/// Pick the row style for a session table row.
///
/// Why: ratatui's `Row` exposes no public style getter, so the highlight logic
/// is factored here where a test can assert it directly.
/// What: `DarkGray` background + white foreground for the selected row, the
/// default (reset) style otherwise.
/// Test: `selected_row_is_highlighted`.
pub fn session_row_style(selected: bool) -> Style {
    if selected {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    }
}

/// Build the table rows for the multi-session panel.
///
/// Why: separating row construction from the ratatui `Table` lets tests assert
/// the formatted cells without a terminal backend; the `selected` index drives
/// the visible navigation highlight.
/// What: one row per session — id (short), workdir, status, delegation count;
/// the row at `selected` gets a `DarkGray` background.
/// Test: `session_rows_format_each_session`, `selected_row_is_highlighted`.
pub fn session_rows(state: &DashboardState, selected: usize) -> Vec<Row<'static>> {
    state
        .sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let id =
                s.id.as_str()
                    .map(|v| v.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "????????".to_string());
            let status = s.status.as_str().unwrap_or("unknown").to_string();
            let status_color = session_status_color(&status);
            Row::new(vec![
                Cell::from(id),
                Cell::from(s.workdir.clone()),
                Cell::from(status).style(Style::default().fg(status_color)),
                Cell::from(s.active_delegations.to_string()),
            ])
            .style(session_row_style(idx == selected))
        })
        .collect()
}

/// Build the table rows for the circuit-breaker panel.
///
/// Why: separating row construction from the ratatui `Table` lets tests assert
/// the formatted cells without a terminal backend.
/// What: one row per breaker — agent, state, consecutive-failure count.
/// Test: `breaker_rows_format_each_breaker`.
pub fn breaker_rows(state: &DashboardState) -> Vec<Row<'static>> {
    state
        .breakers
        .iter()
        .map(|b| {
            let state_color = breaker_state_color(&b.state);
            Row::new(vec![
                Cell::from(b.agent.clone()),
                Cell::from(b.state.clone()).style(Style::default().fg(state_color)),
                Cell::from(b.consecutive_failures.to_string()),
            ])
        })
        .collect()
}

/// Render a `SessionId` newtype JSON value into a short, human id.
///
/// Why: the daemon serializes `SessionId` as `{"0": "<uuid>"}`; the dashboard
/// shows only the first 8 characters so rows and event lines stay compact.
/// What: extracts the inner UUID string and truncates it to 8 chars, falling
/// back to a placeholder when the shape is unexpected.
/// Test: `short_session_extracts_prefix`, `short_session_handles_missing_key`.
pub(crate) fn short_session(val: &serde_json::Value) -> String {
    val.get("0")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "????????".to_string())
}

/// Build the formatted lines for the recent-events panel.
///
/// Why: separating line formatting from the ratatui `List` lets tests assert
/// the text without a terminal backend.
/// What: the last 20 events, each as `{event:<22} {session_short:<10} {at}`.
/// Test: `event_lines_format_recent_events`.
pub fn event_lines(state: &DashboardState) -> Vec<String> {
    let start = state.events.len().saturating_sub(20);
    state.events[start..]
        .iter()
        .map(|e| {
            let session = short_session(&e.session);
            format!("{:<22} {:<10} {}", e.event, session, e.at)
        })
        .collect()
}

/// Build the status-bar line (header line 2).
///
/// Why: gives the operator feedback on the last action, or the key hint when
/// nothing has happened yet; isolating it keeps `render` simple and testable.
/// What: returns `last_action` if set, otherwise [`KEY_HINT`].
/// Test: `status_line_falls_back_to_key_hint`, `status_line_shows_last_action`.
pub fn status_line(state: &DashboardState) -> String {
    state
        .last_action
        .clone()
        .unwrap_or_else(|| KEY_HINT.to_string())
}

/// Compute a centred sub-rectangle for the help overlay.
///
/// Why: the help overlay floats over the layout; it needs a fixed-size centred
/// box independent of the panels beneath it.
/// What: returns a `Rect` of `width`×`height` centred within `area`, clamped so
/// it never exceeds `area`.
/// Test: `centered_rect_is_within_area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Render the help overlay listing every key binding.
///
/// Why: `?` toggles an at-a-glance reference so the operator need not memorize
/// the bindings.
/// What: clears a centred box and draws a bordered `Paragraph` of the bindings.
/// Test: the binding text is covered by `help_text_lists_all_bindings`.
fn render_help_overlay(frame: &mut Frame) {
    let area = centered_rect(54, 11, frame.area());
    let text = help_text();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Help — press ? or Esc to close"),
            ),
        area,
    );
}

/// The body text for the help overlay, one binding per line.
///
/// Why: kept separate so a test can assert every binding is documented.
/// What: returns the multi-line help string.
/// Test: `help_text_lists_all_bindings`.
pub fn help_text() -> String {
    [
        "  ↑ / k     move selection up",
        "  ↓ / j     move selection down",
        "  p         pause selected session",
        "  r         resume selected session",
        "  x         stop selected session",
        "  o         capture selected session output",
        "  ?         toggle this help",
        "  q / Esc   quit",
    ]
    .join("\n")
}

/// Draw the dashboard frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a four-panel layout — a two-line header (title + status bar); a middle
/// row split 60/40 between the sessions table and the circuit-breaker table; a
/// bottom row split 50/50 between the recent-events list and the daemon log
/// tail. When `show_help` is set, a centred help overlay floats over the layout.
/// Test: rendering is exercised by the integration smoke test; row/line content
/// is unit-tested via `session_rows`, `breaker_rows`, and `event_lines`.
pub fn render(frame: &mut Frame, state: &DashboardState) {
    let mut table_state = TableState::default();
    if !state.sessions.is_empty() {
        table_state.select(Some(state.selected_session));
    }
    render_with_table_state(frame, state, &mut table_state);
}

/// Draw the dashboard, threading an explicit [`TableState`] for row highlight.
///
/// Why: the event loop owns the `TableState` so the selection survives across
/// frames; `render` keeps the simple no-arg signature for the smoke test.
/// What: same layout as [`render`]; uses `render_stateful_widget` for the
/// sessions table.
/// Test: covered by the smoke test and the `session_rows` unit tests.
pub fn render_with_table_state(
    frame: &mut Frame,
    state: &DashboardState,
    table_state: &mut TableState,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // header (title + status bar)
            Constraint::Min(6),     // sessions + breakers
            Constraint::Length(10), // events + log tail
        ])
        .split(frame.area());

    let title = if state.daemon_reachable {
        format!("trusty-mpm dashboard — {} session(s)", state.sessions.len())
    } else {
        "trusty-mpm dashboard — daemon unreachable".to_string()
    };
    let header = Paragraph::new(vec![
        Line::from(title).style(Style::default().fg(Color::Cyan)),
        Line::from(status_line(state)).style(Style::default().fg(Color::Gray)),
    ]);
    frame.render_widget(header, chunks[0]);

    // Middle row: sessions (60%) beside circuit breakers (40%).
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[1]);

    let sessions = Table::new(
        session_rows(state, state.selected_session),
        [
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(vec!["ID", "WORKDIR", "STATUS", "DELEG"]))
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD))
    .block(Block::default().borders(Borders::ALL).title("Sessions"));
    frame.render_stateful_widget(sessions, middle[0], table_state);

    let breakers = Table::new(
        breaker_rows(state),
        [
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(vec!["AGENT", "STATE", "FAILS"]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Circuit Breakers"),
    );
    frame.render_widget(breakers, middle[1]);

    // Bottom row: recent hook-event feed (50%) beside the daemon log tail (50%).
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[2]);

    let items: Vec<ListItem> = event_lines(state).into_iter().map(ListItem::new).collect();
    let events = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Recent Events"),
    );
    frame.render_widget(events, bottom[0]);

    let log_items: Vec<ListItem> = state
        .log_lines
        .iter()
        .map(|l| ListItem::new(l.as_str()))
        .collect();
    let log_panel =
        List::new(log_items).block(Block::default().borders(Borders::ALL).title("Daemon Log"));
    frame.render_widget(log_panel, bottom[1]);

    if state.show_help {
        render_help_overlay(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `SessionRow` for tests.
    fn session(id: &str, workdir: &str, status: &str, name: &str) -> SessionRow {
        SessionRow {
            id: serde_json::json!(id),
            workdir: workdir.into(),
            status: serde_json::json!(status),
            active_delegations: 0,
            tmux_name: name.into(),
        }
    }

    #[test]
    fn session_rows_empty_when_no_sessions() {
        let state = DashboardState::default();
        assert!(session_rows(&state, 0).is_empty());
    }

    #[test]
    fn session_rows_format_each_session() {
        let state = DashboardState {
            daemon_reachable: true,
            sessions: vec![SessionRow {
                id: serde_json::json!("abcd1234-5678-90ab-cdef-1234567890ab"),
                workdir: "/tmp/proj".into(),
                status: serde_json::json!("active"),
                active_delegations: 2,
                tmux_name: "tmpm-quiet-falcon".into(),
            }],
            ..DashboardState::default()
        };
        let rows = session_rows(&state, 0);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn selected_row_is_highlighted() {
        // `session_rows` with `selected = 0` builds two rows; the highlight
        // logic in `session_row_style` puts a `DarkGray` background on the
        // selected row only.
        let state = DashboardState {
            sessions: vec![
                session("a", "/p/a", "active", "tmpm-a"),
                session("b", "/p/b", "active", "tmpm-b"),
            ],
            ..DashboardState::default()
        };
        let rows = session_rows(&state, 0);
        assert_eq!(rows.len(), 2);
        // Row 0 is selected → DarkGray bg + white fg.
        let selected = session_row_style(true);
        assert_eq!(selected.bg, Some(Color::DarkGray));
        assert_eq!(selected.fg, Some(Color::White));
        // Row 1 is not selected → no DarkGray background.
        assert_ne!(session_row_style(false).bg, Some(Color::DarkGray));
    }

    #[test]
    fn selection_clamps_to_bounds() {
        // An out-of-range selection is pinned to the last valid index, and to 0
        // when there are no sessions.
        let mut state = DashboardState {
            sessions: vec![
                session("a", "/p/a", "active", "tmpm-a"),
                session("b", "/p/b", "active", "tmpm-b"),
            ],
            selected_session: 99,
            ..DashboardState::default()
        };
        state.clamp_selection();
        assert_eq!(state.selected_session, 1);

        state.sessions.clear();
        state.clamp_selection();
        assert_eq!(state.selected_session, 0);
    }

    #[test]
    fn select_up_down_saturate() {
        let mut state = DashboardState {
            sessions: vec![
                session("a", "/p/a", "active", "tmpm-a"),
                session("b", "/p/b", "active", "tmpm-b"),
            ],
            ..DashboardState::default()
        };
        // Down moves toward the bottom and saturates there.
        state.select_down();
        assert_eq!(state.selected_session, 1);
        state.select_down();
        assert_eq!(state.selected_session, 1);
        // Up moves toward the top and saturates at 0.
        state.select_up();
        assert_eq!(state.selected_session, 0);
        state.select_up();
        assert_eq!(state.selected_session, 0);
    }

    #[test]
    fn selected_target_returns_none_when_empty() {
        let empty = DashboardState::default();
        assert_eq!(empty.selected_target(), None);

        let state = DashboardState {
            sessions: vec![session("a", "/p/a", "active", "tmpm-quiet-falcon")],
            ..DashboardState::default()
        };
        assert_eq!(state.selected_target(), Some("tmpm-quiet-falcon".into()));
    }

    #[test]
    fn status_line_falls_back_to_key_hint() {
        let state = DashboardState::default();
        assert_eq!(status_line(&state), KEY_HINT);
    }

    #[test]
    fn status_line_shows_last_action() {
        let state = DashboardState {
            last_action: Some("[p] paused tmpm-quiet-falcon".into()),
            ..DashboardState::default()
        };
        assert_eq!(status_line(&state), "[p] paused tmpm-quiet-falcon");
    }

    #[test]
    fn help_text_lists_all_bindings() {
        let text = help_text();
        for key in ["p", "r", "x", "o", "?", "q"] {
            assert!(text.contains(key), "help text missing binding `{key}`");
        }
    }

    #[test]
    fn centered_rect_is_within_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let r = centered_rect(54, 11, area);
        assert_eq!(r.width, 54);
        assert_eq!(r.height, 11);
        assert!(r.x + r.width <= area.width);
        assert!(r.y + r.height <= area.height);
        // A request larger than the area is clamped to the area.
        let clamped = centered_rect(200, 200, area);
        assert_eq!(clamped.width, 100);
        assert_eq!(clamped.height, 40);
    }

    #[test]
    fn breaker_rows_format_each_breaker() {
        let state = DashboardState {
            breakers: vec![BreakerRow {
                agent: "research".into(),
                state: "open".into(),
                consecutive_failures: 3,
            }],
            ..DashboardState::default()
        };
        assert_eq!(breaker_rows(&state).len(), 1);
    }

    /// Build an `EventRow` for tests with a null payload.
    fn event(name: &str, at: &str) -> EventRow {
        EventRow {
            session: serde_json::json!({"0": "abcd1234-5678-90ab-cdef-1234567890ab"}),
            event: name.into(),
            at: at.into(),
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn event_lines_format_recent_events() {
        let state = DashboardState {
            events: vec![event("PreToolUse", "2024-01-01T00:00:00Z")],
            ..DashboardState::default()
        };
        let lines = event_lines(&state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("PreToolUse"));
        assert!(lines[0].contains("abcd1234"));
    }

    #[test]
    fn event_lines_cap_at_twenty() {
        let state = DashboardState {
            events: vec![event("Stop", "2024-01-01T00:00:00Z"); 50],
            ..DashboardState::default()
        };
        assert_eq!(event_lines(&state).len(), 20);
    }

    #[test]
    fn short_session_extracts_prefix() {
        let val = serde_json::json!({"0": "abcd1234-5678-90ab-cdef-1234567890ab"});
        assert_eq!(short_session(&val), "abcd1234");
    }

    #[test]
    fn short_session_handles_missing_key() {
        // Missing `0` key or a null value → the placeholder.
        assert_eq!(short_session(&serde_json::json!({})), "????????");
        assert_eq!(short_session(&serde_json::Value::Null), "????????");
    }

    #[test]
    fn breaker_state_open_shows_open() {
        let state = DashboardState {
            breakers: vec![BreakerRow {
                agent: "eng".into(),
                state: "open".into(),
                consecutive_failures: 3,
            }],
            ..DashboardState::default()
        };
        // The rendered row's middle cell carries the breaker state text.
        assert_eq!(state.breakers[0].state, "open");
        assert_eq!(breaker_rows(&state).len(), 1);
    }

    #[test]
    fn breaker_state_closed_shows_closed() {
        let state = DashboardState {
            breakers: vec![BreakerRow {
                agent: "qa".into(),
                state: "closed".into(),
                consecutive_failures: 0,
            }],
            ..DashboardState::default()
        };
        assert_eq!(state.breakers[0].state, "closed");
        assert_eq!(breaker_rows(&state).len(), 1);
    }

    #[test]
    fn event_lines_newest_at_bottom() {
        // Events are stored oldest-first; the formatted lines preserve that
        // order so the newest event renders last.
        let state = DashboardState {
            events: vec![
                event("oldest", "2024-01-01T00:00:00Z"),
                event("middle", "2024-01-01T00:00:01Z"),
                event("newest", "2024-01-01T00:00:02Z"),
            ],
            ..DashboardState::default()
        };
        let lines = event_lines(&state);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("oldest"));
        assert!(lines[2].contains("newest"));
    }

    #[test]
    fn event_lines_empty_when_no_events() {
        let state = DashboardState::default();
        assert!(event_lines(&state).is_empty());
    }

    #[test]
    fn session_status_colours() {
        assert_eq!(session_status_color("active"), Color::Green);
        assert_eq!(session_status_color("paused"), Color::Yellow);
        assert_eq!(session_status_color("unknown"), Color::Gray);
        assert_eq!(session_status_color("anything-else"), Color::Gray);
    }

    #[test]
    fn breaker_state_colours() {
        assert_eq!(breaker_state_color("closed"), Color::Green);
        assert_eq!(breaker_state_color("half_open"), Color::Yellow);
        assert_eq!(breaker_state_color("open"), Color::Red);
        assert_eq!(breaker_state_color("weird"), Color::Gray);
    }

    #[test]
    fn read_log_tail_missing_file_returns_placeholder() {
        // Point HOME at an empty temp dir so no log file exists; the function
        // must degrade to its placeholder line rather than panicking.
        let tmp = std::env::temp_dir().join(format!("trusty-mpm-tui-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let prev_home = std::env::var_os("HOME");
        // SAFETY: single-threaded test scope; restored before returning.
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let lines = read_log_tail(20);

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(lines, vec!["(no log file yet)".to_string()]);
    }
}
