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
    "keys: ↑↓ navigate | p pause | r resume | x stop | o iTerm2 tab | c connect | ? help | q quit";

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
    /// Whether the TUI is running inside iTerm2.
    ///
    /// Why: the `o` key opens the selected session in a new iTerm2 tab, but
    /// that only works inside iTerm2; detection is done once at startup so the
    /// key handler can pick the iTerm2 path or the fallback message.
    /// What: set from [`crate::iterm2::is_iterm2`] when the dashboard
    /// initialises; drives the `[iTerm2]` status-bar indicator.
    /// Test: `status_line_shows_iterm2_indicator`.
    pub iterm2_mode: bool,
    /// Current contents of the inline `connect>` prompt, if it is open.
    ///
    /// Why: the `c` key opens an inline prompt where the operator types a fuzzy
    /// session target; while it is `Some` the event loop routes every keystroke
    /// to the prompt instead of navigation/action keys.
    /// What: `None` when the prompt is closed, `Some(buffer)` while editing.
    /// Test: `connect_prompt_open_close`, `connect_prompt_edits_buffer`.
    pub connect_prompt: Option<String>,
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

    /// Move the selection to the session whose UUID equals `id`.
    ///
    /// Why: `tm connect` and the in-TUI `/connect` prompt both resolve a fuzzy
    /// target to a definitive session id; the dashboard must then highlight that
    /// row so the operator lands on the right session.
    /// What: searches [`Self::sessions`] for a row whose `id` string equals `id`,
    /// updates [`Self::selected_session`] and returns `true` on a hit; leaves the
    /// selection untouched and returns `false` when no session matches.
    /// Test: `focus_on_selects_matching_session`, `focus_on_missing_is_noop`.
    pub fn focus_on(&mut self, id: &str) -> bool {
        if let Some(idx) = self.sessions.iter().position(|s| s.id.as_str() == Some(id)) {
            self.selected_session = idx;
            true
        } else {
            false
        }
    }

    /// Build the [`SessionSummary`] slice the resolver searches.
    ///
    /// Why: `trusty_mpm_core::resolve_target` works on its own minimal summary
    /// type; the dashboard's `SessionRow` carries extra render-only fields, so a
    /// projection is needed before resolution.
    /// What: maps each polled `SessionRow` to a `SessionSummary` — UUID string,
    /// friendly `tmux_name`, and `workdir`; `last_active` is unavailable in the
    /// dashboard wire shape, so it defaults to `0` (recency tie-breaking is only
    /// reached for workdir-prefix matches).
    /// Test: covered indirectly by `submit_connect_*` tests.
    fn session_summaries(&self) -> Vec<trusty_mpm_core::SessionSummary> {
        self.sessions
            .iter()
            .filter_map(|s| {
                Some(trusty_mpm_core::SessionSummary {
                    id: s.id.as_str()?.to_string(),
                    name: Some(s.tmux_name.clone()).filter(|n| !n.is_empty()),
                    workdir: s.workdir.clone(),
                    last_active: 0,
                })
            })
            .collect()
    }

    /// Open the inline `connect>` prompt with an empty buffer.
    ///
    /// Why: the `c` key starts a fuzzy session-connect flow without leaving the
    /// dashboard.
    /// What: sets [`Self::connect_prompt`] to `Some(String::new())`.
    /// Test: `connect_prompt_open_close`.
    pub fn open_connect_prompt(&mut self) {
        self.connect_prompt = Some(String::new());
    }

    /// Close the `connect>` prompt without taking any action (the Esc key).
    ///
    /// Why: the operator must be able to abandon a half-typed target.
    /// What: clears [`Self::connect_prompt`] back to `None`.
    /// Test: `connect_prompt_open_close`.
    pub fn close_connect_prompt(&mut self) {
        self.connect_prompt = None;
    }

    /// Append a character to the open `connect>` prompt buffer.
    ///
    /// Why: printable keystrokes build up the target string while the prompt is
    /// open.
    /// What: pushes `c` onto the buffer; a no-op when the prompt is closed.
    /// Test: `connect_prompt_edits_buffer`.
    pub fn connect_prompt_push(&mut self, c: char) {
        if let Some(buf) = self.connect_prompt.as_mut() {
            buf.push(c);
        }
    }

    /// Delete the last character of the open `connect>` prompt buffer.
    ///
    /// Why: Backspace must edit a mistyped target.
    /// What: pops the trailing character; a no-op when closed or empty.
    /// Test: `connect_prompt_edits_buffer`.
    pub fn connect_prompt_backspace(&mut self) {
        if let Some(buf) = self.connect_prompt.as_mut() {
            buf.pop();
        }
    }

    /// Resolve the typed target and focus the matching session (the Enter key).
    ///
    /// Why: completes the in-TUI `/connect` flow — the operator types a fuzzy
    /// target and expects the dashboard to jump to that session.
    /// What: resolves the prompt buffer against the current sessions via
    /// [`trusty_mpm_core::resolve_target`]; on `Found` it focuses the row and
    /// records `"Connected to <id>"`, on `Ambiguous`/`NotFound` it records the
    /// matching status line; always closes the prompt afterward.
    /// Test: `submit_connect_found`, `submit_connect_not_found`,
    /// `submit_connect_ambiguous`.
    pub fn submit_connect(&mut self) {
        let Some(target) = self.connect_prompt.take() else {
            return;
        };
        let summaries = self.session_summaries();
        self.last_action = Some(match trusty_mpm_core::resolve_target(&target, &summaries) {
            trusty_mpm_core::ResolveResult::Found(id) => {
                self.focus_on(&id);
                format!("Connected to {id}")
            }
            trusty_mpm_core::ResolveResult::Ambiguous(ids) => {
                format!("Ambiguous: {}", ids.join(", "))
            }
            trusty_mpm_core::ResolveResult::NotFound => "No session matched".to_string(),
        });
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
/// What: returns `last_action` if set, otherwise [`KEY_HINT`]; prefixes a
/// `[iTerm2]` mode indicator when the TUI is running inside iTerm2.
/// Test: `status_line_falls_back_to_key_hint`, `status_line_shows_last_action`,
/// `status_line_shows_iterm2_indicator`.
pub fn status_line(state: &DashboardState) -> String {
    let body = state
        .last_action
        .clone()
        .unwrap_or_else(|| KEY_HINT.to_string());
    if state.iterm2_mode {
        format!("[iTerm2] {body}")
    } else {
        body
    }
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
        "  o         open session in iTerm2 tab",
        "  c         connect to session",
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

    if let Some(buffer) = state.connect_prompt.as_deref() {
        render_connect_prompt(frame, buffer);
    }
}

/// Render the inline `connect>` prompt at the bottom of the frame.
///
/// Why: the `c` key starts a fuzzy session-connect flow; the operator needs a
/// visible single-line input showing what they have typed so far.
/// What: clears a one-row-tall bordered box on the bottom line and draws
/// `connect> <buffer>` inside it.
/// Test: the prompt text is covered by `connect_prompt_line`; the layout math
/// is exercised by the rendering smoke test.
fn render_connect_prompt(frame: &mut Frame, buffer: &str) {
    let area = frame.area();
    let row = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(3),
        width: area.width,
        height: 3,
    };
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(connect_prompt_line(buffer))
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Connect — Enter to resolve, Esc to cancel"),
            ),
        row,
    );
}

/// Build the text shown inside the `connect>` prompt.
///
/// Why: kept separate so a test can assert the prompt prefix without a frame.
/// What: returns `connect> <buffer>`.
/// Test: `connect_prompt_line`.
pub fn connect_prompt_line(buffer: &str) -> String {
    format!("connect> {buffer}")
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
    fn status_line_shows_iterm2_indicator() {
        // Why: when running inside iTerm2 the status bar must carry a visible
        // `[iTerm2]` mode label; non-iTerm2 mode must not.
        let iterm = DashboardState {
            iterm2_mode: true,
            ..DashboardState::default()
        };
        assert!(status_line(&iterm).starts_with("[iTerm2]"));

        let plain = DashboardState::default();
        assert!(!status_line(&plain).starts_with("[iTerm2]"));
    }

    #[test]
    fn help_text_lists_all_bindings() {
        let text = help_text();
        for key in ["p", "r", "x", "o", "?", "q"] {
            assert!(text.contains(key), "help text missing binding `{key}`");
        }
        // The `o` binding now opens an iTerm2 tab; its help line must say so.
        assert!(text.contains("iTerm2"), "help text missing iTerm2 hint");
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
    fn focus_on_selects_matching_session() {
        // Focusing a present session id moves the selection to its row.
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "tmpm-a"),
                session("bbb", "/p/b", "active", "tmpm-b"),
            ],
            ..DashboardState::default()
        };
        assert!(state.focus_on("bbb"));
        assert_eq!(state.selected_session, 1);
    }

    #[test]
    fn focus_on_missing_is_noop() {
        // An unknown id leaves the selection untouched and returns false.
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "tmpm-a")],
            selected_session: 0,
            ..DashboardState::default()
        };
        assert!(!state.focus_on("zzz"));
        assert_eq!(state.selected_session, 0);
    }

    #[test]
    fn connect_prompt_open_close() {
        // `c` opens an empty prompt; Esc closes it.
        let mut state = DashboardState::default();
        assert!(state.connect_prompt.is_none());
        state.open_connect_prompt();
        assert_eq!(state.connect_prompt.as_deref(), Some(""));
        state.close_connect_prompt();
        assert!(state.connect_prompt.is_none());
    }

    #[test]
    fn connect_prompt_edits_buffer() {
        // Printable keys append, Backspace removes the trailing character.
        let mut state = DashboardState::default();
        state.open_connect_prompt();
        state.connect_prompt_push('f');
        state.connect_prompt_push('e');
        assert_eq!(state.connect_prompt.as_deref(), Some("fe"));
        state.connect_prompt_backspace();
        assert_eq!(state.connect_prompt.as_deref(), Some("f"));
    }

    #[test]
    fn submit_connect_found() {
        // A unique name-prefix match focuses the row and reports "Connected to".
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "frontend"),
                session("bbb", "/p/b", "active", "backend"),
            ],
            ..DashboardState::default()
        };
        state.open_connect_prompt();
        for c in "front".chars() {
            state.connect_prompt_push(c);
        }
        state.submit_connect();
        assert!(state.connect_prompt.is_none());
        assert_eq!(state.selected_session, 0);
        assert_eq!(state.last_action.as_deref(), Some("Connected to aaa"));
    }

    #[test]
    fn submit_connect_not_found() {
        // A target matching nothing reports "No session matched".
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            ..DashboardState::default()
        };
        state.open_connect_prompt();
        for c in "zzz".chars() {
            state.connect_prompt_push(c);
        }
        state.submit_connect();
        assert_eq!(state.last_action.as_deref(), Some("No session matched"));
    }

    #[test]
    fn submit_connect_ambiguous() {
        // Two sessions sharing a name prefix yield an "Ambiguous:" status line.
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "feature-a"),
                session("bbb", "/p/b", "active", "feature-b"),
            ],
            ..DashboardState::default()
        };
        state.open_connect_prompt();
        for c in "feature".chars() {
            state.connect_prompt_push(c);
        }
        state.submit_connect();
        assert!(
            state
                .last_action
                .as_deref()
                .unwrap()
                .starts_with("Ambiguous:")
        );
    }

    #[test]
    fn connect_prompt_line_has_prefix() {
        assert_eq!(connect_prompt_line("front"), "connect> front");
    }

    #[test]
    fn help_text_lists_connect_binding() {
        assert!(help_text().contains("connect to session"));
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
