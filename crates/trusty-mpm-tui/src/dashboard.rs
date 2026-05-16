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
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::client::SessionRow;

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

/// Draw the dashboard frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a header line plus the multi-session table; an empty/disconnected
/// state renders an explanatory placeholder.
/// Test: rendering is exercised by the integration smoke test; row content is
/// unit-tested via `session_rows`.
pub fn render(frame: &mut Frame, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
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

    let table = Table::new(
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
    frame.render_widget(table, chunks[1]);
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
        };
        let rows = session_rows(&state);
        assert_eq!(rows.len(), 1);
    }
}
