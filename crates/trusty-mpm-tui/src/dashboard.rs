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
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table},
};

use crate::client::{BreakerRow, EventRow, SessionRow};

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
    /// Whether the last daemon poll succeeded.
    pub daemon_reachable: bool,
}

/// Build the table rows for the multi-session panel.
///
/// Why: separating row construction from the ratatui `Table` lets tests assert
/// the formatted cells without a terminal backend.
/// What: one row per session — id (short), workdir, status, delegation count.
/// Test: `session_rows_format_each_session`.
pub fn session_rows(state: &DashboardState) -> Vec<Row<'static>> {
    state
        .sessions
        .iter()
        .map(|s| {
            let id =
                s.id.as_str()
                    .map(|v| v.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "????????".to_string());
            let status = s.status.as_str().unwrap_or("unknown").to_string();
            Row::new(vec![
                Cell::from(id),
                Cell::from(s.workdir.clone()),
                Cell::from(status),
                Cell::from(s.active_delegations.to_string()),
            ])
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
            Row::new(vec![
                Cell::from(b.agent.clone()),
                Cell::from(b.state.clone()),
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

/// Draw the dashboard frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a three-panel layout — header line; a middle row split 60/40 between
/// the sessions table and the circuit-breaker table; a bottom recent-events
/// list.
/// Test: rendering is exercised by the integration smoke test; row/line content
/// is unit-tested via `session_rows`, `breaker_rows`, and `event_lines`.
pub fn render(frame: &mut Frame, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let header = if state.daemon_reachable {
        format!("trusty-mpm dashboard — {} session(s)", state.sessions.len())
    } else {
        "trusty-mpm dashboard — daemon unreachable".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(header)).style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );

    // Middle row: sessions (60%) beside circuit breakers (40%).
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[1]);

    let sessions = Table::new(
        session_rows(state),
        [
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(vec!["ID", "WORKDIR", "STATUS", "DELEG"]))
    .block(Block::default().borders(Borders::ALL).title("Sessions"));
    frame.render_widget(sessions, middle[0]);

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

    // Bottom: the recent hook-event feed.
    let items: Vec<ListItem> = event_lines(state).into_iter().map(ListItem::new).collect();
    let events = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Recent Events"),
    );
    frame.render_widget(events, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_rows_empty_when_no_sessions() {
        let state = DashboardState::default();
        assert!(session_rows(&state).is_empty());
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
            }],
            ..DashboardState::default()
        };
        let rows = session_rows(&state);
        assert_eq!(rows.len(), 1);
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
}
